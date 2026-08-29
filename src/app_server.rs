use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::Builder;

use crate::account_store::{UsageBucket, UsageRecord, UsageWindow, now_epoch};
use crate::auth::AuthDocument;
use crate::config::Config;
use crate::fs::{atomic_copy, private_dir};
use crate::{Error, Result};

const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub fn query_profile(config: &Config, source_auth: &Path) -> (UsageRecord, bool) {
    match query_profile_inner(config, source_auth, CancellationToken::default()) {
        Ok((usage, session_changed)) => (usage.unwrap_or_else(unavailable_usage), session_changed),
        Err(error) => (unavailable_usage(error), false),
    }
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

pub fn query_profile_cancellable(
    config: &Config,
    source_auth: &Path,
    cancellation: CancellationToken,
) -> Result<(UsageRecord, bool)> {
    normalize_cancellable_query(query_profile_inner(config, source_auth, cancellation))
}

fn normalize_cancellable_query(
    result: Result<(Result<UsageRecord>, bool)>,
) -> Result<(UsageRecord, bool)> {
    match result {
        Ok((Err(Error::Cancelled), _)) | Err(Error::Cancelled) => Err(Error::Cancelled),
        Ok((usage, session_changed)) => {
            Ok((usage.unwrap_or_else(unavailable_usage), session_changed))
        }
        Err(error) => Ok((unavailable_usage(error), false)),
    }
}

fn unavailable_usage(error: Error) -> UsageRecord {
    let attempted_at = now_epoch();
    UsageRecord {
        observed_at: attempted_at,
        last_attempted_at: attempted_at,
        error: Some(format!("quota unavailable ({})", error_kind(&error))),
        ..UsageRecord::default()
    }
}

pub fn require_file_credentials(config: &Config) -> Result<()> {
    config.require_no_credential_override()?;
    let mut client = SpawnedClient::start(
        config.codex_binary(),
        &config.codex_home,
        CredentialStore::Effective,
        CancellationToken::default(),
    )?;
    let checked = effective_credential_store(&mut client).and_then(|mode| {
        if mode == "file" {
            Ok(())
        } else {
            Err(Error::Message(format!(
                "cxa requires Codex's file credential store, but Codex is configured to use `{mode}`. Set `cli_auth_credentials_store = \"file\"`, run `codex login` again, then run cxa."
            )))
        }
    });
    let stopped = client.finish();
    checked?;
    stopped
}

fn query_profile_inner(
    config: &Config,
    source_auth: &Path,
    cancellation: CancellationToken,
) -> Result<(Result<UsageRecord>, bool)> {
    private_dir(&config.account_store)?;
    let original_auth = AuthDocument::read(source_auth)?;
    let home = Builder::new()
        .prefix(".quota-")
        .tempdir_in(&config.account_store)
        .map_err(|error| Error::io(&config.account_store, error))?;
    atomic_copy(source_auth, &home.path().join("auth.json"), 0o600)?;
    let source_config = config.codex_home.join("config.toml");
    if source_config.is_file() {
        atomic_copy(&source_config, &home.path().join("config.toml"), 0o600)?;
    }

    let mut client = SpawnedClient::start(
        config.codex_binary(),
        home.path(),
        CredentialStore::ForceFile,
        cancellation,
    )?;
    let usage = query(&mut client);
    client.finish()?;
    let refreshed_auth = AuthDocument::read(home.path().join("auth.json"))?;
    refreshed_auth.copy_to_same_account(source_auth)?;
    let mut session_changed = false;
    if let Ok(session) = AuthDocument::read(&config.session_auth) {
        if refreshed_auth.identity.same_account(&session.identity)
            && session.same_credentials(&original_auth)
        {
            session_changed = refreshed_auth.copy_to_same_account(&config.session_auth)?;
        }
    }
    Ok((usage, session_changed))
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
    initialize(client)?;
    client.request(1, "account/read", Some(json!({"refreshToken": false})))?;
    let result = client.request(2, "account/rateLimits/read", None)?;
    parse_usage(&result)
}

fn effective_credential_store(client: &mut impl RpcClient) -> Result<String> {
    initialize(client)?;
    let result = client.request(1, "config/read", Some(json!({"includeLayers": false})))?;
    result
        .pointer("/config/cli_auth_credentials_store")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::Protocol("config/read returned no effective cli_auth_credentials_store".into())
        })
}

