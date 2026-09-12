use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Case {
    _root: TempDir,
    home: PathBuf,
    codex_home: PathBuf,
    codex: PathBuf,
    store: PathBuf,
}

struct PtyChild {
    child: Child,
    master: File,
    _slave: File,
    original_mode: libc::termios,
    output: Vec<u8>,
}

impl PtyChild {
    fn spawn(mut command: Command) -> Self {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let mut original_mode = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave_fd, &mut original_mode) }, 0);

        command
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self {
            child,
            master,
            _slave: slave,
            original_mode,
            output: Vec::new(),
        }
    }

    fn wait_for_output(&mut self, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            self.read_available(Duration::from_millis(100));
            if self
                .output
                .windows(expected.len())
                .any(|window| window == expected)
            {
                return;
            }
        }
        panic!(
            "PTY output never contained {:?}: {}",
            String::from_utf8_lossy(expected),
            String::from_utf8_lossy(&self.output)
        );
    }

    fn send(&mut self, input: &[u8]) {
        self.master.write_all(input).unwrap();
        self.master.flush().unwrap();
    }

    fn signal(&self, signal: libc::c_int) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, signal) },
            0
        );
    }

    fn wait_success(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.read_available(Duration::from_millis(100));
                assert!(
                    status.success(),
                    "PTY child failed with {status}: {}",
                    String::from_utf8_lossy(&self.output)
                );
                return;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "PTY child did not exit: {}",
                    String::from_utf8_lossy(&self.output)
                );
            }
            self.read_available(Duration::from_millis(50));
        }
    }

    fn assert_terminal_restored(&self) {
        let mut current = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut current) },
            0
        );
        let interactive_flags = libc::ICANON | libc::ECHO;
        assert_eq!(
            current.c_lflag & interactive_flags,
            self.original_mode.c_lflag & interactive_flags
        );
        assert!(
            self.output
                .windows(b"\x1b[?25h".len())
                .any(|window| window == b"\x1b[?25h"),
            "cursor-show sequence missing from {}",
            String::from_utf8_lossy(&self.output)
        );
    }

    fn read_available(&mut self, timeout: Duration) {
        let mut descriptor = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready <= 0 || descriptor.revents & libc::POLLIN == 0 {
            return;
        }
        let mut bytes = [0_u8; 4096];
        let read = unsafe {
            libc::read(
                self.master.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if read > 0 {
            self.output.extend_from_slice(&bytes[..read as usize]);
        }
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Case {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let codex_home = home.join(".codex");
        let codex = home.join("fake-codex");
        let store = home.join(".codex-auth");
        fs::create_dir_all(&codex_home).unwrap();
        write_executable(
            &codex,
            r#"#!/bin/sh
case "$*" in
*app-server*)
  mode=${FAKE_CREDENTIAL_STORE:-file}
  while IFS= read -r line; do
    case "$line" in
      *'"id":0'*) printf '%s\n' '{"id":0,"result":{}}' ;;
      *'"method":"config/read"'*)
        printf '{"id":1,"result":{"config":{"cli_auth_credentials_store":"%s"}}}\n' "$mode"
        ;;
    esac
  done
  exit 0
  ;;
esac
if [ "$1" = login ] && [ -n "$FAKE_AUTH" ]; then
  if [ -n "$FAKE_LOGIN_ARGS" ]; then
    printf '%s\n' "$@" > "$FAKE_LOGIN_ARGS"
  fi
  cp "$FAKE_AUTH" "$CODEX_HOME/auth.json"
  exit 0
fi
exit 1
"#,
        );
        Self {
            _root: root,
            home,
            codex_home,
            codex,
            store,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cxa"));
        command
            .env("HOME", &self.home)
            .env("CODEX_HOME", &self.codex_home)
            .env("CXA_CODEX_BIN", &self.codex)
            .env("CXA_ACCOUNT_STORE", &self.store)
            .env("CXA_SKIP_USAGE_REFRESH", "1")
            .env_remove("CODEX_ACCESS_TOKEN")
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("NO_COLOR");
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command().args(arguments).output().unwrap()
    }

    fn seed(&self, email: &str, user: &str, account: &str) {
        write_auth(
            &self.codex_home.join("auth.json"),
            email,
            user,
            account,
            "token-one",
        );
        let output = self.run(&["init", "--yes"]);
        assert_success(&output);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_auth(path: &Path, email: &str, user_id: &str, account_id: &str, access_token: &str) {
    write_auth_at(
        path,
        email,
        user_id,
        account_id,
        access_token,
        "2026-08-28T00:00:00Z",
    );
}

fn write_auth_at(
    path: &Path,
    email: &str,
    user_id: &str,
    account_id: &str,
    access_token: &str,
    last_refresh: &str,
) {
    let claims = json!({
        "email": email,
        "https://api.openai.com/auth": {"chatgpt_user_id": user_id}
    });
    let id_token = format!(
        "header.{}.signature",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    let value = json!({
        "last_refresh": last_refresh,
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": format!("refresh-{access_token}"),
            "account_id": account_id
        }
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn access_token(path: &Path) -> String {
    let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    value["tokens"]["access_token"].as_str().unwrap().to_owned()
}

fn write_executable(path: &Path, contents: &str) {
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true).mode(0o755);
    use std::io::Write as _;
    options
        .open(path)
        .unwrap()
        .write_all(contents.as_bytes())
        .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct ReapedChild(Child);

impl Drop for ReapedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct BoostFixture<'a> {
    case: &'a Case,
    socket: PathBuf,
    state: PathBuf,
    _server: ReapedChild,
}

impl<'a> BoostFixture<'a> {
    fn new(case: &'a Case, state: Value) -> Self {
        let state_path = case.home.join("fake-boost-state.json");
        let endpoint_available = state["endpoint_available"].as_bool().unwrap();
        fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        write_executable(&case.codex, FAKE_BOOST_CODEX);
        let socket = case.home.join("app-server-control.sock");
        let mut server = ReapedChild(
            Command::new(&case.codex)
                .args(["fixture-server", socket.to_str().unwrap()])
                .env("FAKE_BOOST_STATE", &state_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if endpoint_available && socket.exists() {
                break;
            }
            if let Some(status) = server.0.try_wait().unwrap() {
                let mut stderr = String::new();
                server
                    .0
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                assert!(
                    !endpoint_available,
                    "fake control server exited during startup with {status}: {stderr}"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for fake control server"
            );
            thread::sleep(Duration::from_millis(10));
        }
        Self {
            case,
            socket,
            state: state_path,
            _server: server,
        }
    }

    fn command(&self) -> Command {
        let mut command = self.case.command();
        command.env("FAKE_BOOST_STATE", &self.state);
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command().args(arguments).output().unwrap()
    }

    fn start(&mut self, seconds: i64) -> Output {
        let until = future_deadline(seconds);
        self.command()
            .args([
                "boost",
                "start",
                "--until",
                &until,
                "--socket",
                self.socket.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    }

    fn snapshot(&self) -> Value {
        let output = Command::new(&self.case.codex)
            .arg("fixture-snapshot")
            .env("FAKE_BOOST_STATE", &self.state)
            .output()
            .unwrap();
        assert_success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn fixture_action(&self, arguments: &[&str]) {
        let output = Command::new(&self.case.codex)
            .args(arguments)
            .env("FAKE_BOOST_STATE", &self.state)
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn add_loaded_root(&self) {
        self.fixture_action(&["fixture-add-root"]);
    }

    fn add_loaded_empty(&self, id: &str) {
        self.fixture_action(&["fixture-add-empty", id]);
    }

    fn activate_task(&self, id: &str) {
        self.fixture_action(&["fixture-activate", id]);
    }

    fn wait_for_snapshot(
        &self,
        description: &str,
        timeout: Duration,
        check: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let state = self.snapshot();
            if check(&state) {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}; fake state: {state:#}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_phase(&self, phase: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(state) = self.boost_state().filter(|state| state["phase"] == phase) {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for boost phase {phase}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn boost_state(&self) -> Option<Value> {
        fs::read(self.case.store.join("boost/state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    fn controller_is_terminal(&self) -> bool {
        self.boost_state()
            .is_none_or(|state| state["phase"] == "stopped")
    }
}

impl Drop for BoostFixture<'_> {
    fn drop(&mut self) {
        let _ = self.run(&["boost", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.controller_is_terminal() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        if !self.controller_is_terminal() {
            let message = format!(
                "boost controller did not stop during fixture cleanup: {:#?}",
                self.boost_state()
            );
            if std::thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

fn future_deadline(seconds: i64) -> String {
    (Utc::now() + ChronoDuration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn task_state(
    parent: Option<&str>,
    direct_input: bool,
    model: &str,
    effort: &str,
    tier: Value,
) -> Value {
    json!({
        "parentThreadId": parent,
        "canAcceptDirectInput": direct_input,
        "modelProvider": "openai",
        "status": {"type": "active", "activeFlags": []},
        "materialized": true,
        "path": "/tmp/work/thread.jsonl",
        "model": model,
        "reasoningEffort": effort,
        "serviceTier": tier,
        "permissions": task_permissions(),
        "turn": {"id": format!("turn-{}", parent.unwrap_or("root")), "status": "inProgress"}
    })
}

fn empty_task_state() -> Value {
    json!({
        "parentThreadId": null,
        "canAcceptDirectInput": true,
        "modelProvider": "openai",
        "status": {"type": "idle"},
        "materialized": false,
        "path": "/tmp/work/empty.jsonl",
        "model": "gpt-5.6-sol",
        "reasoningEffort": "high",
        "serviceTier": "default",
        "permissions": task_permissions(),
        "turn": null
    })
}

fn task_permissions() -> Value {
    json!({
        "cwd": "/tmp/work",
        "approvalPolicy": "on-request",
        "approvalsReviewer": "user",
        "sandboxPolicy": {
            "type": "workspaceWrite",
            "writableRoots": ["/tmp/work"],
            "networkAccess": false
        },
        "activePermissionProfile": null,
        "summary": null,
        "collaborationMode": {"mode": "default"},
        "multiAgentMode": "explicitRequestOnly",
        "personality": "pragmatic"
    })
}

fn fake_boost_state(model_available: bool, endpoint_available: bool, quota: &[f64]) -> Value {
    json!({
        "model_available": model_available,
        "endpoint_available": endpoint_available,
        "quota": quota,
        "quota_index": 0,
        "defaults": {
            "model": "gpt-5.6-sol",
            "reasoningEffort": "high",
            "planModeReasoningEffort": "medium",
            "serviceTier": "default",
            "fastMode": true
        },
        "loaded": ["root-1", "child-1"],
        "tasks": {
            "root-1": task_state(None, true, "gpt-5.6-sol", "high", Value::Null),
            "child-1": task_state(Some("root-1"), false, "gpt-6-astra", "ultra", json!("priority"))
        },
        "events": []
    })
}

fn fake_empty_boost_state() -> Value {
    json!({
        "model_available": true,
        "endpoint_available": true,
        "quota": [42.0],
        "quota_index": 0,
        "defaults": {
            "model": "gpt-5.6-sol",
            "reasoningEffort": "high",
            "planModeReasoningEffort": "medium",
            "serviceTier": "default",
            "fastMode": true
        },
        "loaded": ["empty-1"],
        "tasks": {"empty-1": empty_task_state()},
        "events": []
    })
}

fn event_index(
    state: &Value,
    method: &str,
    thread_id: &str,
    effort: Option<&str>,
) -> Option<usize> {
    state["events"].as_array()?.iter().position(|event| {
        event["method"] == method
            && event["threadId"] == thread_id
            && effort.is_none_or(|effort| event["effort"] == effort)
    })
}

const FAKE_BOOST_CODEX: &str = r#"#!/usr/bin/env python3
import fcntl
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import threading

state_path = os.environ["FAKE_BOOST_STATE"]

class RpcError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code

def transaction(action):
    lock_path = state_path + ".lock"
    with open(lock_path, "a+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        with open(state_path) as source:
            state = json.load(source)
        result = action(state)
        temporary = state_path + ".tmp." + str(os.getpid())
        with open(temporary, "w") as output:
            json.dump(state, output, sort_keys=True)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, state_path)
        return result

def task_event(state, method, params):
    event = {"method": method, "threadId": params.get("threadId")}
    if "effort" in params:
        event["effort"] = params["effort"]
    if "serviceTier" in params:
        event["serviceTier"] = params["serviceTier"]
    state["events"].append(event)

def default_permissions():
    return {
        "cwd": "/tmp/work",
        "approvalPolicy": "on-request",
        "approvalsReviewer": "user",
        "sandboxPolicy": {
            "type": "workspaceWrite",
            "writableRoots": ["/tmp/work"],
            "networkAccess": False,
        },
        "activePermissionProfile": None,
        "summary": None,
        "collaborationMode": {"mode": "default"},
        "multiAgentMode": "explicitRequestOnly",
        "personality": "pragmatic",
    }

def empty_task():
    return {
        "parentThreadId": None,
        "canAcceptDirectInput": True,
        "modelProvider": "openai",
        "status": {"type": "idle"},
        "materialized": False,
        "path": "/tmp/work/empty.jsonl",
        "model": "gpt-5.6-sol",
        "reasoningEffort": "high",
        "serviceTier": "default",
        "permissions": default_permissions(),
        "turn": None,
    }

def add_loaded_root(state):
    state["loaded"].append("root-2")
    state["tasks"]["root-2"] = {
        "parentThreadId": None,
        "canAcceptDirectInput": True,
        "modelProvider": "openai",
        "status": {"type": "active", "activeFlags": []},
        "materialized": True,
        "path": "/tmp/work/thread.jsonl",
        "model": "gpt-5.6-sol",
        "reasoningEffort": "high",
        "serviceTier": None,
        "permissions": default_permissions(),
        "turn": {"id": "turn-root-2", "status": "inProgress"},
    }

def add_loaded_empty(state, thread_id):
    state["loaded"].append(thread_id)
    state["tasks"][thread_id] = empty_task()

def activate_task(state, thread_id):
    task = state["tasks"][thread_id]
    task["status"] = {"type": "active", "activeFlags": []}
    task["materialized"] = True
    task["turn"] = {"id": "external-" + thread_id, "status": "inProgress"}
    state["events"].append({"method": "fixture/activate", "threadId": thread_id})

arguments = sys.argv[1:]
if arguments == ["fixture-snapshot"]:
    print(json.dumps(transaction(lambda state: state), sort_keys=True))
    sys.exit(0)
if arguments == ["fixture-add-root"]:
    transaction(add_loaded_root)
    sys.exit(0)
if len(arguments) == 2 and arguments[0] == "fixture-add-empty":
    transaction(lambda state: add_loaded_empty(state, arguments[1]))
    sys.exit(0)
if len(arguments) == 2 and arguments[0] == "fixture-activate":
    transaction(lambda state: activate_task(state, arguments[1]))
    sys.exit(0)

def handle(method, params):
    if method == "initialize":
        return {}
    if method == "model/list":
        available = transaction(lambda state: state["model_available"])
        models = []
        if available:
            models.append({
                "model": "gpt-6-astra",
                "supportedReasoningEfforts": [
                    {"reasoningEffort": "medium"},
                    {"reasoningEffort": "ultra"},
                ],
                "serviceTiers": [{"id": "fast"}],
            })
        return {"data": models, "nextCursor": None}
    if method == "thread/loaded/list":
        return transaction(lambda state: {"data": list(state["loaded"]), "nextCursor": None})
    if method == "thread/read":
        def read_task(state):
            task = state["tasks"][params["threadId"]]
            task_event(state, method, params)
            return {"thread": {
                "id": params["threadId"],
                "parentThreadId": task["parentThreadId"],
                "canAcceptDirectInput": task["canAcceptDirectInput"],
                "modelProvider": task["modelProvider"],
                "status": task["status"],
                "path": task["path"],
            }}
        return transaction(read_task)
    if method == "thread/resume":
        def resume(state):
            task = state["tasks"][params["threadId"]]
            if not task["materialized"]:
                raise RpcError(-32600, "no rollout found for thread id " + params["threadId"])
            return {
                "model": task["model"],
                "reasoningEffort": task["reasoningEffort"],
                "serviceTier": task["serviceTier"],
            }
        return transaction(resume)
    if method == "thread/turns/list":
        def turns(state):
            task = state["tasks"][params["threadId"]]
            if not task["materialized"]:
                raise RpcError(
                    -32600,
                    "thread " + params["threadId"]
                    + " is not materialized yet; thread/turns/list is unavailable before first user message",
                )
            turn = task.get("turn")
            return {"data": [] if turn is None else [dict(turn)], "nextCursor": None}
        return transaction(turns)
    if method == "turn/interrupt":
        def interrupt(state):
            task = state["tasks"][params["threadId"]]
            if task.get("turn") is not None:
                task["turn"]["status"] = "interrupted"
            task_event(state, method, params)
            return {}
        return transaction(interrupt)
    if method in ("thread/settings/update", "turn/settings/update"):
        def update(state):
            task = state["tasks"][params["threadId"]]
            task["model"] = params["model"]
            task["reasoningEffort"] = params["effort"]
            task["serviceTier"] = "priority" if params["serviceTier"] == "fast" else params["serviceTier"]
            task_event(state, method, params)
            return {}
        return transaction(update)
    if method == "turn/start":
        def start_turn(state):
            task = state["tasks"][params["threadId"]]
            task["model"] = params["model"]
            task["reasoningEffort"] = params["effort"]
            task["serviceTier"] = "priority" if params["serviceTier"] == "fast" else params["serviceTier"]
            task["turn"] = {"id": "continued-" + params["threadId"], "status": "inProgress"}
            task_event(state, method, params)
            return {}
        return transaction(start_turn)
    if method == "account/read":
        return {
            "account": {
                "type": "chatgpt",
                "email": "one@example.com",
                "planType": "pro",
            },
            "requiresOpenaiAuth": True,
        }
    if method == "account/rateLimits/read":
        def quota(state):
            index = min(state["quota_index"], len(state["quota"]) - 1)
            used = state["quota"][index]
            state["quota_index"] += 1
            return {"rateLimitsByLimitId": {"codex": {
                "limitId": "codex",
                "primary": {
                    "usedPercent": used,
                    "resetsAt": 4000000000,
                    "windowDurationMins": 10080,
                },
            }}}
        return transaction(quota)
    if method == "config/read":
        def config(state):
            defaults = state["defaults"]
            return {
                "config": {
                    "cli_auth_credentials_store": "file",
                    "model": defaults["model"],
                    "model_reasoning_effort": defaults["reasoningEffort"],
                    "plan_mode_reasoning_effort": defaults["planModeReasoningEffort"],
                    "service_tier": defaults["serviceTier"],
                    "features": {"fast_mode": defaults["fastMode"]},
                },
                "origins": {},
                "layers": [{
                    "name": {"type": "user", "profile": None},
                    "version": "user-version-1",
                    "config": {},
                    "disabledReason": None,
                }],
                "requirements": {},
            }
        return transaction(config)
    if method == "config/batchWrite":
        def batch_write(state):
            if params.get("expectedVersion") != "user-version-1":
                raise ValueError("missing base user config version")
            edits = {edit["keyPath"]: edit["value"] for edit in params["edits"]}
            state["defaults"] = {
                "model": edits["model"],
                "reasoningEffort": edits["model_reasoning_effort"],
                "planModeReasoningEffort": edits["plan_mode_reasoning_effort"],
                "serviceTier": edits["service_tier"],
                "fastMode": edits["features.fast_mode"],
            }
            state["events"].append({"method": method, "settings": dict(state["defaults"])})
            return {}
        return transaction(batch_write)
    raise ValueError("unsupported method: " + method)

def settings_updated(thread_id):
    def notification(state):
        task = state["tasks"][thread_id]
        if not task["materialized"]:
            return None
        settings = dict(task["permissions"])
        settings.update({
            "model": task["model"],
            "modelProvider": task["modelProvider"],
            "serviceTier": task["serviceTier"],
            "effort": task["reasoningEffort"],
        })
        return {
            "method": "thread/settings/updated",
            "params": {"threadId": thread_id, "threadSettings": settings},
        }
    return transaction(notification)

def receive_exact(connection, size):
    chunks = []
    remaining = size
    while remaining:
        chunk = connection.recv(remaining)
        if not chunk:
            raise EOFError()
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)

def receive_frame(connection):
    first, second = receive_exact(connection, 2)
    opcode = first & 0x0f
    length = second & 0x7f
    if length == 126:
        length = struct.unpack("!H", receive_exact(connection, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", receive_exact(connection, 8))[0]
    if not second & 0x80:
        raise ValueError("client websocket frame was not masked")
    mask = receive_exact(connection, 4)
    payload = receive_exact(connection, length)
    payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    return opcode, payload

def send_frame(connection, opcode, payload):
    length = len(payload)
    if length < 126:
        header = bytes([0x80 | opcode, length])
    elif length <= 0xffff:
        header = bytes([0x80 | opcode, 126]) + struct.pack("!H", length)
    else:
        header = bytes([0x80 | opcode, 127]) + struct.pack("!Q", length)
    connection.sendall(header + payload)

def serve_connection(connection):
    try:
        request = b""
        while b"\r\n\r\n" not in request:
            request += connection.recv(4096)
            if len(request) > 16384:
                raise ValueError("oversized websocket handshake")
        headers = {}
        for line in request.decode("ascii").split("\r\n")[1:]:
            if ":" in line:
                name, value = line.split(":", 1)
                headers[name.lower()] = value.strip()
        key = headers["sec-websocket-key"]
        accept = base64.b64encode(hashlib.sha1(
            (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
        ).digest()).decode("ascii")
        connection.sendall((
            "HTTP/1.1 101 Switching Protocols\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            "Sec-WebSocket-Accept: " + accept + "\r\n\r\n"
        ).encode("ascii"))
        while True:
            opcode, payload = receive_frame(connection)
            if opcode == 8:
                return
            if opcode == 9:
                send_frame(connection, 10, payload)
                continue
            if opcode != 1:
                raise ValueError("unsupported websocket opcode: " + str(opcode))
            message = json.loads(payload.decode("utf-8"))
            if "id" not in message:
                continue
            try:
                result = handle(message["method"], message.get("params", {}))
                response = {"id": message["id"], "result": result}
            except Exception as error:
                response = {"id": message["id"], "error": {
                    "code": getattr(error, "code", -32603),
                    "message": str(error),
                }}
            if message["method"] == "thread/settings/update" and "result" in response:
                notification = settings_updated(message["params"]["threadId"])
                if notification is not None:
                    send_frame(connection, 1, json.dumps(notification, separators=(",", ":")).encode("utf-8"))
            send_frame(connection, 1, json.dumps(response, separators=(",", ":")).encode("utf-8"))
    except (BrokenPipeError, ConnectionResetError, EOFError):
        pass
    finally:
        connection.close()

if len(arguments) == 2 and arguments[0] == "fixture-server":
    if not transaction(lambda state: state["endpoint_available"]):
        sys.exit(12)
    socket_path = arguments[1]
    try:
        os.unlink(socket_path)
    except FileNotFoundError:
        pass
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(socket_path)
    server.listen()
    while True:
        connection, _ = server.accept()
        threading.Thread(target=serve_connection, args=(connection,), daemon=True).start()

for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    try:
        result = handle(message["method"], message.get("params", {}))
        response = {"id": message["id"], "result": result}
    except Exception as error:
        response = {"id": message["id"], "error": {
            "code": getattr(error, "code", -32603),
            "message": str(error),
        }}
    print(json.dumps(response, separators=(",", ":")), flush=True)
"#;

fn sleeping_codex(case: &Case) -> (PathBuf, Child) {
    let path = case.home.join("codex-running");
    write_executable(&path, "#!/bin/sh\nsleep 30\n");
    let child = Command::new(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    (path, child)
}

#[test]
fn bare_command_recommends_init_for_the_current_login() {
    let case = Case::new();
    write_auth(
        &case.codex_home.join("auth.json"),
        "current@example.com",
        "user-current",
        "account-current",
        "current",
    );

    let output = case.run(&[]);

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Found the current Codex login: current@example.com"));
    assert!(stdout.contains("cxa is not initialized. Run: cxa init"));
}

#[test]
fn redirected_init_requires_yes_without_creating_a_profile() {
    let case = Case::new();
    write_auth(
        &case.codex_home.join("auth.json"),
        "current@example.com",
        "user-current",
        "account-current",
        "current",
    );

    let output = case.run(&["init"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cxa init --yes"));
    assert!(!case.store.join("profile-1/auth.json").exists());
}

#[test]
fn init_imports_and_selects_the_current_login() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");

    assert_eq!(
        access_token(&case.store.join("profile-1/auth.json")),
        "token-one"
    );
}

#[test]
fn switch_works_while_codex_is_running_and_prints_restart_guidance() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let imported = case.home.join("two.json");
    write_auth(
        &imported,
        "two@example.com",
        "user-two",
        "account-two",
        "token-two",
    );
    assert_success(&case.run(&["import", imported.to_str().unwrap()]));
    let (_codex, mut child) = sleeping_codex(&case);

    let output = case.run(&["use", "2"]);
    let _ = child.kill();
    let _ = child.wait();

    assert_success(&output);
    assert_eq!(
        access_token(&case.codex_home.join("auth.json")),
        "token-two"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(
        "Restart Codex or ChatGPT before expecting an existing session to use this account."
    ));
}

#[test]
fn switching_preserves_live_credentials_when_timestamps_tie() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let second = case.home.join("second.json");
    write_auth(&second, "two@example.com", "user-two", "account-two", "two");
    assert_success(&case.run(&["import", second.to_str().unwrap()]));
    write_auth_at(
        &case.codex_home.join("auth.json"),
        "one@example.com",
        "user-one",
        "account-one",
        "refreshed-one",
        "2026-08-28T00:00:00Z",
    );

    assert_success(&case.run(&["2"]));
    assert_success(&case.run(&["1"]));

    assert_eq!(
        access_token(&case.codex_home.join("auth.json")),
        "refreshed-one"
    );
}

#[test]
fn switching_replaces_the_session_symlink_without_touching_its_old_target() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let imported = case.home.join("two.json");
    write_auth(
        &imported,
        "two@example.com",
        "user-two",
        "account-two",
        "token-two",
    );
    assert_success(&case.run(&["import", imported.to_str().unwrap()]));
    let old_target = case.home.join("old-active.json");
    fs::rename(case.codex_home.join("auth.json"), &old_target).unwrap();
    symlink(&old_target, case.codex_home.join("auth.json")).unwrap();

    assert_success(&case.run(&["2"]));

    assert_eq!(
        access_token(&case.codex_home.join("auth.json")),
        "token-two"
    );
    assert_eq!(access_token(&old_target), "token-one");
    assert!(
        !fs::symlink_metadata(case.codex_home.join("auth.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn import_rejects_duplicate_account_identity() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let duplicate = case.home.join("duplicate.json");
    write_auth(
        &duplicate,
        "renamed@example.com",
        "user-one",
        "account-one",
        "new-token",
    );

    let output = case.run(&["import", duplicate.to_str().unwrap()]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already enrolled as account 1"));
}

#[test]
fn same_email_in_different_workspaces_remains_distinct() {
    let case = Case::new();
    case.seed("same@example.com", "same-user", "workspace-one");
    let second = case.home.join("second.json");
    write_auth(
        &second,
        "same@example.com",
        "same-user",
        "workspace-two",
        "second",
    );

    assert_success(&case.run(&["import", second.to_str().unwrap()]));

    assert!(case.store.join("profile-2/auth.json").is_file());
}

#[test]
fn add_runs_login_in_an_isolated_home() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let fresh = case.home.join("fresh.json");
    write_auth(
        &fresh,
        "two@example.com",
        "user-two",
        "account-two",
        "token-two",
    );
    let login_args = case.home.join("login-args.txt");
    let output = case
        .command()
        .env("FAKE_AUTH", &fresh)
        .env("FAKE_LOGIN_ARGS", &login_args)
        .arg("add")
        .output()
        .unwrap();

    assert_success(&output);
    assert_eq!(
        access_token(&case.store.join("profile-2/auth.json")),
        "token-two"
    );
    assert_eq!(
        access_token(&case.codex_home.join("auth.json")),
        "token-one"
    );
    assert!(
        !fs::read_to_string(login_args)
            .unwrap()
            .lines()
            .any(|argument| argument == "--device-auth")
    );
}

#[test]
fn add_forwards_device_auth_to_codex_login() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let fresh = case.home.join("fresh.json");
    write_auth(
        &fresh,
        "two@example.com",
        "user-two",
        "account-two",
        "token-two",
    );
    let login_args = case.home.join("login-args.txt");
    let output = case
        .command()
        .env("FAKE_AUTH", &fresh)
        .env("FAKE_LOGIN_ARGS", &login_args)
        .args(["add", "--device-auth"])
        .output()
        .unwrap();

    assert_success(&output);
    assert!(
        fs::read_to_string(login_args)
            .unwrap()
            .lines()
            .any(|argument| argument == "--device-auth")
    );
}

#[test]
fn relogin_rejects_a_different_account() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let wrong = case.home.join("wrong.json");
    write_auth(
        &wrong,
        "wrong@example.com",
        "wrong-user",
        "wrong-account",
        "wrong",
    );
    let output = case
        .command()
        .env("FAKE_AUTH", &wrong)
        .args(["relogin", "1"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(
        access_token(&case.store.join("profile-1/auth.json")),
        "token-one"
    );
}

#[test]
fn selected_relogin_updates_the_session_and_prints_restart_guidance() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let replacement = case.home.join("replacement.json");
    write_auth(
        &replacement,
        "one@example.com",
        "user-one",
        "account-one",
        "replacement",
    );
    let output = case
        .command()
        .env("FAKE_AUTH", &replacement)
        .args(["relogin", "1"])
        .output()
        .unwrap();

    assert_success(&output);
    assert_eq!(
        access_token(&case.codex_home.join("auth.json")),
        "replacement"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Restart Codex or ChatGPT"));
}

#[test]
fn list_preserves_rotation_and_restart_guidance_when_quota_fails() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let refreshed = case.home.join("refreshed.json");
    write_auth_at(
        &refreshed,
        "one@example.com",
        "user-one",
        "account-one",
        "refreshed-one",
        "2026-08-28T00:00:00Z",
    );
    let codex = case.home.join("fake-codex");
    write_executable(
        &codex,
        r#"#!/bin/sh
case "$CODEX_HOME" in
  "$CXA_ACCOUNT_STORE"/.quota-*) ;;
  *)
    while IFS= read -r line; do
      case "$line" in
        *'"id":0'*) printf '%s\n' '{"id":0,"result":{}}' ;;
        *'"method":"config/read"'*) printf '%s\n' '{"id":1,"result":{"config":{"cli_auth_credentials_store":"file"}}}' ;;
      esac
    done
    exit 0
    ;;
esac
while IFS= read -r line; do
  case "$line" in
    *'"id":0'*) printf '%s\n' '{"id":0,"result":{}}' ;;
    *'"id":1'*)
      case "$line" in *'"refreshToken":false'*) ;; *) exit 2 ;; esac
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"id":2'*)
      cp "$FAKE_REFRESHED" "$CODEX_HOME/auth.json"
      printf '%s\n' '{"id":2,"error":{"code":-32000,"message":"quota failed"}}'
      ;;
  esac
done
"#,
    );

    let output = case
        .command()
        .env_remove("CXA_SKIP_USAGE_REFRESH")
        .env("CXA_CODEX_BIN", &codex)
        .env("FAKE_REFRESHED", &refreshed)
        .arg("list")
        .output()
        .unwrap();

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("quota unavailable (Protocol)"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Restart Codex or ChatGPT"));
    assert_eq!(
        access_token(&case.store.join("profile-1/auth.json")),
        "refreshed-one"
    );
    assert_eq!(
        access_token(&case.codex_home.join("auth.json")),
        "refreshed-one"
    );
}

#[test]
fn list_attributes_quota_to_each_saved_profile() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let second = case.home.join("second.json");
    write_auth(
        &second,
        "two@example.com",
        "user-two",
        "account-two",
        "token-two",
    );
    assert_success(&case.run(&["import", second.to_str().unwrap()]));
    let codex = case.home.join("fake-codex");
    write_executable(
        &codex,
        r#"#!/bin/sh
case "$CODEX_HOME" in
  "$CXA_ACCOUNT_STORE"/.quota-*) ;;
  *)
    while IFS= read -r line; do
      case "$line" in
        *'"id":0'*) printf '%s\n' '{"id":0,"result":{}}' ;;
        *'"method":"config/read"'*) printf '%s\n' '{"id":1,"result":{"config":{"cli_auth_credentials_store":"file"}}}' ;;
      esac
    done
    exit 0
    ;;
esac
if grep -q token-one "$CODEX_HOME/auth.json"; then
  used=11
  spark=0
  account=one
else
  used=77
  spark=100
  account=two
fi
touch "$CXA_ACCOUNT_STORE/$account.started"
attempt=0
while [ ! -e "$CXA_ACCOUNT_STORE/one.started" ] || [ ! -e "$CXA_ACCOUNT_STORE/two.started" ]; do
  attempt=$((attempt + 1))
  [ "$attempt" -lt 100 ] || exit 9
  sleep 0.01
done
if [ "$account" = one ]; then sleep 0.2; fi
while IFS= read -r line; do
  case "$line" in
    *'"id":0'*) printf '%s\n' '{"id":0,"result":{}}' ;;
    *'"id":1'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"id":2'*)
      printf '{"id":2,"result":{"rateLimitsByLimitId":{"codex":{"limitId":"codex","planType":"pro","primary":{"usedPercent":%s,"windowDurationMins":10080}},"codex_bengalfox":{"limitId":"codex_bengalfox","limitName":"GPT-5.3-Codex-Spark","planType":"pro","primary":{"usedPercent":0,"windowDurationMins":300},"secondary":{"usedPercent":%s,"windowDurationMins":10080}}}}}\n' "$used" "$spark"
      ;;
  esac
done
"#,
    );

    let output = case
        .command()
        .env_remove("CXA_SKIP_USAGE_REFRESH")
        .env("CXA_CODEX_BIN", &codex)
        .arg("list")
        .output()
        .unwrap();

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let account_two = stdout.find("two@example.com").unwrap();
    let account_one_output = &stdout[..account_two];
    let account_two_output = &stdout[account_two..];
    assert!(account_one_output.contains("one@example.com  Pro 20x · updated just now"));
    assert!(account_one_output.contains("11% used"));
    assert!(!account_one_output.contains("77% used"));
    assert!(account_two_output.contains("77% used"));
    assert!(account_two_output.contains("Codex Spark  EXHAUSTED"));
    assert!(account_two_output.contains("[████████████████] 100% used"));
    assert!(!stdout.contains("codex primary"));
    assert!(stdout.lines().all(|line| line.chars().count() <= 80));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Fetching usage"));
}

#[test]
fn status_infers_selection_when_codex_changes_to_an_enrolled_account() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let second = case.home.join("second.json");
    write_auth(&second, "two@example.com", "user-two", "account-two", "two");
    assert_success(&case.run(&["import", second.to_str().unwrap()]));
    fs::copy(&second, case.codex_home.join("auth.json")).unwrap();

    let output = case.run(&["status"]);

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("* 2  two@example.com"));
}

#[test]
fn relative_configuration_paths_are_rejected() {
    let case = Case::new();
    let output = case
        .command()
        .env("CXA_ACCOUNT_STORE", "relative-store")
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be an absolute path"));
}

#[test]
fn overlapping_codex_home_and_account_store_are_rejected() {
    let case = Case::new();
    let output = case
        .command()
        .env("CXA_ACCOUNT_STORE", case.codex_home.join("accounts"))
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be separate directories"));
}

#[test]
fn redirected_output_contains_no_colour_codes() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");

    let output = case.run(&["status"]);

    assert_success(&output);
    assert!(!output.stdout.windows(2).any(|bytes| bytes == b"\x1b["));

    let empty = Case::new();
    let output = empty.run(&["list"]);
    assert_success(&output);
    assert!(!output.stdout.windows(2).any(|bytes| bytes == b"\x1b["));
}

#[test]
fn watch_requires_an_interactive_terminal() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");

    for arguments in [["watch"].as_slice(), ["list", "--watch"].as_slice()] {
        let output = case.run(arguments);

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Watch mode requires an interactive terminal")
        );
    }
}

#[test]
fn watch_exit_remains_responsive_while_the_account_lock_is_held() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let _lock = cxa::fs::ExclusiveLock::acquire(&case.store.join("switch.lock")).unwrap();
    let mut command = case.command();
    command.arg("watch");
    let mut watch = PtyChild::spawn(command);

    watch.wait_for_output(b"\x1b[?25l");
    watch.send(b"q");
    watch.wait_success();
    watch.assert_terminal_restored();
}

#[test]
fn unrelated_keys_do_not_consume_the_watch_interval() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let mut command = case.command();
    command.args(["watch", "--interval", "5"]);
    let mut watch = PtyChild::spawn(command);

    watch.wait_for_output(b"refresh in 5s");
    for _ in 0..10 {
        watch.send(b"\x1b[A");
    }
    thread::sleep(Duration::from_millis(250));
    watch.read_available(Duration::from_millis(50));
    assert!(
        !watch
            .output
            .windows(b"refresh in 4s".len())
            .any(|window| window == b"refresh in 4s"),
        "unrelated input advanced the countdown: {}",
        String::from_utf8_lossy(&watch.output)
    );
    watch.send(b"q");
    watch.wait_success();
    watch.assert_terminal_restored();
}

#[test]
fn watch_cancels_active_quota_workers_before_exiting() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let codex = case.home.join("slow-codex");
    write_executable(
        &codex,
        r#"#!/bin/sh
case "$CODEX_HOME" in
  "$CXA_ACCOUNT_STORE"/.quota-*)
    trap 'touch "$CXA_ACCOUNT_STORE/quota.stopped"; exit 0' HUP INT TERM
    while IFS= read -r line; do
      case "$line" in
        *'"id":0'*) printf '%s\n' '{"id":0,"result":{}}' ;;
        *'"id":1'*) printf '%s\n' '{"id":1,"result":{}}' ;;
        *'"id":2'*)
          touch "$CXA_ACCOUNT_STORE/quota.started"
          while :; do sleep 1; done
          ;;
      esac
    done
    ;;
  *)
    while IFS= read -r line; do
      case "$line" in
        *'"id":0'*) printf '%s\n' '{"id":0,"result":{}}' ;;
        *'"method":"config/read"'*) printf '%s\n' '{"id":1,"result":{"config":{"cli_auth_credentials_store":"file"}}}' ;;
      esac
    done
    ;;
