use std::collections::HashSet;
use std::env;
use std::io::{Read, Write};
use std::os::fd::{AsFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use serde_json::{Value, json};
use tungstenite::client::{IntoClientRequest, client_with_config};
use tungstenite::handshake::HandshakeError;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket, accept_hdr_with_config};

use crate::fs::DeadlineUnixStream;
use crate::{Error, Result};

pub const MAX_CLIENTS: usize = 16;
pub const MAX_PENDING_REQUESTS: usize = 16;
pub const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
const MAX_DRAIN_MESSAGES: usize = 1;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(300);
const CODEX_SUBPROTOCOL: &str = "codex-app-server";

#[derive(Debug)]
enum FrameReadState {
    Header {
        bytes: [u8; 14],
        filled: usize,
        target: usize,
    },
    Payload {
        remaining: u64,
        fin: bool,
        opcode: u8,
    },
}

impl FrameReadState {
    fn header() -> Self {
        Self::Header {
            bytes: [0; 14],
            filled: 0,
            target: 2,
        }
    }

    fn is_partial(&self) -> bool {
        !matches!(self, Self::Header { filled: 0, .. })
    }
}

#[derive(Debug)]
struct FramedUnixStream {
    inner: DeadlineUnixStream,
    frame: Option<FrameReadState>,
    fragmented_message: bool,
}

impl FramedUnixStream {
    fn handshake(inner: UnixStream, deadline: Instant) -> Self {
        Self {
            inner: DeadlineUnixStream::new(inner, deadline),
            frame: None,
            fragmented_message: false,
        }
    }

    #[cfg(test)]
    fn framed(inner: UnixStream) -> Self {
        Self {
            inner: DeadlineUnixStream::without_deadline(inner),
            frame: Some(FrameReadState::header()),
            fragmented_message: false,
        }
    }

    fn start_frames(&mut self) {
        self.frame = Some(FrameReadState::header());
        self.fragmented_message = false;
        self.inner.clear_deadline();
    }

    fn has_partial_frame(&self) -> bool {
        self.fragmented_message || self.frame.as_ref().is_some_and(FrameReadState::is_partial)
    }

    fn complete_frame(&mut self, fin: bool, opcode: u8) {
        match opcode {
            0x0 if fin => self.fragmented_message = false,
            0x1 | 0x2 => self.fragmented_message = !fin,
            _ => {}
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }
}

impl Read for FramedUnixStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.frame.is_none() {
            return self.inner.read(buffer);
        }
        let Some(state) = self.frame.as_mut() else {
            unreachable!();
        };
        let mut completed = None;
        let count = match state {
            FrameReadState::Header {
                bytes,
                filled,
                target,
            } => {
                let limit = (*target - *filled).min(buffer.len());
                let count = self.inner.read(&mut buffer[..limit])?;
                bytes[*filled..*filled + count].copy_from_slice(&buffer[..count]);
                *filled += count;
                if *filled == 2 && *target == 2 {
                    let extended = match bytes[1] & 0x7f {
                        126 => 2,
                        127 => 8,
                        _ => 0,
                    };
                    let mask = usize::from(bytes[1] & 0x80 != 0) * 4;
                    *target = 2 + extended + mask;
                }
                if *filled == *target {
                    let fin = bytes[0] & 0x80 != 0;
                    let opcode = bytes[0] & 0x0f;
                    let payload = match bytes[1] & 0x7f {
                        value @ 0..=125 => u64::from(value),
                        126 => u64::from(u16::from_be_bytes([bytes[2], bytes[3]])),
                        127 => u64::from_be_bytes(bytes[2..10].try_into().expect("frame header")),
                        _ => unreachable!(),
                    };
                    *state = if payload == 0 {
                        completed = Some((fin, opcode));
                        FrameReadState::header()
                    } else {
                        FrameReadState::Payload {
                            remaining: payload,
                            fin,
                            opcode,
                        }
                    };
                }
                count
            }
            FrameReadState::Payload {
                remaining,
                fin,
                opcode,
            } => {
                let limit = (*remaining).min(buffer.len() as u64) as usize;
                let count = self.inner.read(&mut buffer[..limit])?;
                *remaining -= count as u64;
                if *remaining == 0 {
                    completed = Some((*fin, *opcode));
                    *state = FrameReadState::header();
                }
                count
            }
        };
        if let Some((fin, opcode)) = completed {
            self.complete_frame(fin, opcode);
        }
        Ok(count)
    }
}

