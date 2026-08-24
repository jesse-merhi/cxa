use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::Builder;
use tungstenite::client::{IntoClientRequest, client_with_config};
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

use crate::account_store::{UsageRecord, UsageWindow, now_epoch};
use crate::config::Config;
use crate::fs::{atomic_copy, private_dir};
use crate::{Error, Result};

const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

pub struct QuotaResult {
    pub usage: UsageRecord,
    pub refreshed_auth: Option<Vec<u8>>,
}

pub fn query_shared(socket_path: &Path) -> QuotaResult {
    match SocketClient::connect(socket_path).and_then(|mut client| query(&mut client, false)) {
        Ok(usage) => QuotaResult {
            usage,
            refreshed_auth: None,
        },
        Err(error) => QuotaResult {
            usage: unavailable(&error),
            refreshed_auth: None,
        },
    }
}

pub fn prepare_offline_home(config: &Config, source_auth: &Path) -> Result<tempfile::TempDir> {
    private_dir(&config.account_store)?;
    let home = Builder::new()
        .prefix(".quota-")
        .tempdir_in(&config.account_store)
        .map_err(|error| Error::io(&config.account_store, error))?;
    atomic_copy(source_auth, &home.path().join("auth.json"), 0o600)?;
    let source_config = config.codex_home.join("config.toml");
    if source_config.is_file() {
        atomic_copy(&source_config, &home.path().join("config.toml"), 0o600)?;
    }
    Ok(home)
}

pub fn query_offline(home: &Path) -> QuotaResult {
    query_spawned(home, true)
}

pub fn query_offline_read_only(home: &Path) -> QuotaResult {
    query_spawned(home, false)
}

fn query_spawned(home: &Path, refresh_token: bool) -> QuotaResult {
    let mut client = match SpawnedClient::start(home) {
        Ok(client) => client,
        Err(error) => {
            return QuotaResult {
                usage: unavailable(&error),
                refreshed_auth: None,
            };
        }
    };
    let usage = query(&mut client, refresh_token);
    let refreshed = client.finish();
    let refreshed_auth = refresh_token
        .then(|| refreshed.as_ref().ok().cloned().flatten())
        .flatten();
    let usage = match (usage, refreshed) {
        (Ok(usage), Ok(_)) => usage,
        (Err(error), _) | (Ok(_), Err(error)) => unavailable(&error),
    };
    QuotaResult {
        usage,
        refreshed_auth,
    }
}

fn unavailable(error: &Error) -> UsageRecord {
    UsageRecord {
        observed_at: now_epoch(),
        error: Some(format!("quota unavailable ({})", error_kind(error))),
        ..UsageRecord::default()
    }
}

fn error_kind(error: &Error) -> &'static str {
    match error {
        Error::Timeout => "Timeout",
        Error::Protocol(_) => "Protocol",
        Error::WebSocket(_) => "WebSocket",
        Error::Io { .. } => "Io",
        _ => "Error",
    }
}

trait RpcClient {
    fn send(&mut self, message: Value) -> Result<()>;
    fn receive(&mut self, deadline: Instant) -> Result<Value>;