esac
"#,
    );
    let mut command = case.command();
    command
        .env_remove("CXA_SKIP_USAGE_REFRESH")
        .env("CXA_CODEX_BIN", &codex)
        .arg("watch");
    let mut watch = PtyChild::spawn(command);

    watch.wait_for_output(b"loading");
    let started = case.store.join("quota.started");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(started.is_file());
    watch.send(b"q");
    watch.wait_success();
    watch.assert_terminal_restored();
    let reserved = watch
        .output
        .windows(b"\x1b[1A\x1b[s".len())
        .position(|window| window == b"\x1b[1A\x1b[s")
        .unwrap_or_else(|| panic!("no reserved origin in {:?}", watch.output));
    let loading = watch
        .output
        .windows(b"loading".len())
        .position(|window| window == b"loading")
        .unwrap();
    assert!(reserved < loading);
    assert!(
        watch
            .output
            .windows(b"\x1b[s".len())
            .any(|window| window == b"\x1b[s")
    );
    assert!(
        watch
            .output
            .windows(b"\x1b[u\x1b[J".len())
            .any(|window| window == b"\x1b[u\x1b[J")
    );
    assert!(case.store.join("quota.stopped").is_file());
    assert!(fs::read_dir(&case.store).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".quota-")
    }));
}

#[test]
fn termination_signal_restores_watch_terminal_state() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let mut command = case.command();
    command.arg("watch");
    let mut watch = PtyChild::spawn(command);

    watch.wait_for_output(b"Watching");
    watch.signal(libc::SIGTERM);
    watch.wait_success();
    watch.assert_terminal_restored();
}

