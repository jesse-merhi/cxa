use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use nix::sys::stat::Mode;
use nix::unistd::mkfifo;
use serde_json::{Value, json};
use tempfile::TempDir;

struct Case {
    root: TempDir,
    bin: PathBuf,
    codex_home: PathBuf,
    active_auth: PathBuf,
    store: PathBuf,
}

impl Case {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let codex_home = root.path().join("codex");
        let active_auth = root.path().join("shared/auth.json");
        let store = root.path().join("store");
        for directory in [&bin, &codex_home, active_auth.parent().unwrap(), &store] {
            fs::create_dir_all(directory).unwrap();
        }
        write_executable(
            &bin.join("pgrep"),
            r#"#!/usr/bin/env bash
if [[ -n ${CXA_TEST_PGREP_COUNT_FILE:-} ]]; then
  count=0
  [[ ! -f $CXA_TEST_PGREP_COUNT_FILE ]] || read -r count < "$CXA_TEST_PGREP_COUNT_FILE"
  count=$((count + 1))
  printf '%s\n' "$count" > "$CXA_TEST_PGREP_COUNT_FILE"
  if [[ $count == ${CXA_TEST_WRITER_ON_CALL:-0} ]]; then
    if [[ ${CXA_TEST_WRITE_MALFORMED_ACTIVE:-0} == 1 ]]; then
      printf 'malformed' > "$CXA_ACTIVE_AUTH"
    fi
    if [[ -n ${CXA_TEST_WRITE_ACTIVE_AUTH:-} ]]; then
      cp "$CXA_TEST_WRITE_ACTIVE_AUTH" "$CXA_ACTIVE_AUTH"
    fi
    if [[ -n ${CXA_TEST_WRITE_SESSION_AUTH:-} ]]; then
      cp "$CXA_TEST_WRITE_SESSION_AUTH" "$CODEX_HOME/auth.json"
    fi
    exit 0
  fi
  exit 1
  exit
fi
if [[ ${CXA_TEST_CUSTOM_WRITER_RUNNING:-0} == 1 && -n ${CXA_TEST_CUSTOM_WRITER_PATTERN:-} && "$*" == *"$CXA_TEST_CUSTOM_WRITER_PATTERN"* ]]; then
  exit 0
fi
[[ ${CXA_TEST_WRITER_RUNNING:-0} == 1 ]]
"#,
        );
        write_executable(&bin.join("systemctl"), "#!/usr/bin/env bash\nexit 0\n");
        write_executable(&bin.join("tmux"), "#!/usr/bin/env bash\nexit 1\n");
        write_executable(
            &bin.join("codex"),
            r#"#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == login ]]; then
  [[ ${CXA_TEST_REQUIRE_ACTIVE_MISSING:-0} != 1 || ! -e $CXA_ACTIVE_AUTH ]]
  [[ ${CXA_TEST_REQUIRE_ACTIVE_PRESENT:-0} != 1 || -e $CXA_ACTIVE_AUTH ]]
  cp "$CXA_TEST_LOGIN_AUTH" "$CODEX_HOME/auth.json"
  exit "${CXA_TEST_LOGIN_EXIT:-0}"
fi
if [[ ${1:-} == logout ]]; then
  [[ ${CXA_TEST_REQUIRE_FILE_LOGOUT:-0} != 1 || "$*" == *'cli_auth_credentials_store="file"'* ]]
  [[ -z ${CXA_TEST_LOGOUT_MARKER:-} ]] || touch "$CXA_TEST_LOGOUT_MARKER"
  exit "${CXA_TEST_LOGOUT_EXIT:-0}"
fi
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":0,"result":{"serverInfo":{"name":"fake","version":"1"}}}'
      ;;
    *'"method":"initialized"'*) ;;
    *'"method":"account/read"'*)
      [[ ${CXA_TEST_REQUIRE_ACTIVE_MISSING:-0} != 1 || ! -e $CXA_ACTIVE_AUTH ]]
      if [[ $line == *'"refreshToken":true'* && -n ${CXA_TEST_REFRESH_AUTH:-} ]]; then
        [[ -z ${CXA_TEST_REFRESH_MARKER:-} ]] || touch "$CXA_TEST_REFRESH_MARKER"
        cp "$CXA_TEST_REFRESH_AUTH" "$CODEX_HOME/auth.json"
      fi
      printf '%s\n' '{"id":1,"result":{"account":{"type":"chatgpt"},"requiresOpenaiAuth":true}}'
      ;;
    *'"method":"account/rateLimits/read"'*)
      if [[ ${CXA_TEST_ROTATE_ON_RATE_LIMIT:-0} == 1 && ${CXA_TEST_DELAYED_ROTATION:-0} != 1 && -n ${CXA_TEST_REFRESH_AUTH:-} ]]; then
        cp "$CXA_TEST_REFRESH_AUTH" "$CODEX_HOME/auth.json"
      fi
      if [[ ${CXA_TEST_RATE_LIMIT_FAIL:-0} == 1 ]]; then
        printf '%s\n' '{"id":2,"error":{"code":-32603,"message":"failed"}}'
      else
        printf '%s\n' '{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":25,"resetsAt":4102444800,"windowDurationMins":300},"secondary":{"usedPercent":75,"resetsAt":4102448400,"windowDurationMins":10080},"individualLimit":{"usedPercent":100,"resetsAt":4102452000},"credits":{"hasCredits":false,"unlimited":false,"balance":"0"},"planType":"team","rateLimitReachedType":"workspace_member_usage_limit_reached","spendControlReached":true}}}'
      fi
      if [[ ${CXA_TEST_KILL_PARENT_AFTER_RATE_LIMIT:-0} == 1 ]]; then
        kill -KILL "$PPID"
        if [[ ${CXA_TEST_DELAYED_ROTATION:-0} == 1 ]]; then
          exec >/dev/null 2>&1
          touch "$CXA_TEST_WRITER_STARTED"
          sleep 1
          cp "$CXA_TEST_REFRESH_AUTH" "$CODEX_HOME/auth.json"
          touch "$CXA_TEST_WRITER_FINISHED"
        fi
      fi
      ;;
  esac
done
"#,
        );
        Self {
            root,
            bin,
            codex_home,
            active_auth,
            store,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cxa"));
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .env("PATH", path)
            .env("CODEX_HOME", &self.codex_home)
            .env("CXA_CODEX_HOME", &self.codex_home)
            .env("CXA_ACTIVE_AUTH", &self.active_auth)
            .env("CXA_ACCOUNT_STORE", &self.store)
            .env(
                "CXA_SHARED_APP_SERVER_SOCKET",
                self.root.path().join("missing.sock"),
            )
            .env("CXA_SKIP_USAGE_REFRESH", "1")
            .env("CXA_TEST_PGREP_BACKEND", "1")
            .env("CXA_TEST_WRITER_RUNNING", "0");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn profile(&self, slot: u32) -> PathBuf {
        self.store.join(format!("profile-{slot}"))
    }

    fn enroll(&self, slot: u32, email: &str, token: &str, account: &str, user: &str) {
        let profile = self.profile(slot);
        fs::create_dir_all(&profile).unwrap();
        write_auth(&profile.join("auth.json"), email, token, account, user, 1);
    }

    fn select(&self, slot: u32) {
        fs::write(self.store.join("active-profile"), format!("{slot}\n")).unwrap();
        fs::copy(self.profile(slot).join("auth.json"), &self.active_auth).unwrap();
        let session = self.codex_home.join("auth.json");
        let _ = fs::remove_file(&session);
        symlink(&self.active_auth, session).unwrap();
    }
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_auth(
    path: &Path,
    email: &str,
    token: &str,
    account_id: &str,
    user_id: &str,
    refresh_second: u32,
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
        "last_refresh": format!("2026-01-01T00:00:{refresh_second:02}Z"),
        "tokens": {
            "id_token": id_token,
            "access_token": token,
            "refresh_token": format!("refresh-{token}"),
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

fn write_refresh_transaction(
    case: &Case,
    slot: Option<u32>,
    source: &Path,
    activate: bool,
    link_session: bool,
    hold_active: bool,
) {
    if let Some(directory) = source.parent() {
        let generated = directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".quota-") || name.starts_with(".enroll-"));
        if generated {
            fs::write(directory.join(".cxa-owned-temp"), b"cxa\n").unwrap();
        }
    }
    fs::write(
        case.store.join(".auth-transaction.json"),
        serde_json::to_vec(&json!({
            "mode": "refreshing",
            "slot": slot,
            "activate": activate,
            "select": false,
            "link_session": link_session,
            "hold_active": hold_active,
            "profile_pending": null,
            "recovery_source": source,
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn multi_account_list_explains_read_only_usage_refresh() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    fs::write(
        case.profile(2).join("usage.json"),
        serde_json::to_vec(&json!({
            "observed_at": 1,
            "primary_window": {"used_percent": 70},
            "reached": false
        }))
        .unwrap(),
    )
    .unwrap();

    let output = case.run(&["list"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("two@example.com"));
    assert!(stdout.contains("primary 70% used"));
    assert!(stdout.contains(
        "Usage refreshes without switching; relogin an account if its saved access token has expired."
    ));
}

#[test]
fn list_adopts_the_matching_detached_live_session_without_changing_it() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "two@example.com",
        "live-two",
        "account-two",
        "user-two",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .arg("list")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("* 2  two@example.com"));
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "2\n"
    );
    assert_eq!(access_token(&session), "live-two");
    assert!(!case.active_auth.exists());
}

#[test]
fn list_keeps_cached_live_quota_and_refreshes_inactive_accounts_while_codex_runs() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .arg("list")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("primary 25% used").count(), 1);
    assert!(!case.profile(1).join("usage.json").is_file());
    assert!(case.profile(2).join("usage.json").is_file());
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "two");
    assert_eq!(access_token(&case.active_auth), "one");
}