impl Write for FramedUnixStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl AsFd for FramedUnixStream {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.inner.as_fd()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ClientAction {
    Forward(String),
    Reply(String),
}

pub fn client_action(payload: &str, pending: &mut HashSet<String>) -> ClientAction {
    let Ok(message) = serde_json::from_str::<Value>(payload) else {
        return reply_error(Value::Null, -32700, "invalid JSON");
    };
    let Some(object) = message.as_object() else {
        return reply_error(Value::Null, -32600, "invalid request");
    };
    let method = object.get("method").and_then(Value::as_str);
    if method == Some("initialized") && !object.contains_key("id") {
        return ClientAction::Forward(json!({"method": "initialized"}).to_string());
    }
    let Some(id) = object.get("id").cloned() else {
        return reply_error(Value::Null, -32600, "request id required");
    };
    let Some(method) = method else {
        return reply_error(id, -32600, "request method required");
    };
    if !matches!(
        method,
        "initialize" | "account/read" | "account/rateLimits/read"
    ) {
        return reply_error(id, -32601, "method not available through quota bridge");
    }
    let key = id.to_string();
    if pending.contains(&key) {
        return reply_error(id, -32600, "duplicate pending request id");
    }
    if pending.len() >= MAX_PENDING_REQUESTS {
        return reply_error(id, -32000, "too many pending quota requests");
    }
    pending.insert(key);
    let forwarded = match method {
        "initialize" => json!({
            "id": id,
            "method": method,
            "params": {"clientInfo": {
                "name": "codex-quota-proxy",
                "title": "Codex Quota Proxy",
                "version": "1"
            }}
        }),
        "account/read" => json!({
            "id": id,
            "method": method,
            "params": {"refreshToken": true}
        }),
        _ => json!({"id": id, "method": method}),
    };
    ClientAction::Forward(forwarded.to_string())
}

fn reply_error(id: Value, code: i64, message: &str) -> ClientAction {
    ClientAction::Reply(
        json!({
            "id": id,
            "error": {"code": code, "message": message}
        })
        .to_string(),
    )
}

pub fn server_message_allowed(payload: &str, pending: &mut HashSet<String>) -> bool {
    let Ok(message) = serde_json::from_str::<Value>(payload) else {
        return false;
    };
    let Some(object) = message.as_object() else {
        return false;
    };
    if object.contains_key("id") && !object.contains_key("method") {
        let key = object["id"].to_string();
        return pending.remove(&key);
    }
    object.get("method").and_then(Value::as_str) == Some("account/rateLimits/updated")
}

pub fn run(upstream_path: &Path) -> Result<()> {
    let listen_fds = env::var("LISTEN_FDS")
        .ok()
        .and_then(|value| value.parse().ok());
    let listen_pid = env::var("LISTEN_PID")
        .ok()
        .and_then(|value| value.parse().ok());
    if listen_fds != Some(1) || listen_pid != Some(std::process::id()) {
        return Err(Error::Message(
            "codex-quota-proxy requires one systemd-activated socket".into(),
        ));
    }
    let listener = unsafe { UnixListener::from_raw_fd(3) };
    serve(listener, upstream_path)
}

pub fn serve(listener: UnixListener, upstream_path: &Path) -> Result<()> {
    let clients = Arc::new(AtomicUsize::new(0));
    for accepted in listener.incoming() {
        let client = accepted.map_err(|error| Error::io("systemd quota socket", error))?;
        if clients
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_CLIENTS).then_some(count + 1)
            })
            .is_err()
        {
            continue;
        }
        let upstream_path = upstream_path.to_owned();
        let clients = Arc::clone(&clients);
        std::thread::spawn(move || {
            if let Err(error) = proxy_connection(client, &upstream_path) {
                eprintln!("codex-quota-proxy: {error}");
            }
            clients.fetch_sub(1, Ordering::AcqRel);
        });
    }
    Ok(())
}