#[test]
fn informational_flags_do_not_require_home_configuration() {
    let output = Command::new(env!("CARGO_BIN_EXE_cxa"))
        .env_remove("HOME")
        .arg("--version")
        .output()
        .unwrap();

    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .starts_with(&format!("cxa {}", env!("CARGO_PKG_VERSION")))
    );
}

#[test]
fn absolute_path_overrides_do_not_require_home() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");

    let output = case
        .command()
        .env_remove("HOME")
        .arg("status")
        .output()
        .unwrap();

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("one@example.com"));
}

#[test]
fn non_file_codex_credentials_are_rejected_before_switching() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let output = case
        .command()
        .env("FAKE_CREDENTIAL_STORE", "keyring")
        .arg("1")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("cxa requires Codex's file credential store")
    );
}

#[test]
fn malformed_enrolled_profile_is_reported() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let profile = case.store.join("profile-2");
    fs::create_dir_all(&profile).unwrap();
    fs::write(profile.join("auth.json"), b"not json").unwrap();

    let output = case.run(&["list"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("profile-2/auth.json"));
    assert!(stderr.contains("invalid JSON"));
}

#[test]
fn credential_environment_overrides_are_rejected_before_switching() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");

    let output = case
        .command()
        .env("CODEX_ACCESS_TOKEN", "external-token")
        .arg("1")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Unset CODEX_ACCESS_TOKEN"));
}