#[test]
fn list_does_not_start_a_second_refresh_owner_for_the_live_account() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let refreshed = case.root.path().join("refreshed.json");
    write_auth(
        &refreshed,
        "one@example.com",
        "rotated",
        "account-one",
        "user-one",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .env("CXA_TEST_REFRESH_AUTH", &refreshed)
        .env("CXA_TEST_ROTATE_ON_RATE_LIMIT", "1")
        .arg("list")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert_eq!(access_token(&case.active_auth), "one");
    assert!(!case.profile(1).join("usage.json").is_file());
}

#[test]
fn detached_live_account_never_uses_the_active_accounts_shared_server() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    fs::remove_file(&session).unwrap();
    fs::copy(case.profile(2).join("auth.json"), &session).unwrap();
    let unavailable_shared_socket = case.root.path().join("shared.sock");
    fs::write(&unavailable_shared_socket, b"not a socket").unwrap();

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .env("CXA_SHARED_APP_SERVER_SOCKET", &unavailable_shared_socket)
        .arg("list")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!case.profile(2).join("usage.json").exists());
}

#[test]
fn read_only_quota_refresh_preserves_credentials_rotated_by_codex() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let refreshed = case.root.path().join("refreshed.json");
    write_auth(
        &refreshed,
        "two@example.com",
        "rotated",
        "account-two",
        "user-two",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .env("CXA_TEST_REFRESH_AUTH", &refreshed)
        .env("CXA_TEST_ROTATE_ON_RATE_LIMIT", "1")
        .arg("list")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "rotated");
    assert_eq!(access_token(&case.active_auth), "one");
}

#[test]
fn interrupted_read_only_quota_rotation_recovers_before_orphan_cleanup() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let refreshed = case.root.path().join("refreshed.json");
    write_auth(
        &refreshed,
        "two@example.com",
        "rotated",
        "account-two",
        "user-two",
        2,
    );

    let interrupted = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .env("CXA_TEST_REFRESH_AUTH", &refreshed)
        .env("CXA_TEST_ROTATE_ON_RATE_LIMIT", "1")
        .env("CXA_TEST_KILL_PARENT_AFTER_RATE_LIMIT", "1")
        .arg("list")
        .output()
        .unwrap();

    assert!(!interrupted.status.success());
    assert!(case.store.join(".auth-transaction.json").exists());
    assert!(case.store.read_dir().unwrap().flatten().any(|entry| {
        entry.file_name().to_string_lossy().starts_with(".quota-")
            && access_token(&entry.path().join("auth.json")) == "rotated"
    }));

    let recovery = case.run(&["status"]);
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "rotated");
    assert_eq!(access_token(&case.active_auth), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
    assert!(
        !case
            .store
            .read_dir()
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".quota-"))
    );
}

#[test]
fn recovery_waits_for_an_orphaned_staging_writer() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let refreshed = case.root.path().join("refreshed.json");
    let writer_started = case.root.path().join("writer-started");
    let writer_finished = case.root.path().join("writer-finished");
    write_auth(
        &refreshed,
        "two@example.com",
        "rotated",
        "account-two",
        "user-two",
        2,
    );

    let interrupted = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .env("CXA_TEST_REFRESH_AUTH", &refreshed)
        .env("CXA_TEST_ROTATE_ON_RATE_LIMIT", "1")
        .env("CXA_TEST_KILL_PARENT_AFTER_RATE_LIMIT", "1")
        .env("CXA_TEST_DELAYED_ROTATION", "1")
        .env("CXA_TEST_WRITER_STARTED", &writer_started)
        .env("CXA_TEST_WRITER_FINISHED", &writer_finished)
        .arg("list")
        .output()
        .unwrap();

    assert!(!interrupted.status.success());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !writer_started.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(writer_started.exists());

    let deferred = case.run(&["status"]);
    assert!(!deferred.status.success());
    assert!(
        String::from_utf8_lossy(&deferred.stderr).contains("recovery is waiting for Codex to exit"),
        "{}",
        String::from_utf8_lossy(&deferred.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let recovery = loop {
        let output = case.run(&["status"]);
        if output.status.success() {
            break output;
        }
        assert!(
            Instant::now() < deadline,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(writer_finished.exists());
    assert!(recovery.status.success());
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "rotated");
    assert_eq!(access_token(&case.active_auth), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn switches_active_credentials_selection_and_shared_link() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);

    let output = case.run(&["2"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "two");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "2\n"
    );
    assert_eq!(
        fs::read_link(case.codex_home.join("auth.json")).unwrap(),
        case.active_auth
    );
}

#[test]
fn a_running_writer_blocks_switch_without_changes() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .arg("2")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
}

#[test]
fn a_configured_custom_codex_writer_blocks_switch_without_changes() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let home = case.root.path().join("home");
    let service_env = home.join(".config/cxa/service.env");
    fs::create_dir_all(service_env.parent().unwrap()).unwrap();
    fs::write(
        &service_env,
        format!(
            "CODEX_HOME={}\nCXA_ACCOUNT_STORE={}\nCXA_CODEX_BIN=\"/opt/custom/codex-wrapper\"\n",
            serde_json::to_string(&case.codex_home.display().to_string()).unwrap(),
            serde_json::to_string(&case.store.display().to_string()).unwrap()
        ),
    )
    .unwrap();

    let output = case
        .command()
        .env("HOME", &home)
        .env("CODEX_HOME", &case.codex_home)
        .env_remove("CXA_CODEX_HOME")
        .env_remove("CXA_ACCOUNT_STORE")
        .env_remove("CXA_CODEX_BIN")
        .env("CXA_TEST_CUSTOM_WRITER_RUNNING", "1")
        .env(
            "CXA_TEST_CUSTOM_WRITER_PATTERN",
            "/opt/custom/codex-wrapper",
        )
        .arg("2")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
}

#[test]
fn empty_overrides_use_persisted_systemd_paths() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let home = case.root.path().join("home");
    let service_env = home.join(".config/cxa/service.env");
    fs::create_dir_all(service_env.parent().unwrap()).unwrap();
    fs::write(
        &service_env,
        format!(
            "CODEX_HOME={}\nCXA_ACCOUNT_STORE={}\nCXA_CODEX_BIN=\"/opt/custom/codex-wrapper\"\n",
            serde_json::to_string(&case.codex_home.display().to_string()).unwrap(),
            serde_json::to_string(&case.store.display().to_string()).unwrap()
        ),
    )
    .unwrap();

    let output = case
        .command()
        .env("HOME", &home)
        .env("CODEX_HOME", &case.codex_home)
        .env_remove("CXA_CODEX_HOME")
        .env("CXA_ACCOUNT_STORE", "")
        .env("CXA_CODEX_BIN", "")
        .env("CXA_TEST_CUSTOM_WRITER_RUNNING", "1")
        .env(
            "CXA_TEST_CUSTOM_WRITER_PATTERN",
            "/opt/custom/codex-wrapper",
        )
        .arg("2")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Codex is running"));
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
}