fn initialize(client: &mut impl RpcClient) -> Result<()> {
    client.request(
        0,
        "initialize",
        Some(json!({"clientInfo": {
            "name": "cxa", "title": "Codex Account Switcher", "version": "1"
        }})),
    )?;
    client.send(json!({"method": "initialized"}))?;
    Ok(())
}

fn parse_usage(result: &Value) -> Result<UsageRecord> {
    let mut buckets = if let Some(limits) = result
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .filter(|limits| !limits.is_empty())
    {
        limits
            .iter()
            .filter_map(|(limit_id, value)| {
                value.as_object().map(|value| parse_bucket(limit_id, value))
            })
            .collect()
    } else {
        let limits = result
            .get("rateLimits")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::Protocol("account/rateLimits/read returned no limits".into()))?;
        vec![parse_bucket("codex", limits)]
    };
    buckets.sort_by(|left, right| left.limit_id.cmp(&right.limit_id));
    let observed_at = now_epoch();
    Ok(UsageRecord {
        observed_at,
        last_attempted_at: observed_at,
        buckets,
        error: None,
    })
}

fn parse_bucket(limit_id: &str, limits: &serde_json::Map<String, Value>) -> UsageBucket {
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
    UsageBucket {
        limit_id: limits
            .get("limitId")
            .and_then(Value::as_str)
            .unwrap_or(limit_id)
            .to_owned(),
        limit_name: limits
            .get("limitName")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        primary_window: window("primary"),
        secondary_window: window("secondary"),
        individual_window,
        reached: reached_type.is_some() || spend_control_reached,
        reached_type,
        spend_control_reached,
        plan_type: limits
            .get("planType")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }
}

struct SpawnedClient {
    child: ChildGuard,
    stdin: ChildStdin,
    messages: Receiver<std::result::Result<Value, String>>,
    cancellation: CancellationToken,
}

enum CredentialStore {
    Effective,
    ForceFile,
}

impl SpawnedClient {
    fn start(
        codex_binary: &Path,
        home: &Path,
        credential_store: CredentialStore,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        let mut command = Command::new(codex_binary);
        if matches!(credential_store, CredentialStore::ForceFile) {
            command.args(["-c", "cli_auth_credentials_store=\"file\""]);
        }
        command
            .arg("app-server")
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
            cancellation,
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
        loop {
            if self.cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            match self
                .messages
                .recv_timeout(remaining.min(Duration::from_millis(80)))
            {
                Ok(message) => return message.map_err(Error::Protocol),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Protocol("app server closed its output".into()));
                }
            }
        }
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
                "credits": {"hasCredits": false, "unlimited": false}
            }
        }))
        .unwrap();

        let bucket = &usage.buckets[0];
        assert_eq!(
            bucket.primary_window.as_ref().unwrap().used_percent,
            Some(25.0)
        );
        assert_eq!(
            bucket.primary_window.as_ref().unwrap().resets_at,
            Some(1234)
        );
        assert!(!usage.exhausted_now(now_epoch()));
        assert!(!usage.label(now_epoch()).contains("no credits"));
    }

    #[test]
    fn cancellable_queries_only_propagate_cancellation() {
        let (usage, session_changed) =
            normalize_cancellable_query(Err(Error::Protocol("temporary setup failure".into())))
                .unwrap();
        assert!(!session_changed);
        assert!(usage.error.as_deref().unwrap().contains("Protocol"));
        assert!(matches!(
            normalize_cancellable_query(Err(Error::Cancelled)),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            normalize_cancellable_query(Ok((Err(Error::Cancelled), false))),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn parses_every_named_quota_bucket() {
        let usage = parse_usage(&json!({
            "rateLimits": {"primary": {"usedPercent": 1.0}},
            "rateLimitsByLimitId": {
                "codex": {"limitId": "codex", "primary": {"usedPercent": 25.0}},
                "codex_other": {
                    "limitId": "codex_other",
                    "limitName": "Other models",
                    "primary": {"usedPercent": 100.0}
                }
            }
        }))
        .unwrap();

        assert_eq!(usage.buckets.len(), 2);
        assert_eq!(usage.buckets[1].limit_name.as_deref(), Some("Other models"));
        assert!(usage.exhausted_now(now_epoch()));
        assert!(usage.label(now_epoch()).contains("Other models EXHAUSTED"));
    }
}