#[test]
fn api_key_environment_does_not_block_file_credentials() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");

    let output = case
        .command()
        .env("OPENAI_API_KEY", "unrelated")
        .env("CODEX_API_KEY", "unrelated")
        .arg("status")
        .output()
        .unwrap();

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("one@example.com"));
}

#[test]
fn boost_deadline_updates_active_new_and_child_tasks_then_restores_standard() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let mut boost = BoostFixture::new(&case, fake_boost_state(true, true, &[42.0]));

    let output = boost.start(5);
    assert_success(&output);

    let active = boost.wait_for_snapshot(
        "initial tasks to receive boosted settings",
        Duration::from_secs(4),
        |state| {
            state["tasks"]["root-1"]["reasoningEffort"] == "ultra"
                && state["tasks"]["root-1"]["serviceTier"] == "priority"
                && state["tasks"]["child-1"]["reasoningEffort"] == "ultra"
                && event_index(state, "turn/start", "root-1", Some("ultra")).is_some()
                && event_index(state, "turn/settings/update", "child-1", Some("ultra")).is_some()
        },
    );
    let interrupted = event_index(&active, "turn/interrupt", "root-1", None).unwrap();
    let settings = event_index(&active, "thread/settings/update", "root-1", Some("ultra")).unwrap();
    let continued = event_index(&active, "turn/start", "root-1", Some("ultra")).unwrap();
    assert!(interrupted < settings && settings < continued);
    assert!(event_index(&active, "turn/start", "child-1", None).is_none());
    assert_eq!(
        active["defaults"],
        json!({
            "model": "gpt-5.6-sol",
            "reasoningEffort": "high",
            "planModeReasoningEffort": "medium",
            "serviceTier": "default",
            "fastMode": true
        })
    );
    assert!(
        active["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["method"] != "config/batchWrite")
    );

    boost.add_loaded_root();
    boost.wait_for_snapshot(
        "newly loaded root to be interrupted and continued",
        Duration::from_secs(3),
        |state| {
            state["tasks"]["root-2"]["serviceTier"] == "priority"
                && event_index(state, "turn/interrupt", "root-2", None).is_some()
                && event_index(state, "turn/start", "root-2", Some("ultra")).is_some()
        },
    );

    let stopped = boost.wait_for_phase("stopped", Duration::from_secs(8));
    assert_eq!(stopped["reason"], "Deadline reached");
    assert_eq!(stopped["tasks"], json!([]));
    let final_state = boost.snapshot();
    assert_eq!(
        final_state["defaults"],
        json!({
            "model": "gpt-6-astra",
            "reasoningEffort": "medium",
            "planModeReasoningEffort": "medium",
            "serviceTier": "default",
            "fastMode": true
        })
    );
    for task in ["root-1", "root-2", "child-1"] {
        assert_eq!(final_state["tasks"][task]["model"], "gpt-6-astra");
        assert_eq!(final_state["tasks"][task]["reasoningEffort"], "medium");
        assert_eq!(final_state["tasks"][task]["serviceTier"], "default");
        assert_ne!(final_state["tasks"][task]["turn"]["status"], "inProgress");
        assert_eq!(
            final_state["tasks"][task]["permissions"],
            task_permissions()
        );
    }
    assert!(event_index(&final_state, "turn/interrupt", "child-1", None).is_some());
}