pub fn proxy_connection(client: UnixStream, upstream_path: &Path) -> Result<()> {
    client
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| Error::io("quota client", error))?;
    client
        .set_write_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| Error::io("quota client", error))?;
    let websocket_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_SIZE))
        .max_frame_size(Some(MAX_MESSAGE_SIZE));
    let mut downstream = accept_hdr_with_config(
        FramedUnixStream::handshake(client, Instant::now() + HANDSHAKE_TIMEOUT),
        select_codex_subprotocol,
        Some(websocket_config),
    )
    .map_err(handshake_error)?;
    downstream.get_mut().start_frames();
    downstream
        .get_mut()
        .set_read_timeout(Some(FRAME_TIMEOUT))
        .map_err(|error| Error::io("quota client", error))?;
    downstream
        .get_mut()
        .set_write_timeout(Some(FRAME_TIMEOUT))
        .map_err(|error| Error::io("quota client", error))?;

    let upstream_stream =
        UnixStream::connect(upstream_path).map_err(|error| Error::io(upstream_path, error))?;
    upstream_stream
        .set_read_timeout(Some(FRAME_TIMEOUT))
        .map_err(|error| Error::io(upstream_path, error))?;
    upstream_stream
        .set_write_timeout(Some(FRAME_TIMEOUT))
        .map_err(|error| Error::io(upstream_path, error))?;
    let request = "ws://localhost/rpc"
        .into_client_request()
        .map_err(|error| Error::Protocol(error.to_string()))?;
    let (mut upstream, _) = client_with_config(
        request,
        FramedUnixStream::handshake(upstream_stream, Instant::now() + HANDSHAKE_TIMEOUT),
        Some(websocket_config),
    )
    .map_err(handshake_error)?;
    upstream.get_mut().start_frames();
    bridge(&mut downstream, &mut upstream)
}

#[allow(clippy::result_large_err)]
fn select_codex_subprotocol(
    request: &Request,
    mut response: Response,
) -> std::result::Result<Response, ErrorResponse> {
    let protocols = request
        .headers()
        .get_all(tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL);
    if protocols.iter().next().is_none() {
        return Ok(response);
    }
    let requested = request
        .headers()
        .get_all(tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|protocol| protocol.trim() == CODEX_SUBPROTOCOL);
    if !requested {
        let mut error = ErrorResponse::new(Some(format!(
            "WebSocket subprotocol {CODEX_SUBPROTOCOL} is required"
        )));
        *error.status_mut() = tungstenite::http::StatusCode::BAD_REQUEST;
        return Err(error);
    }
    response.headers_mut().insert(
        tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
        tungstenite::http::HeaderValue::from_static(CODEX_SUBPROTOCOL),
    );
    Ok(response)
}

fn handshake_error<Role: tungstenite::handshake::HandshakeRole>(
    error: HandshakeError<Role>,
) -> Error {
    match error {
        HandshakeError::Failure(error) => Error::WebSocket(error),
        HandshakeError::Interrupted(_) => {
            Error::Protocol("blocking WebSocket handshake was interrupted".into())
        }
    }
}