#[test]
fn relink_uses_persisted_systemd_paths_when_the_session_link_is_missing() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    fs::write(case.store.join("active-profile"), b"1\n").unwrap();
    let home = case.root.path().join("home");
    let service_auth = case.root.path().join("service/auth.json");
    let service_socket = case.root.path().join("service/app-server.sock");
    fs::create_dir_all(service_auth.parent().unwrap()).unwrap();
    fs::copy(case.profile(1).join("auth.json"), &service_auth).unwrap();
    let service_env = home.join(".config/cxa/service.env");
    fs::create_dir_all(service_env.parent().unwrap()).unwrap();
    fs::write(
        &service_env,
        format!(
            "CODEX_HOME={}\nCXA_ACCOUNT_STORE={}\nCXA_ACTIVE_AUTH={}\nCXA_SHARED_APP_SERVER_SOCKET={}\n",
            serde_json::to_string(&case.codex_home.display().to_string()).unwrap(),
            serde_json::to_string(&case.store.display().to_string()).unwrap(),
            serde_json::to_string(&service_auth.display().to_string()).unwrap(),
            serde_json::to_string(&service_socket.display().to_string()).unwrap(),
        ),
    )
    .unwrap();

    let output = case
        .command()
        .env("HOME", &home)
        .env("CODEX_HOME", &case.codex_home)
        .env_remove("CXA_CODEX_HOME")
        .env_remove("CXA_ACCOUNT_STORE")
        .env_remove("CXA_ACTIVE_AUTH")
        .env_remove("CXA_SHARED_APP_SERVER_SOCKET")
        .arg("relink")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_link(case.codex_home.join("auth.json")).unwrap(),
        service_auth
    );
}

#[test]
fn custom_persisted_codex_home_requires_a_matching_shell_override() {
    let case = Case::new();
    let home = case.root.path().join("home");
    let service_env = home.join(".config/cxa/service.env");
    fs::create_dir_all(service_env.parent().unwrap()).unwrap();
    fs::write(
        &service_env,
        format!(
            "CODEX_HOME={}\n",
            serde_json::to_string(&case.codex_home.display().to_string()).unwrap()
        ),
    )
    .unwrap();

    let output = case
        .command()
        .env("HOME", &home)
        .env_remove("CODEX_HOME")
        .env_remove("CXA_CODEX_HOME")
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("systemd service uses a custom CODEX_HOME"));
    assert!(stderr.contains("export that CODEX_HOME"));
}

#[test]
fn add_while_writer_running_enrolls_without_touching_active_auth() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "two@example.com",
        "two",
        "account-two",
        "user-two",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .env("CXA_TEST_REQUIRE_ACTIVE_PRESENT", "1")
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .arg("add")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "two");
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
}

#[test]
fn add_uses_the_configured_codex_executable() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "two@example.com",
        "two",
        "account-two",
        "user-two",
        2,
    );
    let custom_codex = case.bin.join("custom-codex");
    fs::rename(case.bin.join("codex"), &custom_codex).unwrap();
    write_executable(&case.bin.join("codex"), "#!/usr/bin/env bash\nexit 99\n");

    let output = case
        .command()
        .env("CXA_CODEX_BIN", &custom_codex)
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .arg("add")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "two");
}

#[test]
fn import_while_writer_running_copies_credentials_without_touching_the_source_or_active_auth() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let source = case.root.path().join("import.json");
    write_auth(
        &source,
        "two@example.com",
        "two",
        "account-two",
        "user-two",
        2,
    );
    let source_before = fs::read(&source).unwrap();

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .arg("import")
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "two");
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
}

#[test]
fn import_rejects_credentials_that_change_while_being_staged() {
    let case = Case::new();
    let source = case.root.path().join("changing-auth");
    write_auth(
        &source,
        "one@example.com",
        "one",
        "account-one",
        "user-one",
        1,
    );
    let valid = fs::read(&source).unwrap();
    fs::remove_file(&source).unwrap();
    mkfifo(&source, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    let writer_source = source.clone();
    let writer = thread::spawn(move || {
        for (index, bytes) in [valid, b"malformed".to_vec()].into_iter().enumerate() {
            let mut pipe = fs::OpenOptions::new()
                .write(true)
                .open(&writer_source)
                .unwrap();
            pipe.write_all(&bytes).unwrap();
            drop(pipe);
            if index == 0 {
                thread::sleep(Duration::from_millis(100));
            }
        }
    });

    let output = case.command().arg("import").arg(&source).output().unwrap();
    writer.join().unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("changed or became invalid while being staged"),
        "{stderr}"
    );
    assert!(!case.profile(1).exists());
    assert!(!case.store.join("profile-1.pending").exists());
}

#[test]
fn relogin_rejects_a_different_workspace() {
    let case = Case::new();
    case.enroll(1, "same@example.com", "one", "account-one", "user-one");
    case.select(1);
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "same@example.com",
        "wrong",
        "account-two",
        "user-one",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .arg("relogin")
        .arg("1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert_eq!(access_token(&case.active_auth), "one");
}

#[test]
fn failed_relogin_link_validation_preserves_credentials_for_revocation() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    fs::remove_file(&session).unwrap();
    write_auth(
        &session,
        "unknown@example.com",
        "unknown",
        "account-unknown",
        "user-unknown",
        1,
    );
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "one@example.com",
        "fresh-one",
        "account-one",
        "user-one",
        2,
    );

    let rejected = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .args(["relogin", "1"])
        .output()
        .unwrap();

    assert!(!rejected.status.success());
    let staged = case
        .store
        .read_dir()
        .unwrap()
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().starts_with(".enroll-"))
        .expect("failed relogin should preserve staged credentials")
        .path();
    assert_eq!(access_token(&staged.join("auth.json")), "fresh-one");
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");

    let logout_marker = case.root.path().join("logged-out");
    let retried = case
        .command()
        .env("CXA_TEST_LOGOUT_MARKER", &logout_marker)
        .env("CXA_TEST_REQUIRE_FILE_LOGOUT", "1")
        .arg("status")
        .output()
        .unwrap();
    assert!(!retried.status.success());
    assert!(
        String::from_utf8_lossy(&retried.stderr)
            .contains("Shared session credentials are not linked")
    );
    assert!(logout_marker.exists());
    assert!(!staged.exists());
}

#[test]
fn offline_quota_refresh_rotates_credentials_and_records_usage() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let refreshed = case.root.path().join("refreshed.json");
    write_auth(
        &refreshed,
        "one@example.com",
        "refreshed-one",
        "account-one",
        "user-one",
        2,
    );

    let output = case
        .command()
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .env("CXA_TEST_REFRESH_AUTH", &refreshed)
        .env("CXA_TEST_REQUIRE_ACTIVE_MISSING", "1")
        .arg("list")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        access_token(&case.profile(1).join("auth.json")),
        "refreshed-one"
    );
    assert_eq!(access_token(&case.active_auth), "refreshed-one");
    let usage: Value =
        serde_json::from_slice(&fs::read(case.profile(1).join("usage.json")).unwrap()).unwrap();
    assert_eq!(usage["primary_window"]["used_percent"], 25.0);
    assert_eq!(usage["secondary_window"]["used_percent"], 75.0);
    assert_eq!(usage["individual_window"]["used_percent"], 100.0);
}

#[test]
fn failed_quota_refresh_preserves_the_last_successful_snapshot() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let usage_path = case.profile(1).join("usage.json");
    fs::write(
        &usage_path,
        serde_json::to_vec(&json!({
            "observed_at": 1,
            "primary_window": {"used_percent": 42},
            "reached": false
        }))
        .unwrap(),
    )
    .unwrap();

    let output = case
        .command()
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .env("CXA_TEST_RATE_LIMIT_FAIL", "1")
        .arg("list")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("primary 42% used"));
    let usage: Value = serde_json::from_slice(&fs::read(usage_path).unwrap()).unwrap();
    assert!(usage.get("error").is_none());
}