#[test]
fn boost_skips_no_rollout_tasks_until_work_materializes() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let mut boost = BoostFixture::new(&case, fake_empty_boost_state());

    let output = boost.start(20);
    assert_success(&output);
    let active = boost.wait_for_phase("active", Duration::from_secs(4));
    assert_eq!(active["tasks"], json!([]));
    let initial = boost.snapshot();
    assert_eq!(initial["tasks"]["empty-1"], empty_task_state());
    assert!(event_index(&initial, "turn/start", "empty-1", None).is_none());
    assert!(event_index(&initial, "thread/settings/update", "empty-1", None).is_none());

    boost.add_loaded_empty("empty-2");
    let both_skipped = boost.wait_for_snapshot(
        "new empty task to be inspected without enrollment",
        Duration::from_secs(3),
        |state| event_index(state, "thread/read", "empty-2", None).is_some(),
    );
    assert_eq!(boost.boost_state().unwrap()["tasks"], json!([]));
    assert_eq!(both_skipped["tasks"]["empty-2"], empty_task_state());
    assert!(event_index(&both_skipped, "turn/start", "empty-2", None).is_none());

    boost.activate_task("empty-1");
    let enrolled = boost.wait_for_snapshot(
        "materialized task to be enrolled and continued",
        Duration::from_secs(3),
        |state| {
            state["tasks"]["empty-1"]["serviceTier"] == "priority"
                && event_index(state, "turn/start", "empty-1", Some("ultra")).is_some()
        },
    );
    let activated = event_index(&enrolled, "fixture/activate", "empty-1", None).unwrap();
    let interrupted = event_index(&enrolled, "turn/interrupt", "empty-1", None).unwrap();
    let continued = event_index(&enrolled, "turn/start", "empty-1", Some("ultra")).unwrap();
    assert!(activated < interrupted && interrupted < continued);
    assert_eq!(
        boost.boost_state().unwrap()["tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let output = boost.run(&["boost", "stop"]);
    assert_success(&output);
    let stopped = boost.wait_for_phase("stopped", Duration::from_secs(8));
    assert_eq!(stopped["reason"], "Stop requested");
    assert_eq!(stopped["tasks"], json!([]));
    let final_state = boost.snapshot();
    assert_eq!(final_state["tasks"]["empty-1"]["model"], "gpt-6-astra");
    assert_eq!(final_state["tasks"]["empty-1"]["reasoningEffort"], "medium");
    assert_eq!(final_state["tasks"]["empty-1"]["serviceTier"], "default");
    assert_eq!(
        final_state["tasks"]["empty-1"]["turn"]["status"],
        "interrupted"
    );
    assert_eq!(final_state["tasks"]["empty-2"], empty_task_state());
    assert!(event_index(&final_state, "turn/start", "empty-2", None).is_none());
    assert!(event_index(&final_state, "thread/settings/update", "empty-2", None).is_none());
}

#[test]
fn boost_weekly_reset_stops_before_its_future_deadline() {
    let case = Case::new();
    case.seed("one@example.com", "user-one", "account-one");
    let mut boost = BoostFixture::new(&case, fake_boost_state(true, true, &[72.0, 4.0]));

    let output = boost.start(25);
    assert_success(&output);
    boost.wait_for_snapshot("boost activation", Duration::from_secs(4), |state| {
        event_index(state, "turn/start", "root-1", Some("ultra")).is_some()
    });

    let stopped = boost.wait_for_phase("stopped", Duration::from_secs(22));
    assert!(
        stopped["reason"]
            .as_str()
            .unwrap()
            .contains("weekly quota reset observed")
    );
    assert!(stopped["deadline"].as_i64().unwrap() > Utc::now().timestamp());
    let final_state = boost.snapshot();
    assert!(final_state["quota_index"].as_u64().unwrap() >= 2);
    assert_eq!(final_state["defaults"]["reasoningEffort"], "medium");
    assert_eq!(final_state["defaults"]["serviceTier"], "default");
}

#[test]
fn boost_preflight_failure_does_not_mutate_defaults_or_tasks() {
    for (model_available, endpoint_available, expected) in [
        (false, true, "Astra is not available"),
        (true, false, "Cannot control Codex"),
    ] {
        let case = Case::new();
        let initial = fake_boost_state(model_available, endpoint_available, &[50.0]);
        let mut boost = BoostFixture::new(&case, initial.clone());

        let output = boost.start(5);

        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
        assert_eq!(boost.snapshot(), initial);
        assert!(boost.boost_state().is_none());
    }
}

#[test]
fn boost_stop_recovers_recorded_pending_cleanup() {
    let case = Case::new();
    let boost = BoostFixture::new(&case, fake_boost_state(true, true, &[50.0]));
    boost.add_loaded_empty("empty-legacy");
    let state_dir = case.store.join("boost");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        state_dir.join("state.json"),
        serde_json::to_vec_pretty(&json!({
            "run_id": "recorded-run",
            "phase": "cleanupRequired",
            "deadline": Utc::now().timestamp() + 300,
            "sockets": [&boost.socket],
            "tasks": [
                {
                    "socket": &boost.socket,
                    "thread_id": "root-1",
                    "child": false
                },
                {
                    "socket": &boost.socket,
                    "thread_id": "empty-legacy",
                    "child": false
                }
            ],
            "activated": true,
            "defaults_pending": true,
            "reason": "controller interrupted",
            "errors": ["recorded cleanup is pending"]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = boost.run(&["boost", "stop"]);

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Boost stopped"));
    let recovered = boost.boost_state().unwrap();
    assert_eq!(recovered["phase"], "stopped");
    assert_eq!(recovered["tasks"], json!([]));
    assert_eq!(recovered["defaults_pending"], false);
    let final_state = boost.snapshot();
    for task in ["root-1", "child-1"] {
        assert_eq!(final_state["tasks"][task]["reasoningEffort"], "medium");
        assert_eq!(final_state["tasks"][task]["serviceTier"], "default");
    }
    assert_eq!(final_state["tasks"]["empty-legacy"], empty_task_state());
    assert!(event_index(&final_state, "thread/settings/update", "empty-legacy", None).is_none());
    assert_eq!(final_state["defaults"]["serviceTier"], "default");
}
