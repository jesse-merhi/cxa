use std::collections::HashSet;
use std::env;
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
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket, accept_with_config};

use crate::{Error, Result};

pub const MAX_CLIENTS: usize = 16;
pub const MAX_PENDING_REQUESTS: usize = 16;
pub const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

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
            "params": {"refreshToken": false}
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
    let mut downstream =
        accept_with_config(client, Some(websocket_config)).map_err(handshake_error)?;
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
    let mut request = "ws://localhost/rpc"
        .into_client_request()
        .map_err(|error| Error::Protocol(error.to_string()))?;
    request.headers_mut().insert(
        tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
        tungstenite::http::HeaderValue::from_static("codex-app-server"),
    );
    let (mut upstream, _) = client_with_config(request, upstream_stream, Some(websocket_config))
        .map_err(handshake_error)?;
    bridge(&mut downstream, &mut upstream)
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
    downstream: &mut WebSocket<UnixStream>,
    upstream: &mut WebSocket<UnixStream>,
) -> Result<()> {
    let mut pending = HashSet::new();
    let mut client_partial_since = None;
    let mut server_partial_since = None;
    loop {
        let now = Instant::now();
        if [client_partial_since, server_partial_since]
            .into_iter()
            .flatten()
            .any(|started| now.duration_since(started) >= FRAME_TIMEOUT)
        {
            return Err(Error::Timeout);
        }
        let timeout = [client_partial_since, server_partial_since]
            .into_iter()
            .flatten()
            .map(|started| FRAME_TIMEOUT.saturating_sub(now.duration_since(started)))
            .min()
            .unwrap_or(IDLE_TIMEOUT)
            .max(Duration::from_millis(1));
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

struct DrainedMessages {
    messages: Vec<Message>,
    incomplete: bool,
    closed: bool,
}

fn drain_messages(websocket: &mut WebSocket<UnixStream>) -> Result<DrainedMessages> {
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
                if is_close {
                    break Ok(());
                }
            }
            Err(tungstenite::Error::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                incomplete = messages.is_empty();
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
    if !drained.messages.is_empty() {
        None
    } else if drained.incomplete {
        previous.or_else(|| Some(Instant::now()))
    } else {
        previous
    }
}

fn handle_client_message(
    message: Message,
    downstream: &mut WebSocket<UnixStream>,
    upstream: &mut WebSocket<UnixStream>,
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
    downstream: &mut WebSocket<UnixStream>,
    upstream: &mut WebSocket<UnixStream>,
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
    use std::sync::mpsc;
    use tempfile::tempdir;

    #[allow(clippy::result_large_err)]
    fn select_codex_subprotocol(
        _: &tungstenite::handshake::server::Request,
        mut response: tungstenite::handshake::server::Response,
    ) -> std::result::Result<
        tungstenite::handshake::server::Response,
        tungstenite::handshake::server::ErrorResponse,
    > {
        response.headers_mut().insert(
            tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
            tungstenite::http::HeaderValue::from_static("codex-app-server"),
        );
        Ok(response)
    }

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
    fn disables_refresh_and_denies_thread_start() {
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
            false
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
            let mut websocket = tungstenite::accept_hdr(stream, select_codex_subprotocol).unwrap();
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
        let (mut client, _) = tungstenite::client(request, client_stream).unwrap();
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
}