#[test]
fn startup_recovers_a_rollback_transaction() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let hold = case.active_auth.with_extension("json.cxa-hold");
    fs::rename(&case.active_auth, &hold).unwrap();
    fs::write(
        case.store.join(".auth-transaction.json"),
        r#"{"mode":"rollback","slot":null,"activate":false,"select":false,"link_session":false,"profile_pending":null}"#,
    )
    .unwrap();

    let output = case.run(&["status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn startup_recovers_when_malformed_active_was_moved_before_hold_creation() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    fs::write(&case.active_auth, b"malformed").unwrap();
    let pending = case.active_auth.with_extension("json.cxa-pending");
    fs::rename(&case.active_auth, &pending).unwrap();
    fs::write(
        case.store.join(".auth-transaction.json"),
        serde_json::to_vec(&json!({
            "mode": "rollback",
            "slot": null,
            "activate": false,
            "select": false,
            "link_session": false,
            "hold_active": true,
            "profile_pending": null,
            "recovery_source": null,
        }))
        .unwrap(),
    )
    .unwrap();

    let output = case.run(&["status"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "one");
    assert!(!pending.exists());
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn startup_completes_a_durable_commit() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let hold = case.active_auth.with_extension("json.cxa-hold");
    fs::rename(&case.active_auth, &hold).unwrap();
    fs::write(
        case.store.join(".auth-transaction.json"),
        r#"{"mode":"commit","slot":2,"activate":true,"select":true,"link_session":true,"profile_pending":null}"#,
    )
    .unwrap();

    let output = case.run(&["status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "two");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "2\n"
    );
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn startup_replays_an_already_restored_inactive_commit_without_removing_active_auth() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "fresh-two", "account-two", "user-two");
    case.select(1);
    fs::write(
        case.store.join(".auth-transaction.json"),
        serde_json::to_vec(&json!({
            "mode": "commit",
            "slot": 2,
            "activate": false,
            "select": false,
            "link_session": true,
            "hold_active": true,
            "profile_pending": null,
            "recovery_source": null,
        }))
        .unwrap(),
    )
    .unwrap();

    let output = case.run(&["status"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        access_token(&case.profile(2).join("auth.json")),
        "fresh-two"
    );
    assert_eq!(
        fs::read_link(case.codex_home.join("auth.json")).unwrap(),
        case.active_auth
    );
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn startup_promotes_a_profile_staged_outside_the_profile_namespace() {
    let case = Case::new();
    let source = case.root.path().join("source.json");
    let pending = case.store.join(".profile-1.auth.cxa-pending");
    write_auth(
        &source,
        "one@example.com",
        "one",
        "account-one",
        "user-one",
        1,
    );
    fs::copy(&source, &pending).unwrap();
    fs::write(
        case.store.join(".auth-transaction.json"),
        serde_json::to_vec(&json!({
            "mode": "commit",
            "slot": 1,
            "activate": false,
            "select": true,
            "link_session": false,
            "hold_active": false,
            "profile_pending": pending,
            "recovery_source": source,
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(!case.profile(1).exists());

    let output = case.run(&["list"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn startup_recovers_rotated_credentials_from_an_interrupted_quota_refresh() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let home = case.store.join(".quota-crashed");
    let source = home.join("auth.json");
    write_auth(
        &source,
        "one@example.com",
        "rotated",
        "account-one",
        "user-one",
        2,
    );
    fs::rename(
        &case.active_auth,
        case.active_auth.with_extension("json.cxa-hold"),
    )
    .unwrap();
    write_refresh_transaction(&case, Some(1), &source, true, false, true);

    let output = case.run(&["status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "rotated");
    assert_eq!(access_token(&case.active_auth), "rotated");
    assert!(!case.store.join(".auth-transaction.json").exists());
    assert!(!home.exists());
}

#[test]
fn startup_finishes_an_interrupted_live_enrollment_without_switching_accounts() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let home = case.store.join(".enroll-crashed");
    let source = home.join("auth.json");
    write_auth(
        &source,
        "two@example.com",
        "two",
        "account-two",
        "user-two",
        2,
    );
    write_refresh_transaction(&case, None, &source, false, false, false);

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .arg("status")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "two");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
    assert!(!home.exists());
}

#[test]
fn status_rejects_malformed_active_credentials_and_switch_repairs_them() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    fs::write(&case.active_auth, b"not valid JSON").unwrap();

    let output = case.run(&["status"]);
    assert!(!output.status.success());
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");

    let repair = case.run(&["1"]);
    assert!(
        repair.status.success(),
        "{}",
        String::from_utf8_lossy(&repair.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "one");
}

#[test]
fn a_writer_starting_after_rotation_defers_then_recovers_the_commit() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let refreshed = case.root.path().join("refreshed.json");
    write_auth(
        &refreshed,
        "one@example.com",
        "rotated",
        "account-one",
        "user-one",
        2,
    );
    let count = case.root.path().join("pgrep-count");

    let output = case
        .command()
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .env("CXA_TEST_REFRESH_AUTH", &refreshed)
        .env("CXA_TEST_PGREP_COUNT_FILE", &count)
        .env("CXA_TEST_WRITER_ON_CALL", "6")
        .arg("list")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!case.active_auth.exists());
    assert!(case.store.join(".auth-transaction.json").exists());

    let recovery = case.run(&["status"]);
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "rotated");
    assert_eq!(access_token(&case.active_auth), "rotated");
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn failed_login_revokes_staged_credentials_and_restores_active_auth() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let login = case.root.path().join("login.json");
    let logout_marker = case.root.path().join("logged-out");
    write_auth(
        &login,
        "two@example.com",
        "two",
        "account-two",
        "user-two",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .env("CXA_TEST_LOGIN_EXIT", "1")
        .env("CXA_TEST_LOGOUT_MARKER", &logout_marker)
        .arg("add")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(logout_marker.exists());
    assert_eq!(access_token(&case.active_auth), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
    assert_eq!(
        case.store
            .read_dir()
            .unwrap()
            .flatten()
            .filter(|entry| { entry.file_name().to_string_lossy().starts_with(".enroll-") })
            .count(),
        0
    );
}

#[test]
fn status_reconciles_the_selected_slot_to_live_credentials() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    fs::copy(case.profile(2).join("auth.json"), &case.active_auth).unwrap();

    let output = case.run(&["status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Default Codex account: 2  two@example.com")
    );
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "2\n"
    );
}

#[test]
fn status_preserves_credentials_from_a_writer_starting_during_reconciliation() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "old", "account-one", "user-one");
    case.select(1);
    write_auth(
        &case.profile(1).join("auth.json"),
        "one@example.com",
        "stored-newer",
        "account-one",
        "user-one",
        2,
    );
    let rotated = case.root.path().join("rotated.json");
    write_auth(
        &rotated,
        "one@example.com",
        "writer-newest",
        "account-one",
        "user-one",
        3,
    );
    let count = case.root.path().join("pgrep-count");

    let first = case
        .command()
        .env("CXA_TEST_PGREP_COUNT_FILE", &count)
        .env("CXA_TEST_WRITER_ON_CALL", "2")
        .env("CXA_TEST_WRITE_ACTIVE_AUTH", &rotated)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let recovery = case.run(&["status"]);

    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(
        access_token(&case.profile(1).join("auth.json")),
        "writer-newest"
    );
    assert_eq!(access_token(&case.active_auth), "writer-newest");
}

#[test]
fn relogin_before_first_selection_activates_and_links_the_account() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "old", "account-one", "user-one");
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "one@example.com",
        "fresh",
        "account-one",
        "user-one",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .args(["relogin", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "fresh");
    assert_eq!(access_token(&case.active_auth), "fresh");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
    assert_eq!(
        fs::read_link(case.codex_home.join("auth.json")).unwrap(),
        case.active_auth
    );
}

#[test]
fn first_selection_relogin_rolls_back_when_session_link_creation_fails() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "one@example.com",
        "fresh-one",
        "account-one",
        "user-one",
        2,
    );
    write_auth(
        &case.codex_home.join("auth.json"),
        "one@example.com",
        "one",
        "account-one",
        "user-one",
        1,
    );
    fs::create_dir(case.codex_home.join("auth.json.cxa-link")).unwrap();

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .args(["relogin", "1"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert!(!case.active_auth.exists());
    assert!(!case.store.join("active-profile").exists());
    assert!(!case.store.join(".auth-transaction.json").exists());
    assert!(!case.store.join("profile-1/.auth.cxa-backup").exists());
    assert_eq!(access_token(&case.codex_home.join("auth.json")), "one");
    let staged = case
        .store
        .read_dir()
        .unwrap()
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().starts_with(".enroll-"))
        .expect("failed relogin should preserve staged credentials")
        .path();
    assert_eq!(access_token(&staged.join("auth.json")), "fresh-one");
}

#[test]
fn switch_rolls_back_when_session_link_creation_fails() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    fs::remove_file(case.codex_home.join("auth.json")).unwrap();
    write_auth(
        &case.codex_home.join("auth.json"),
        "one@example.com",
        "one",
        "account-one",
        "user-one",
        1,
    );
    fs::create_dir(case.codex_home.join("auth.json.cxa-link")).unwrap();

    let output = case.run(&["use", "2"]);

    assert!(!output.status.success());
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
    assert_eq!(access_token(&case.codex_home.join("auth.json")), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn switch_rollback_restores_the_original_session_symlink() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    let legacy_target = case.profile(1).join("auth.json");
    fs::remove_file(&session).unwrap();
    symlink(&legacy_target, &session).unwrap();
    fs::create_dir(case.codex_home.join("auth.json.cxa-link")).unwrap();

    let output = case.run(&["use", "2"]);

    assert!(!output.status.success());
    assert_eq!(fs::read_link(&session).unwrap(), legacy_target);
    assert_eq!(access_token(&session), "one");
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn status_reports_a_missing_session_link_as_unhealthy() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    fs::remove_file(case.codex_home.join("auth.json")).unwrap();

    let output = case.run(&["status"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Session credentials: MISSING"));
}

#[test]
fn status_accepts_a_matching_detached_session_while_codex_runs() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "one@example.com",
        "live-one",
        "account-one",
        "user-one",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .arg("status")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Default Codex account: 1  one@example.com"));
    assert!(stdout.contains("matches the selection; relink after Codex stops"));
    assert_eq!(access_token(&session), "live-one");
    assert!(!case.active_auth.exists());
}

#[test]
fn writer_detection_failure_blocks_a_switch_conservatively() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    write_executable(&case.bin.join("pgrep"), "#!/usr/bin/env bash\nexit 2\n");

    let output = case.run(&["2"]);
    assert!(!output.status.success());
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_switching_uses_proc_without_pgrep() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    write_executable(&case.bin.join("pgrep"), "#!/usr/bin/env bash\nexit 127\n");

    let output = case
        .command()
        .env_remove("CXA_TEST_PGREP_BACKEND")
        .arg("2")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "two");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_proc_detection_matches_a_configured_executable_started_by_basename() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let assistant = case.bin.join("assistant");
    fs::copy("/bin/sleep", &assistant).unwrap();
    fs::set_permissions(&assistant, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        case.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut writer = Command::new("assistant")
        .arg("10")
        .env("PATH", path)
        .spawn()
        .unwrap();

    let output = case
        .command()
        .env_remove("CXA_TEST_PGREP_BACKEND")
        .env("CXA_CODEX_BIN", &assistant)
        .arg("2")
        .output()
        .unwrap();

    writer.kill().unwrap();
    writer.wait().unwrap();
    assert!(!output.status.success());
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_detection_matches_a_configured_executable_started_by_basename() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let assistant = case.bin.join("assistant");
    fs::copy("/bin/sleep", &assistant).unwrap();
    fs::set_permissions(&assistant, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        case.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut writer = Command::new("assistant")
        .arg("10")
        .env("PATH", path)
        .spawn()
        .unwrap();

    let output = case
        .command()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env_remove("CXA_TEST_PGREP_BACKEND")
        .env("CXA_CODEX_BIN", &assistant)
        .arg("2")
        .output()
        .unwrap();

    writer.kill().unwrap();
    writer.wait().unwrap();
    assert!(!output.status.success());
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
}

#[test]
fn unknown_detached_credentials_are_preserved() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    fs::remove_file(&session).unwrap();
    write_auth(
        &session,
        "unknown@example.com",
        "detached",
        "account-unknown",
        "user-unknown",
        2,
    );

    let output = case.run(&["2"]);
    assert!(!output.status.success());
    assert!(!session.is_symlink());
    assert_eq!(access_token(&session), "detached");
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
    assert!(!case.store.join(".auth-transaction.json").exists());
    let next = case.run(&["list"]);
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
}

#[test]
fn duplicate_add_is_rejected_and_revoked() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let login = case.root.path().join("login.json");
    let logout_marker = case.root.path().join("logged-out");
    write_auth(
        &login,
        "one@example.com",
        "duplicate",
        "account-one",
        "user-one",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .env("CXA_TEST_LOGOUT_MARKER", &logout_marker)
        .arg("add")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(logout_marker.exists());
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
}

#[test]
fn same_email_accounts_in_different_workspaces_remain_distinct() {
    let case = Case::new();
    case.enroll(1, "same@example.com", "one", "workspace-one", "user-one");
    case.select(1);
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "same@example.com",
        "two",
        "workspace-two",
        "user-one",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .arg("add")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "two");
}

#[test]
fn non_oauth_login_modes_are_rejected_before_staging() {
    for option in [
        "--with-api-key",
        "--with-access-token",
        "--with-api-key=secret",
        "--with-access-token=secret",
    ] {
        let case = Case::new();
        let output = case.run(&["add", option]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("requires ChatGPT OAuth credentials")
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
        assert_eq!(
            case.store
                .read_dir()
                .unwrap()
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(".enroll-"))
                .count(),
            0
        );
    }
}

#[test]
fn failed_rejection_revocation_is_preserved_and_retried_by_service_guard() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "one@example.com",
        "duplicate",
        "account-one",
        "user-one",
        2,
    );

    let rejected = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .env("CXA_TEST_LOGOUT_EXIT", "1")
        .arg("add")
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let staged = case
        .store
        .read_dir()
        .unwrap()
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().starts_with(".enroll-"))
        .expect("failed revocation should preserve the staged home")
        .path();
    assert_eq!(access_token(&staged.join("auth.json")), "duplicate");

    let retried = case.run(&["service-guard"]);
    assert!(
        retried.status.success(),
        "{}",
        String::from_utf8_lossy(&retried.stderr)
    );
    assert!(!staged.exists());
    assert_eq!(access_token(&case.active_auth), "one");
}