fn bridge(
    downstream: &mut WebSocket<FramedUnixStream>,
    upstream: &mut WebSocket<FramedUnixStream>,
) -> Result<()> {
    let mut pending = HashSet::new();
    let mut client_partial_since = None;
    let mut server_partial_since = None;
    let mut last_activity = Instant::now();
    loop {
        let now = Instant::now();
        if now.duration_since(last_activity) >= IDLE_CONNECTION_TIMEOUT {
            return Err(Error::Timeout);
        }
        if [client_partial_since, server_partial_since]
            .into_iter()
            .flatten()
            .any(|started| now.duration_since(started) >= FRAME_TIMEOUT)
        {
            return Err(Error::Timeout);
        }
        let timeout = connection_timeout(
            client_partial_since,
            server_partial_since,
            last_activity,
            now,
        );
        let (client_ready, server_ready) = {
            let mut descriptors = [
                PollFd::new(downstream.get_ref().as_fd(), PollFlags::POLLIN),
                PollFd::new(upstream.get_ref().as_fd(), PollFlags::POLLIN),
            ];
            if poll(
                &mut descriptors,
                PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX),
            )? == 0
            {
                return Err(Error::Timeout);
            }
            let readable = |descriptor: &PollFd<'_>| {
                descriptor.revents().is_some_and(|events| {
                    events.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR)
                })
            };
            (readable(&descriptors[0]), readable(&descriptors[1]))
        };

        if client_ready {
            let drained = drain_messages(downstream)?;
            client_partial_since = update_partial_deadline(client_partial_since, &drained);
            if !drained.messages.is_empty() || drained.incomplete {
                last_activity = Instant::now();
            }
            if drained.closed {
                let _ = upstream.close(None);
                return Ok(());
            }
            for message in drained.messages {
                if !handle_client_message(message, downstream, upstream, &mut pending)? {
                    return Ok(());
                }
            }
        }
        if server_ready {
            let drained = drain_messages(upstream)?;
            server_partial_since = update_partial_deadline(server_partial_since, &drained);
            if !drained.messages.is_empty() || drained.incomplete {
                last_activity = Instant::now();
            }
            if drained.closed {
                let _ = downstream.close(None);
                return Ok(());
            }
            for message in drained.messages {
                if !handle_server_message(message, downstream, upstream, &mut pending)? {
                    return Ok(());
                }
            }
        }
    }
}

fn connection_timeout(
    client_partial_since: Option<Instant>,
    server_partial_since: Option<Instant>,
    last_activity: Instant,
    now: Instant,
) -> Duration {
    [client_partial_since, server_partial_since]
        .into_iter()
        .flatten()
        .map(|started| {
            FRAME_TIMEOUT
                .saturating_sub(now.duration_since(started))
                .max(Duration::from_millis(1))
        })
        .chain(std::iter::once(
            IDLE_CONNECTION_TIMEOUT
                .saturating_sub(now.duration_since(last_activity))
                .max(Duration::from_millis(1)),
        ))
        .min()
        .expect("idle timeout is always present")
}

struct DrainedMessages {
    messages: Vec<Message>,
    incomplete: bool,
    closed: bool,
}

fn drain_messages(websocket: &mut WebSocket<FramedUnixStream>) -> Result<DrainedMessages> {
    websocket
        .get_mut()
        .set_nonblocking(true)
        .map_err(|error| Error::io("quota WebSocket", error))?;
    let mut messages = Vec::new();
    let mut incomplete = false;
    let mut closed = false;
    let read_result = loop {
        match websocket.read() {
            Ok(message) => {
                let is_close = matches!(message, Message::Close(_));
                messages.push(message);
                if is_close || messages.len() >= MAX_DRAIN_MESSAGES {
                    break Ok(());
                }
            }
            Err(tungstenite::Error::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                incomplete = websocket.get_ref().has_partial_frame();
                break Ok(());
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                closed = true;
                break Ok(());
            }
            Err(tungstenite::Error::Protocol(
                tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
            )) => {
                closed = true;
                break Ok(());
            }
            Err(error) => break Err(Error::WebSocket(error)),
        }
    };
    let blocking_result = websocket
        .get_mut()
        .set_nonblocking(false)
        .map_err(|error| Error::io("quota WebSocket", error));
    read_result?;
    blocking_result?;
    Ok(DrainedMessages {
        messages,
        incomplete,
        closed,
    })
}