    fn request(&mut self, id: i64, method: &str, params: Option<Value>) -> Result<Value> {
        let mut message = json!({"id": id, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(message)?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let response = self.receive(deadline)?;
            if response.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if response.get("error").is_some_and(|value| !value.is_null()) {
                return Err(Error::Protocol(format!("{method} failed")));
            }
            return Ok(response.get("result").cloned().unwrap_or_else(|| json!({})));
        }
    }
}

fn query(client: &mut impl RpcClient, refresh_token: bool) -> Result<UsageRecord> {
    client.request(
        0,
        "initialize",
        Some(json!({"clientInfo": {
            "name": "cxa", "title": "Codex Account Switcher", "version": "1"
        }})),
    )?;
    client.send(json!({"method": "initialized"}))?;
    client.request(
        1,
        "account/read",
        Some(json!({"refreshToken": refresh_token})),
    )?;
    let result = client.request(2, "account/rateLimits/read", None)?;
    parse_usage(&result)
}

fn parse_usage(result: &Value) -> Result<UsageRecord> {
    let limits = result
        .get("rateLimits")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Protocol("account/rateLimits/read returned no limits".into()))?;
    let window = |name: &str| {
        limits
            .get(name)
            .and_then(Value::as_object)
            .map(|value| UsageWindow {
                used_percent: value.get("usedPercent").and_then(Value::as_f64),
                resets_at: value.get("resetsAt").and_then(Value::as_i64),
                window_minutes: value.get("windowDurationMins").and_then(Value::as_i64),
            })
    };
    let credits = limits.get("credits").and_then(Value::as_object);
    let reached_type = limits
        .get("rateLimitReachedType")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let spend_control_reached = limits
        .get("spendControlReached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(UsageRecord {
        observed_at: now_epoch(),
        primary_window: window("primary"),
        secondary_window: window("secondary"),
        individual_window: window("individualLimit"),
        reached: reached_type.is_some() || spend_control_reached,
        reached_type,
        spend_control_reached,
        has_credits: credits
            .and_then(|value| value.get("hasCredits"))
            .and_then(Value::as_bool),
        unlimited: credits
            .and_then(|value| value.get("unlimited"))
            .and_then(Value::as_bool),
        balance: credits.and_then(|value| value.get("balance")).cloned(),
        plan_type: limits
            .get("planType")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        error: None,
    })
}

struct SocketClient {
    websocket: WebSocket<UnixStream>,
}

impl SocketClient {
    fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path).map_err(|error| Error::io(path, error))?;
        stream
            .set_read_timeout(Some(FRAME_TIMEOUT))
            .map_err(|error| Error::io(path, error))?;
        stream
            .set_write_timeout(Some(FRAME_TIMEOUT))
            .map_err(|error| Error::io(path, error))?;
        let mut request = "ws://localhost/rpc"
            .into_client_request()
            .map_err(|error| Error::Protocol(error.to_string()))?;
        request.headers_mut().insert(
            tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
            tungstenite::http::HeaderValue::from_static("codex-app-server"),
        );
        let websocket_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_SIZE))
            .max_frame_size(Some(MAX_MESSAGE_SIZE));
        let (websocket, _) =
            client_with_config(request, stream, Some(websocket_config)).map_err(handshake_error)?;
        Ok(Self { websocket })
    }
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

impl RpcClient for SocketClient {
    fn send(&mut self, message: Value) -> Result<()> {
        self.websocket
            .send(Message::Text(message.to_string().into()))?;
        Ok(())
    }

    fn receive(&mut self, deadline: Instant) -> Result<Value> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            self.websocket
                .get_mut()
                .set_read_timeout(Some(remaining))
                .map_err(|error| Error::io("shared app-server socket", error))?;
            match self.websocket.read()? {
                Message::Text(text) => {
                    return serde_json::from_str(&text)
                        .map_err(|error| Error::Protocol(error.to_string()));
                }
                Message::Ping(payload) => self.websocket.send(Message::Pong(payload))?,
                Message::Close(_) => {
                    return Err(Error::Protocol("app server closed the WebSocket".into()));
                }
                _ => {}
            }
        }
    }
}

struct SpawnedClient {
    child: ChildGuard,
    stdin: ChildStdin,
    messages: Receiver<std::result::Result<Value, String>>,
    auth_path: PathBuf,
}

impl SpawnedClient {
    fn start(home: &Path) -> Result<Self> {
        let auth_path = home.join("auth.json");
        let mut command = Command::new("codex");
        command
            .args(["-c", "cli_auth_credentials_store=\"file\"", "app-server"])
            .env("CODEX_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| Error::io("codex", error))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("app server has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("app server has no stdout".into()))?;
        let (sender, messages) = mpsc::sync_channel(16);
        std::thread::spawn(move || read_json_lines(stdout, sender));
        Ok(Self {
            child: ChildGuard(Some(child)),
            stdin,
            messages,
            auth_path,
        })
    }

    fn finish(&mut self) -> Result<Option<Vec<u8>>> {
        self.child.stop()?;
        match fs::read(&self.auth_path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::io(&self.auth_path, error)),
        }
    }
}