#[test]
fn failed_profile_promotion_preserves_rotated_auth_for_recovery() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let refreshed = case.root.path().join("refreshed.json");
    write_auth(
        &refreshed,
        "one@example.com",
        "rotated",
        "account-one",
        "user-one",
        2,
    );
    let blocked_pending = case.store.join(".profile-1.auth.cxa-pending");
    fs::create_dir(&blocked_pending).unwrap();

    let output = case
        .command()
        .env("CXA_SKIP_USAGE_REFRESH", "0")
        .env("CXA_USAGE_TTL", "0")
        .env("CXA_TEST_REFRESH_AUTH", &refreshed)
        .arg("list")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!case.active_auth.exists());
    assert!(case.store.join(".auth-transaction.json").exists());
    assert!(case.store.read_dir().unwrap().flatten().any(|entry| {
        entry.file_name().to_string_lossy().starts_with(".quota-")
            && entry.path().join("auth.json").is_file()
    }));

    fs::remove_dir(&blocked_pending).unwrap();
    let recovery = case.run(&["status"]);
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "rotated");
    assert_eq!(access_token(&case.active_auth), "rotated");
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn bare_command_recommends_init_for_a_current_codex_login() {
    let case = Case::new();
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "current@example.com",
        "current",
        "account-current",
        "user-current",
        1,
    );

    let output = case.run(&[]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Found the current Codex login: current@example.com"));
    assert!(stderr.contains("Run: cxa init"));
    assert!(!case.profile(1).exists());
}

#[test]
fn init_requires_confirmation_when_stdin_is_not_interactive() {
    let case = Case::new();
    write_auth(
        &case.codex_home.join("auth.json"),
        "current@example.com",
        "current",
        "account-current",
        "user-current",
        1,
    );

    let output = case.run(&["init"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cxa init --yes"));
    assert!(!case.profile(1).exists());
}

#[test]
fn init_imports_selects_and_links_the_current_codex_login() {
    let case = Case::new();
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "current@example.com",
        "current",
        "account-current",
        "user-current",
        1,
    );

    let output = case.run(&["init", "--yes"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Imported current@example.com as account 1."));
    assert!(stdout.contains("Account 1 is now selected."));
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "current");
    assert_eq!(access_token(&case.active_auth), "current");
    assert_eq!(fs::read_link(&session).unwrap(), case.active_auth);
}

#[test]
fn init_leaves_a_running_codex_session_detached_and_is_idempotent() {
    let case = Case::new();
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "current@example.com",
        "current",
        "account-current",
        "user-current",
        1,
    );
    let before = fs::read(&session).unwrap();

    let output = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .args(["init", "--yes"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !fs::symlink_metadata(&session)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&session).unwrap(), before);
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "current");
    assert!(!case.active_auth.exists());
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );

    let repeated = case
        .command()
        .env("CXA_TEST_WRITER_RUNNING", "1")
        .args(["init", "--yes"])
        .output()
        .unwrap();
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert!(String::from_utf8_lossy(&repeated.stdout).contains("cxa is already initialized"));
    assert!(!case.profile(2).exists());
}

