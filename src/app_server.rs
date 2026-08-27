use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::Builder;

use crate::account_store::{UsageRecord, UsageWindow, now_epoch};
use crate::config::Config;
use crate::fs::{atomic_copy, private_dir};
use crate::{Error, Result};

const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub fn query_profile(config: &Config, source_auth: &Path) -> UsageRecord {
    match query_profile_inner(config, source_auth) {
        Ok(usage) => usage,
        Err(error) => UsageRecord {
            observed_at: now_epoch(),
            error: Some(format!("quota unavailable ({})", error_kind(&error))),
            ..UsageRecord::default()
        },
    }
}

fn query_profile_inner(config: &Config, source_auth: &Path) -> Result<UsageRecord> {
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

    let mut client = SpawnedClient::start(config.codex_binary(), home.path())?;
    let usage = query(&mut client);
    let stopped = client.finish();
    match (usage, stopped) {
        (Ok(usage), Ok(())) => Ok(usage),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn error_kind(error: &Error) -> &'static str {
    match error {
        Error::Timeout => "Timeout",
        Error::Protocol(_) => "Protocol",
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

fn query(client: &mut impl RpcClient) -> Result<UsageRecord> {
    client.request(
        0,
        "initialize",
        Some(json!({"clientInfo": {
            "name": "cxa", "title": "Codex Account Switcher", "version": "1"
        }})),
    )?;
    client.send(json!({"method": "initialized"}))?;
    client.request(1, "account/read", Some(json!({"refreshToken": false})))?;
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
    let individual_window = limits
        .get("individualLimit")
        .and_then(Value::as_object)
        .map(|value| UsageWindow {
            used_percent: value
                .get("remainingPercent")
                .and_then(Value::as_f64)
                .map(|remaining| (100.0 - remaining).clamp(0.0, 100.0))
                .or_else(|| value.get("usedPercent").and_then(Value::as_f64)),
            resets_at: value.get("resetsAt").and_then(Value::as_i64),
            window_minutes: value.get("windowDurationMins").and_then(Value::as_i64),
        });
    Ok(UsageRecord {
        observed_at: now_epoch(),
        primary_window: window("primary"),
        secondary_window: window("secondary"),
        individual_window,
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

struct SpawnedClient {
    child: ChildGuard,
    stdin: ChildStdin,
    messages: Receiver<std::result::Result<Value, String>>,
}

impl SpawnedClient {
    fn start(codex_binary: &Path, home: &Path) -> Result<Self> {
        let mut command = Command::new(codex_binary);
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
        })
    }

    fn finish(&mut self) -> Result<()> {
        self.child.stop()
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
        self.messages
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
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

    #[test]
    fn parses_quota_windows() {
        let usage = parse_usage(&json!({
            "rateLimits": {
                "primary": {"usedPercent": 25.0, "resetsAt": 1234},
                "credits": {"hasCredits": true, "unlimited": false}
            }
        }))
        .unwrap();

        assert_eq!(usage.primary_window.unwrap().used_percent, Some(25.0));
        assert_eq!(usage.has_credits, Some(true));
    }
}