fn update_partial_deadline(
    previous: Option<Instant>,
    drained: &DrainedMessages,
) -> Option<Instant> {
    if drained.incomplete {
        previous.or_else(|| Some(Instant::now()))
    } else if !drained.messages.is_empty() {
        None
    } else {
        previous
    }
}

fn handle_client_message(
    message: Message,
    downstream: &mut WebSocket<FramedUnixStream>,
    upstream: &mut WebSocket<FramedUnixStream>,
    pending: &mut HashSet<String>,
) -> Result<bool> {
    match message {
        Message::Text(text) => match client_action(&text, pending) {
            ClientAction::Forward(message) => upstream.send(Message::Text(message.into()))?,
            ClientAction::Reply(message) => downstream.send(Message::Text(message.into()))?,
        },
        Message::Ping(payload) => downstream.send(Message::Pong(payload))?,
        Message::Pong(_) => {}
        Message::Close(frame) => {
            upstream.close(frame)?;
            return Ok(false);
        }
        _ => {
            return Err(Error::Protocol(
                "unsupported client WebSocket message".into(),
            ));
        }
    }
    Ok(true)
}

fn handle_server_message(
    message: Message,
    downstream: &mut WebSocket<FramedUnixStream>,
    upstream: &mut WebSocket<FramedUnixStream>,
    pending: &mut HashSet<String>,
) -> Result<bool> {
    match message {
        Message::Text(text) => {
            if server_message_allowed(&text, pending) {
                downstream.send(Message::Text(text))?;
            }
        }
        Message::Ping(payload) => upstream.send(Message::Pong(payload))?,
        Message::Pong(_) => {}
        Message::Close(frame) => {
            downstream.close(frame)?;
            return Ok(false);
        }
        _ => {
            return Err(Error::Protocol(
                "unsupported server WebSocket message".into(),
            ));
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;
    use tempfile::tempdir;

    #[test]
    fn forwards_only_account_and_quota_methods() {
        let mut pending = HashSet::new();
        for (id, method) in ["initialize", "account/read", "account/rateLimits/read"]
            .into_iter()
            .enumerate()
        {
            let action = client_action(
                &json!({"id": id, "method": method}).to_string(),
                &mut pending,
            );
            assert!(matches!(action, ClientAction::Forward(_)));
        }
    }

    #[test]
    fn reloads_auth_and_denies_thread_start() {
        let mut pending = HashSet::new();
        let action = client_action(
            &json!({"id": 1, "method": "account/read", "params": {"refreshToken": true}})
                .to_string(),
            &mut pending,
        );
        let ClientAction::Forward(forwarded) = action else {
            panic!("account/read was not forwarded");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&forwarded).unwrap()["params"]["refreshToken"],
            true
        );

        let action = client_action(
            &json!({"id": 2, "method": "thread/start"}).to_string(),
            &mut pending,
        );
        let ClientAction::Reply(reply) = action else {
            panic!("thread/start was not denied");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap()["error"]["code"],
            -32601
        );
    }

    #[test]
    fn accepts_only_matching_responses_and_rate_limit_notifications() {
        let mut pending = HashSet::from(["1".to_owned()]);
        assert!(!server_message_allowed(
            r#"{"id":2,"result":{}}"#,
            &mut pending
        ));
        assert!(!server_message_allowed(
            r#"{"method":"thread/started","params":{}}"#,
            &mut pending
        ));
        assert!(server_message_allowed(
            r#"{"method":"account/rateLimits/updated","params":{}}"#,
            &mut pending
        ));
        assert!(server_message_allowed(
            r#"{"id":1,"result":{}}"#,
            &mut pending
        ));
        assert!(pending.is_empty());
    }

    #[test]
    fn accepts_missing_and_selects_the_codex_subprotocol_when_offered() {
        let missing = Request::new(());
        let accepted = select_codex_subprotocol(&missing, Response::new(())).unwrap();
        assert!(
            accepted
                .headers()
                .get(tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL)
                .is_none()
        );

        let mut offered = Request::new(());
        offered.headers_mut().insert(
            tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
            tungstenite::http::HeaderValue::from_static("other, codex-app-server"),
        );
        let accepted = select_codex_subprotocol(&offered, Response::new(())).unwrap();
        assert_eq!(
            accepted
                .headers()
                .get(tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL),
            Some(&tungstenite::http::HeaderValue::from_static(
                "codex-app-server"
            ))
        );

        let mut unsupported = Request::new(());
        unsupported.headers_mut().insert(
            tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
            tungstenite::http::HeaderValue::from_static("other"),
        );
        assert!(select_codex_subprotocol(&unsupported, Response::new(())).is_err());
    }

    #[test]
    fn bridges_multiple_requests_notifications_and_local_denials() {
        let directory = tempdir().unwrap();
        let upstream_path = directory.path().join("upstream.sock");
        let listener = UnixListener::bind(&upstream_path).unwrap();
        let (observed_sender, observed_receiver) = mpsc::channel();
        let upstream_thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(250)))
                .unwrap();
            let mut websocket = tungstenite::accept(stream).unwrap();
            let first = websocket.read().unwrap().into_text().unwrap();
            let second = websocket.read().unwrap().into_text().unwrap();
            let first: Value = serde_json::from_str(&first).unwrap();
            let second: Value = serde_json::from_str(&second).unwrap();
            assert_eq!(first["id"], 1);
            assert_eq!(second["id"], 2);
            websocket
                .send(Message::Text(
                    json!({"method":"account/rateLimits/updated","params":{"source":"push"}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            websocket
                .send(Message::Text(
                    json!({"id":2,"result":{"account":{"type":"chatgpt"}}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            websocket
                .send(Message::Text(
                    json!({"id":1,"result":{"serverInfo":{"name":"fake"}}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            let denied_was_forwarded = websocket
                .read()
                .ok()
                .and_then(|message| message.into_text().ok())
                .is_some_and(|text| text.contains("thread/start"));
            observed_sender.send(denied_was_forwarded).unwrap();
        });

        let (client_stream, proxy_stream) = UnixStream::pair().unwrap();
        let proxy_path = upstream_path.clone();
        let proxy_thread = std::thread::spawn(move || {
            proxy_connection(proxy_stream, &proxy_path).unwrap();
        });
        client_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request = "ws://localhost/quota".into_client_request().unwrap();
        let (mut client, response) = tungstenite::client(request, client_stream).unwrap();
        assert!(
            response
                .headers()
                .get(tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL)
                .is_none()
        );
        client
            .send(Message::Text(
                json!({"id":1,"method":"initialize"}).to_string().into(),
            ))
            .unwrap();
        client
            .send(Message::Text(
                json!({"id":2,"method":"account/read","params":{"refreshToken":true}})
                    .to_string()
                    .into(),
            ))
            .unwrap();

        let mut received = Vec::new();
        for _ in 0..3 {
            received.push(
                serde_json::from_str::<Value>(&client.read().unwrap().into_text().unwrap())
                    .unwrap(),
            );
        }
        assert!(received.iter().any(|message| {
            message["method"] == "account/rateLimits/updated"
                && message["params"]["source"] == "push"
        }));
        assert!(received.iter().any(|message| message["id"] == 1));
        assert!(received.iter().any(|message| message["id"] == 2));

        client
            .send(Message::Text(
                json!({"id":3,"method":"thread/start"}).to_string().into(),
            ))
            .unwrap();
        let denial: Value =
            serde_json::from_str(&client.read().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(denial["id"], 3);
        assert_eq!(denial["error"]["code"], -32601);
        assert!(
            !observed_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
        );
        drop(client);
        upstream_thread.join().unwrap();
        proxy_thread.join().unwrap();
    }

    #[test]
    fn each_nonblocking_drain_has_a_message_bound() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let (start_sender, start_receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut websocket = tungstenite::accept(server_stream).unwrap();
            start_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
            for id in 0..(MAX_DRAIN_MESSAGES + 5) {
                websocket
                    .send(Message::Text(json!({"id": id}).to_string().into()))
                    .unwrap();
            }
            ready_sender.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let request = "ws://localhost/quota".into_client_request().unwrap();
        let (mut client, _) = tungstenite::client(
            request,
            FramedUnixStream::handshake(client_stream, Instant::now() + HANDSHAKE_TIMEOUT),
        )
        .unwrap();
        client.get_mut().start_frames();
        start_sender.send(()).unwrap();
        ready_receiver.recv_timeout(Duration::from_secs(2)).unwrap();

        let drained = drain_messages(&mut client).unwrap();

        assert_eq!(drained.messages.len(), MAX_DRAIN_MESSAGES);
        let socket_still_readable = {
            let mut descriptor = [PollFd::new(client.get_ref().as_fd(), PollFlags::POLLIN)];
            poll(&mut descriptor, PollTimeout::ZERO).unwrap() > 0
        };
        assert!(
            socket_still_readable,
            "the bridge must not hide queued frames inside Tungstenite before polling again"
        );
        server.join().unwrap();
    }

    #[test]
    fn handshake_reads_share_one_absolute_deadline() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_millis(70);
        let mut reader = FramedUnixStream::handshake(reader, deadline);
        let writer = std::thread::spawn(move || {
            for byte in b"slow handshake" {
                std::thread::sleep(Duration::from_millis(25));
                if writer.write_all(&[*byte]).is_err() {
                    break;
                }
            }
        });
        let mut buffer = [0; 14];

        let error = reader.read_exact(&mut buffer).unwrap_err();

        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(Instant::now() < deadline + Duration::from_millis(100));
        drop(reader);
        writer.join().unwrap();
    }

    #[test]
    fn a_partial_frame_after_a_message_starts_the_timeout() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let mut client = WebSocket::from_raw_socket(
            FramedUnixStream::framed(client_stream),
            tungstenite::protocol::Role::Client,
            None,
        );
        let mut server = WebSocket::from_raw_socket(
            FramedUnixStream::framed(server_stream),
            tungstenite::protocol::Role::Server,
            None,
        );
        client.send(Message::Text("complete".into())).unwrap();
        client
            .get_mut()
            .write_all(&[0x81, 0xff, 0, 0, 0, 0, 0, 0x40, 0, 0, 1, 2, 3, 4])
            .unwrap();

        let drained = drain_messages(&mut server).unwrap();

        assert_eq!(drained.messages.len(), 1);
        assert!(!drained.incomplete);
        let partial = drain_messages(&mut server).unwrap();
        assert!(partial.messages.is_empty());
        assert!(partial.incomplete);
        assert!(update_partial_deadline(None, &partial).is_some());
    }

    #[test]
    fn a_complete_non_final_frame_starts_the_timeout() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let mut server = WebSocket::from_raw_socket(
            FramedUnixStream::framed(server_stream),
            tungstenite::protocol::Role::Server,
            None,
        );
        let mask = [1_u8, 2, 3, 4];
        let payload = [b'{' ^ mask[0], b'}' ^ mask[1]];
        (&client_stream)
            .write_all(&[
                0x01, 0x82, mask[0], mask[1], mask[2], mask[3], payload[0], payload[1],
            ])
            .unwrap();

        let drained = drain_messages(&mut server).unwrap();

        assert!(drained.messages.is_empty());
        assert!(drained.incomplete);
        assert!(update_partial_deadline(None, &drained).is_some());
    }

    #[test]
    fn complete_idle_connections_have_a_poll_deadline() {
        let now = Instant::now();
        assert_eq!(
            connection_timeout(None, None, now, now),
            IDLE_CONNECTION_TIMEOUT
        );
        assert_eq!(connection_timeout(Some(now), None, now, now), FRAME_TIMEOUT);
    }
}