#[test]
fn init_treats_a_writer_starting_during_linking_as_detached() {
    let case = Case::new();
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "current@example.com",
        "current",
        "account-current",
        "user-current",
        1,
    );
    let count = case.root.path().join("pgrep-count");

    let output = case
        .command()
        .env("CXA_TEST_PGREP_COUNT_FILE", &count)
        .env("CXA_TEST_WRITER_ON_CALL", "3")
        .args(["init", "--yes"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Imported current@example.com as account 1."));
    assert!(stdout.contains("Codex session credentials are detached"));
    assert!(stdout.contains("cxa relink"));
    assert!(
        !fs::symlink_metadata(&session)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "current");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "1\n"
    );
    assert!(case.store.join(".auth-transaction.json").exists());

    let repeated = case.run(&["init", "--yes"]);
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    let repeated_stdout = String::from_utf8_lossy(&repeated.stdout);
    assert!(repeated_stdout.contains("cxa is already initialized"));
    assert!(repeated_stdout.contains("cxa relink"));

    let relink = case.run(&["relink"]);
    assert!(
        relink.status.success(),
        "{}",
        String::from_utf8_lossy(&relink.stderr)
    );
    assert_eq!(fs::read_link(&session).unwrap(), case.active_auth);
}

#[test]
fn init_explains_how_to_create_a_codex_login() {
    let case = Case::new();

    let output = case.run(&["init", "--yes"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No current Codex login was found."));
    assert!(stderr.contains("codex login"));
}

#[test]
fn terminal_colour_can_be_forced_for_status_and_errors() {
    let case = Case::new();
    write_auth(
        &case.codex_home.join("auth.json"),
        "current@example.com",
        "current",
        "account-current",
        "user-current",
        1,
    );

    let error = case
        .command()
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .output()
        .unwrap();
    assert!(!error.status.success());
    assert!(String::from_utf8_lossy(&error.stderr).contains("\u{1b}["));

    case.enroll(
        1,
        "current@example.com",
        "current",
        "account-current",
        "user-current",
    );
    case.select(1);
    let list = case
        .command()
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .arg("list")
        .output()
        .unwrap();
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("\u{1b}["));
}

#[test]
fn redirected_output_contains_no_colour_codes() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);

    let output = case.run(&["list"]);

    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\u{1b}["));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("\u{1b}["));
}

#[test]
fn malformed_profiles_still_reserve_their_slots() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    fs::create_dir_all(case.profile(2)).unwrap();
    fs::write(case.profile(2).join("auth.json"), b"malformed").unwrap();
    let source = case.root.path().join("three.json");
    write_auth(
        &source,
        "three@example.com",
        "three",
        "account-three",
        "user-three",
        2,
    );

    let output = case
        .command()
        .args(["import"])
        .arg(&source)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(case.profile(2).join("auth.json")).unwrap(),
        b"malformed"
    );
    assert_eq!(access_token(&case.profile(3).join("auth.json")), "three");
}

#[test]
fn relink_preserves_equal_timestamp_credential_conflicts() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "stored", "account-one", "user-one");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    fs::remove_file(&session).unwrap();
    write_auth(
        &session,
        "one@example.com",
        "detached",
        "account-one",
        "user-one",
        1,
    );

    let output = case.run(&["relink"]);

    assert!(!output.status.success());
    assert!(!session.is_symlink());
    assert_eq!(access_token(&session), "detached");
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "stored");
    assert!(!case.store.join(".auth-transaction.json").exists());
    let next = case.run(&["list"]);
    assert!(
        next.status.success(),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
}

#[test]
fn relink_promotes_newer_detached_credentials_to_the_active_file() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "stored", "account-one", "user-one");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    fs::remove_file(&session).unwrap();
    write_auth(
        &session,
        "one@example.com",
        "detached-newer",
        "account-one",
        "user-one",
        2,
    );

    let output = case.run(&["relink"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        access_token(&case.profile(1).join("auth.json")),
        "detached-newer"
    );
    assert_eq!(access_token(&case.active_auth), "detached-newer");
    assert_eq!(fs::read_link(&session).unwrap(), case.active_auth);
}

#[test]
fn switching_from_a_newer_detached_session_keeps_the_target_active() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "old-one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    fs::remove_file(&session).unwrap();
    write_auth(
        &session,
        "one@example.com",
        "new-one",
        "account-one",
        "user-one",
        2,
    );

    let output = case.run(&["2"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "new-one");
    assert_eq!(access_token(&case.active_auth), "two");
    assert_eq!(fs::read_link(&session).unwrap(), case.active_auth);
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "2\n"
    );
}

#[test]
fn relink_preserves_credentials_from_a_writer_starting_during_link_replacement() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "stored", "account-one", "user-one");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    fs::remove_file(&session).unwrap();
    write_auth(
        &session,
        "one@example.com",
        "stored",
        "account-one",
        "user-one",
        1,
    );
    let rotated = case.root.path().join("rotated.json");
    write_auth(
        &rotated,
        "one@example.com",
        "writer-newest",
        "account-one",
        "user-one",
        3,
    );
    let count = case.root.path().join("pgrep-count");

    let first = case
        .command()
        .env("CXA_TEST_PGREP_COUNT_FILE", &count)
        .env("CXA_TEST_WRITER_ON_CALL", "5")
        .env("CXA_TEST_WRITE_SESSION_AUTH", &rotated)
        .arg("relink")
        .output()
        .unwrap();
    assert!(!first.status.success());

    let recovery = case.run(&["status"]);

    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert_eq!(
        access_token(&case.profile(1).join("auth.json")),
        "writer-newest"
    );
    assert_eq!(access_token(&case.active_auth), "writer-newest");
    assert_eq!(fs::read_link(&session).unwrap(), case.active_auth);
}

#[test]
fn recovery_keeps_credentials_written_by_a_late_codex_process() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let source = case.store.join(".quota-crashed/auth.json");
    write_auth(
        &source,
        "one@example.com",
        "rotated",
        "account-one",
        "user-one",
        2,
    );
    fs::rename(
        &case.active_auth,
        case.active_auth.with_extension("json.cxa-hold"),
    )
    .unwrap();
    write_refresh_transaction(&case, Some(1), &source, true, false, true);
    write_auth(
        &case.active_auth,
        "changed@example.com",
        "late-writer",
        "account-one",
        "user-one",
        3,
    );

    let output = case.run(&["status"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        access_token(&case.profile(1).join("auth.json")),
        "late-writer"
    );
    assert_eq!(access_token(&case.active_auth), "late-writer");
}

#[test]
fn rollback_restores_the_newest_selected_credentials() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "old", "account-one", "user-one");
    case.select(1);
    fs::rename(
        &case.active_auth,
        case.active_auth.with_extension("json.cxa-hold"),
    )
    .unwrap();
    write_auth(
        &case.active_auth,
        "one@example.com",
        "late-newer",
        "account-one",
        "user-one",
        2,
    );
    fs::write(
        case.store.join(".auth-transaction.json"),
        serde_json::to_vec(&json!({
            "mode": "rollback",
            "slot": null,
            "activate": false,
            "select": false,
            "link_session": false,
            "hold_active": true,
            "profile_pending": null,
            "recovery_source": null,
        }))
        .unwrap(),
    )
    .unwrap();

    let output = case.run(&["status"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        access_token(&case.profile(1).join("auth.json")),
        "late-newer"
    );
    assert_eq!(access_token(&case.active_auth), "late-newer");
}

#[test]
fn status_promotes_newer_selected_profile_credentials_to_active_auth() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "old", "account-one", "user-one");
    case.select(1);
    write_auth(
        &case.profile(1).join("auth.json"),
        "one@example.com",
        "rotated",
        "account-one",
        "user-one",
        2,
    );

    let output = case.run(&["status"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "rotated");
}

#[test]
fn service_guard_rejects_unknown_writer_status() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    write_executable(&case.bin.join("pgrep"), "#!/usr/bin/env bash\nexit 2\n");

    let output = case.run(&["service-guard"]);

    assert!(!output.status.success());
    assert!(!case.store.join(".shared-app-server-starting").exists());
}