impl RpcClient for SpawnedClient {
    fn send(&mut self, message: Value) -> Result<()> {
        let encoded =
            serde_json::to_vec(&message).map_err(|error| Error::Protocol(error.to_string()))?;
        self.stdin
            .write_all(&encoded)
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|error| Error::io("codex app-server stdin", error))
    }

    fn receive(&mut self, deadline: Instant) -> Result<Value> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.messages
            .recv_timeout(remaining)
            .map_err(|_| Error::Timeout)?
            .map_err(Error::Protocol)
    }
}

fn read_json_lines(
    mut stdout: impl Read,
    sender: mpsc::SyncSender<std::result::Result<Value, String>>,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => return,
            Ok(length) => {
                buffer.extend_from_slice(&chunk[..length]);
                while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=newline).collect();
                    if line.len() > MAX_MESSAGE_SIZE {
                        let _ = sender.send(Err("oversized app-server response line".into()));
                        return;
                    }
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    let value = serde_json::from_slice(&line).map_err(|error| error.to_string());
                    if sender.send(value).is_err() {
                        return;
                    }
                }
                if buffer.len() > MAX_MESSAGE_SIZE {
                    let _ = sender.send(Err("oversized app-server response line".into()));
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error.to_string()));
                return;
            }
        }
    }
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn stop(&mut self) -> Result<()> {
        let Some(mut child) = self.0.take() else {
            return Ok(());
        };
        let process_group = -(child.id() as i32);
        unsafe {
            libc::kill(process_group, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if child
                .try_wait()
                .map_err(|error| Error::io("codex", error))?
                .is_some()
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
        child.wait().map_err(|error| Error::io("codex", error))?;
        Ok(())
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
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
    fn parses_all_quota_windows_and_exhaustion() {
        let record = parse_usage(&json!({"rateLimits": {
            "primary": {"usedPercent": 25, "resetsAt": 4_102_444_800_i64},
            "secondary": {"usedPercent": 75, "resetsAt": 4_102_448_400_i64},
            "individualLimit": {"usedPercent": 100, "resetsAt": 4_102_452_000_i64},
            "credits": {"hasCredits": false, "unlimited": false, "balance": "0"},
            "planType": "team",
            "rateLimitReachedType": "workspace_member_usage_limit_reached",
            "spendControlReached": true
        }}))
        .unwrap();
        assert_eq!(record.primary_window.unwrap().used_percent, Some(25.0));
        assert_eq!(record.secondary_window.unwrap().used_percent, Some(75.0));
        assert_eq!(record.individual_window.unwrap().used_percent, Some(100.0));
        assert!(record.reached);
        assert!(record.spend_control_reached);
    }

    #[test]
    fn queries_quota_over_the_shared_websocket() {
        let directory = tempdir().unwrap();
        let socket = directory.path().join("app-server.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut websocket = tungstenite::accept_hdr(stream, select_codex_subprotocol).unwrap();
            let initialize: Value =
                serde_json::from_str(&websocket.read().unwrap().into_text().unwrap()).unwrap();
            assert_eq!(initialize["method"], "initialize");
            websocket
                .send(Message::Text(
                    json!({"id":0,"result":{"serverInfo":{"name":"fake"}}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            let initialized: Value =
                serde_json::from_str(&websocket.read().unwrap().into_text().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");
            let account: Value =
                serde_json::from_str(&websocket.read().unwrap().into_text().unwrap()).unwrap();
            assert_eq!(account["method"], "account/read");
            assert_eq!(account["params"]["refreshToken"], false);
            websocket
                .send(Message::Text(
                    json!({"id":1,"result":{"account":{"type":"chatgpt"}}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            let quota: Value =
                serde_json::from_str(&websocket.read().unwrap().into_text().unwrap()).unwrap();
            assert_eq!(quota["method"], "account/rateLimits/read");
            websocket
                .send(Message::Text(
                    json!({"id":2,"result":{"rateLimits":{"primary":{"usedPercent":31}}}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
        });

        let result = query_shared(&socket);
        assert!(result.usage.succeeded(), "{:?}", result.usage.error);
        assert_eq!(
            result.usage.primary_window.unwrap().used_percent,
            Some(31.0)
        );
        server.join().unwrap();
    }
}