#[test]
fn relogin_accepts_an_email_change_for_the_same_account_ids() {
    let case = Case::new();
    case.enroll(1, "old@example.com", "old", "account-one", "user-one");
    case.select(1);
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "new@example.com",
        "fresh",
        "account-one",
        "user-one",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .args(["relogin", "1"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "fresh");
}

#[test]
fn relogin_uses_the_selection_reconciled_by_the_barrier() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    fs::copy(case.profile(2).join("auth.json"), &case.active_auth).unwrap();
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "one@example.com",
        "fresh-one",
        "account-one",
        "user-one",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .args(["relogin", "1"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        access_token(&case.profile(1).join("auth.json")),
        "fresh-one"
    );
    assert_eq!(access_token(&case.active_auth), "two");
    assert_eq!(
        fs::read_to_string(case.store.join("active-profile")).unwrap(),
        "2\n"
    );
}

#[test]
fn relative_persistent_paths_are_rejected() {
    let case = Case::new();

    let output = case
        .command()
        .env("CXA_ACCOUNT_STORE", "relative-store")
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("CXA_ACCOUNT_STORE must be an absolute path")
    );
}

#[test]
fn overlapping_persistent_paths_are_rejected() {
    let case = Case::new();

    for conflict in [
        case.store.join("active-profile"),
        case.store.join(".auth-transaction.json"),
    ] {
        let output = case
            .command()
            .env("CXA_ACTIVE_AUTH", conflict)
            .arg("status")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("must use different paths"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = case
        .command()
        .env("CXA_SHARED_APP_SERVER_SOCKET", &case.active_auth)
        .arg("status")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must use different paths"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn session_scratch_paths_are_reserved_before_init() {
    let case = Case::new();
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "current@example.com",
        "current",
        "account-current",
        "user-current",
        1,
    );
    let before = fs::read(&session).unwrap();

    for suffix in [".cxa-link", ".cxa-detached"] {
        let conflict = PathBuf::from(format!("{}{suffix}", session.display()));
        let output = case
            .command()
            .env("CXA_ACTIVE_AUTH", conflict)
            .args(["init", "--yes"])
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("must use different paths"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(&session).unwrap(), before);
        assert!(!case.profile(1).exists());
    }
}

#[test]
fn active_credentials_reject_a_directory_before_init_mutates_it() {
    let case = Case::new();
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "current@example.com",
        "current",
        "account-current",
        "user-current",
        1,
    );
    let active_directory = case.root.path().join("active-directory");
    fs::create_dir(&active_directory).unwrap();
    fs::write(active_directory.join("keep"), b"keep").unwrap();

    let output = case
        .command()
        .env("CXA_ACTIVE_AUTH", &active_directory)
        .args(["init", "--yes"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("regular credential file"));
    assert_eq!(fs::read(active_directory.join("keep")).unwrap(), b"keep");
    assert!(!case.profile(1).exists());
    assert!(!case.store.join(".auth-transaction.json").exists());
    assert!(!PathBuf::from(format!("{}.cxa-hold", active_directory.display())).exists());
}

#[test]
fn active_credentials_reject_a_future_profile_directory_before_init() {
    let case = Case::new();
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "current@example.com",
        "current",
        "account-current",
        "user-current",
        1,
    );
    let before = fs::read(&session).unwrap();
    let alias = case.root.path().join("store-alias");
    symlink(&case.store, &alias).unwrap();

    for conflict in [
        case.profile(1),
        case.profile(1).join("nested/auth.json"),
        alias.join("profile-1"),
        alias.join("profile-1/nested/auth.json"),
    ] {
        let output = case
            .command()
            .env("CXA_ACTIVE_AUTH", &conflict)
            .args(["init", "--yes"])
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("must not point at an enrolled profile"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(&session).unwrap(), before);
        assert!(!case.profile(1).exists());
        assert!(!case.store.join(".auth-transaction.json").exists());
    }
}

#[test]
fn session_credentials_reject_a_future_profile_directory() {
    let case = Case::new();
    let profile_home = case.profile(1);

    let output = case
        .command()
        .env("CODEX_HOME", &profile_home)
        .env("CXA_CODEX_HOME", &profile_home)
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("must not place session credentials inside an enrolled profile"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!profile_home.exists());
}

#[test]
fn unresolved_case_only_path_aliases_are_rejected_on_every_platform() {
    let case = Case::new();
    let account_store = case.root.path().join("Future-Store");
    let active_auth = case.root.path().join("future-store/ACTIVE-PROFILE");

    let output = case
        .command()
        .env("CXA_ACCOUNT_STORE", &account_store)
        .env("CXA_ACTIVE_AUTH", &active_auth)
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must use different paths"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!account_store.exists());
    assert!(!active_auth.parent().unwrap().exists());
}

#[test]
fn persistent_paths_reject_dynamic_profile_staging_files() {
    let case = Case::new();
    let staging = case.store.join(".profile-2.auth.cxa-pending");

    for variable in ["CXA_ACTIVE_AUTH", "CXA_SHARED_APP_SERVER_SOCKET"] {
        let output = case
            .command()
            .env(variable, &staging)
            .arg("status")
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("must not overlap cxa profile staging"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn dangling_account_store_alias_collisions_are_rejected_before_creation() {
    let case = Case::new();
    fs::remove_dir(&case.store).unwrap();
    let alias = case.root.path().join("store-alias");
    symlink(&case.store, &alias).unwrap();

    let output = case
        .command()
        .env("CXA_ACTIVE_AUTH", alias.join("active-profile"))
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must use different paths"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!case.store.exists());
}

#[test]
fn shared_socket_rejects_a_nested_future_profile_path() {
    let case = Case::new();
    let socket = case.profile(1).join("nested/app-server.sock");

    let output = case
        .command()
        .env("CXA_SHARED_APP_SERVER_SOCKET", socket)
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("must not point inside an enrolled profile"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_codex_home_is_used_without_a_cxa_override() {
    let case = Case::new();
    let native_home = case.root.path().join("native-codex");
    write_auth(
        &native_home.join("auth.json"),
        "native@example.com",
        "native",
        "account-native",
        "user-native",
        1,
    );

    let output = case
        .command()
        .env("HOME", case.root.path().join("plain-home"))
        .env_remove("CXA_CODEX_HOME")
        .env("CODEX_HOME", &native_home)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Found the current Codex login: native@example.com")
    );
}

#[test]
fn mismatched_codex_home_overrides_are_rejected() {
    let case = Case::new();

    let output = case
        .command()
        .env("CODEX_HOME", case.root.path().join("other-codex"))
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("CXA_CODEX_HOME must match CODEX_HOME")
    );
}

#[test]
fn cxa_codex_home_without_native_override_is_rejected() {
    let case = Case::new();

    let output = case
        .command()
        .env_remove("CODEX_HOME")
        .arg("status")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("CXA_CODEX_HOME requires CODEX_HOME"));
}

#[test]
fn informational_flags_do_not_require_configuration() {
    for flag in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cxa"))
            .env_remove("HOME")
            .arg(flag)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{flag}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn active_credentials_cannot_be_the_session_link_itself() {
    let case = Case::new();
    let session = case.codex_home.join("auth.json");
    write_auth(
        &session,
        "one@example.com",
        "one",
        "account-one",
        "user-one",
        1,
    );

    let output = case
        .command()
        .env("CXA_ACTIVE_AUTH", &session)
        .arg("init")
        .arg("--yes")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(session.is_file());
    assert_eq!(access_token(&session), "one");
}

#[test]
fn active_credentials_cannot_alias_the_session_file() {
    let case = Case::new();
    let session = case.codex_home.join("auth.json");
    let active_alias = case.root.path().join("active-alias.json");
    write_auth(
        &session,
        "one@example.com",
        "one",
        "account-one",
        "user-one",
        1,
    );
    symlink(&session, &active_alias).unwrap();

    let output = case
        .command()
        .env("CXA_ACTIVE_AUTH", &active_alias)
        .arg("init")
        .arg("--yes")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(active_alias.is_symlink());
    assert_eq!(access_token(&session), "one");
}

#[test]
fn custom_active_extension_does_not_overlap_transaction_artifacts() {
    let mut case = Case::new();
    case.active_auth = case.root.path().join("shared/account.prod");
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let unrelated = case.active_auth.with_extension("json.cxa-hold");
    fs::write(&unrelated, b"unrelated").unwrap();

    let output = case.run(&["2"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated");
    assert_eq!(access_token(&case.active_auth), "two");
}

#[test]
fn switching_preserves_a_symlink_backed_active_credential() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    fs::write(case.store.join("active-profile"), b"1\n").unwrap();
    let target = case.root.path().join("service/auth.json");
    let alias = case.root.path().join("active-auth.json");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    write_auth(
        &target,
        "one@example.com",
        "one",
        "account-one",
        "user-one",
        1,
    );
    symlink(&target, &alias).unwrap();

    let output = case
        .command()
        .env("CXA_ACTIVE_AUTH", &alias)
        .arg("2")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(alias.is_symlink());
    assert_eq!(access_token(&target), "two");
}

#[test]
fn relink_accepts_a_session_link_through_the_configured_active_alias() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    let active_alias = case.root.path().join("active-alias.json");
    fs::remove_file(&session).unwrap();
    symlink(&case.active_auth, &active_alias).unwrap();
    symlink(&active_alias, &session).unwrap();

    let output = case
        .command()
        .env("CXA_ACTIVE_AUTH", &active_alias)
        .arg("relink")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_link(&session).unwrap(), active_alias);
    assert_eq!(access_token(&case.active_auth), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn status_rejects_a_session_link_with_the_wrong_target_case() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    let wrong_case_target = case.active_auth.with_file_name("AUTH.JSON");
    fs::remove_file(&session).unwrap();
    symlink(&wrong_case_target, &session).unwrap();

    let output = case.run(&["status"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Shared session credentials are not linked")
    );
}

#[test]
fn relink_migrates_a_legacy_profile_session_link_to_the_shared_active_file() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let session = case.codex_home.join("auth.json");
    fs::remove_file(&session).unwrap();
    symlink(case.profile(1).join("auth.json"), &session).unwrap();

    let output = case.run(&["relink"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_link(&session).unwrap(), case.active_auth);
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert_eq!(access_token(&case.active_auth), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn relink_migrates_the_previous_default_active_credential() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let previous_active = case.active_auth.clone();
    let system_active = case.root.path().join("systemd/auth.json");
    let session = case.codex_home.join("auth.json");

    let output = case
        .command()
        .env("CXA_ACTIVE_AUTH", &system_active)
        .arg("relink")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&system_active), "one");
    assert_eq!(fs::read_link(&session).unwrap(), system_active);
    assert_eq!(access_token(&previous_active), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn recovery_resolves_a_dangling_symlink_backed_active_credential() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let alias = case.root.path().join("active-alias.json");
    symlink(&case.active_auth, &alias).unwrap();
    fs::rename(
        &case.active_auth,
        case.active_auth.with_extension("json.cxa-hold"),
    )
    .unwrap();
    fs::write(
        case.store.join(".auth-transaction.json"),
        r#"{"mode":"rollback","slot":null,"activate":false,"select":false,"link_session":false,"hold_active":true,"profile_pending":null}"#,
    )
    .unwrap();

    let output = case
        .command()
        .env("CXA_ACTIVE_AUTH", &alias)
        .arg("list")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(alias.is_symlink());
    assert_eq!(access_token(&case.active_auth), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
}

#[test]
fn stale_service_start_markers_do_not_block_switching() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    fs::write(case.store.join(".shared-app-server-starting"), b"0\n").unwrap();

    let output = case.run(&["1"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn inactive_relogin_restores_a_missing_selected_credential() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "old-two", "account-two", "user-two");
    case.select(1);
    fs::remove_file(&case.active_auth).unwrap();
    let login = case.root.path().join("login.json");
    write_auth(
        &login,
        "two@example.com",
        "fresh-two",
        "account-two",
        "user-two",
        2,
    );

    let output = case
        .command()
        .env("CXA_TEST_LOGIN_AUTH", &login)
        .args(["relogin", "2"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.active_auth), "one");
    assert_eq!(
        access_token(&case.profile(2).join("auth.json")),
        "fresh-two"
    );
}

#[test]
fn failed_rollback_keeps_its_recovery_transaction() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let count = case.root.path().join("pgrep-count");

    let output = case
        .command()
        .env("CXA_TEST_PGREP_COUNT_FILE", &count)
        .env("CXA_TEST_WRITER_ON_CALL", "4")
        .arg("2")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(case.store.join(".auth-transaction.json").exists());
    assert!(case.active_auth.with_extension("json.cxa-hold").exists());

    let recovery = case.run(&["status"]);
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert!(!case.store.join(".auth-transaction.json").exists());
    assert_eq!(access_token(&case.active_auth), "one");
}

#[test]
fn late_writer_during_barrier_setup_leaves_its_recovery_transaction() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.enroll(2, "two@example.com", "two", "account-two", "user-two");
    case.select(1);
    let count = case.root.path().join("pgrep-count");

    let output = case
        .command()
        .env("CXA_TEST_PGREP_COUNT_FILE", &count)
        .env("CXA_TEST_WRITER_ON_CALL", "3")
        .arg("2")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(case.store.join(".auth-transaction.json").exists());
    assert!(case.active_auth.with_extension("json.cxa-hold").exists());

    let recovery = case.run(&["status"]);
    assert!(
        recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&recovery.stderr)
    );
    assert!(!case.store.join(".auth-transaction.json").exists());
    assert_eq!(access_token(&case.active_auth), "one");
}

#[test]
fn interrupted_duplicate_add_does_not_replace_the_existing_profile() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let home = case.store.join(".enroll-crashed");
    let source = home.join("auth.json");
    write_auth(
        &source,
        "one@example.com",
        "duplicate",
        "account-one",
        "user-one",
        2,
    );
    write_refresh_transaction(&case, None, &source, false, false, false);

    let output = case.run(&["status"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert!(!case.store.join(".auth-transaction.json").exists());
    assert!(!home.exists());
}

#[test]
fn interrupted_wrong_account_relogin_is_revoked_during_recovery() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let home = case.store.join(".enroll-crashed");
    let source = home.join("auth.json");
    write_auth(
        &source,
        "wrong@example.com",
        "wrong",
        "account-wrong",
        "user-wrong",
        2,
    );
    fs::rename(
        &case.active_auth,
        case.active_auth.with_extension("json.cxa-hold"),
    )
    .unwrap();
    write_refresh_transaction(&case, Some(1), &source, true, true, true);
    let logout_marker = case.root.path().join("logged-out");

    let output = case
        .command()
        .env("CXA_TEST_LOGOUT_MARKER", &logout_marker)
        .env("CXA_TEST_REQUIRE_FILE_LOGOUT", "1")
        .arg("status")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(logout_marker.exists());
    assert!(!home.exists());
    assert_eq!(access_token(&case.profile(1).join("auth.json")), "one");
    assert_eq!(access_token(&case.active_auth), "one");
}

#[test]
fn import_preserves_unmarked_sources_with_reserved_prefixes() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
    case.select(1);
    let source = case.store.join(".quota-user/auth.json");
    write_auth(
        &source,
        "two@example.com",
        "two",
        "account-two",
        "user-two",
        2,
    );
    let before = fs::read(&source).unwrap();

    let output = case.command().arg("import").arg(&source).output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_eq!(access_token(&case.profile(2).join("auth.json")), "two");
}

#[test]
fn import_rejects_its_transaction_staging_path_without_removing_the_source() {
    let case = Case::new();
    let source = case.store.join(".profile-1.auth.cxa-pending");
    write_auth(
        &source,
        "one@example.com",
        "one",
        "account-one",
        "user-one",
        1,
    );
    let before = fs::read(&source).unwrap();

    let output = case.command().arg("import").arg(&source).output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("overlaps cxa transaction staging"));
    assert_eq!(fs::read(&source).unwrap(), before);
    assert!(!case.profile(1).exists());
}

#[test]
fn import_rejects_credentials_without_a_refresh_token() {
    let case = Case::new();
    let source = case.root.path().join("incomplete.json");
    write_auth(
        &source,
        "one@example.com",
        "one",
        "account-one",
        "user-one",
        1,
    );
    let mut auth: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
    auth["tokens"]
        .as_object_mut()
        .unwrap()
        .remove("refresh_token");
    fs::write(&source, serde_json::to_vec(&auth).unwrap()).unwrap();

    let output = case.command().arg("import").arg(&source).output().unwrap();

    assert!(!output.status.success());
    assert!(!case.profile(1).exists());
    assert!(source.exists());
}
