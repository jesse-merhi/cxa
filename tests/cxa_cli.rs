use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
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
  [[ $count == ${CXA_TEST_WRITER_ON_CALL:-0} ]]
  exit
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
      if [[ -n ${CXA_TEST_REFRESH_AUTH:-} ]]; then
        cp "$CXA_TEST_REFRESH_AUTH" "$CODEX_HOME/auth.json"
      fi
      printf '%s\n' '{"id":1,"result":{"account":{"type":"chatgpt"},"requiresOpenaiAuth":true}}'
      ;;
    *'"method":"account/rateLimits/read"'*)
      if [[ ${CXA_TEST_RATE_LIMIT_FAIL:-0} == 1 ]]; then
        printf '%s\n' '{"id":2,"error":{"code":-32603,"message":"failed"}}'
      else
        printf '%s\n' '{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":25,"resetsAt":4102444800,"windowDurationMins":300},"secondary":{"usedPercent":75,"resetsAt":4102448400,"windowDurationMins":10080},"individualLimit":{"usedPercent":100,"resetsAt":4102452000},"credits":{"hasCredits":false,"unlimited":false,"balance":"0"},"planType":"team","rateLimitReachedType":"workspace_member_usage_limit_reached","spendControlReached":true}}}'
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
            .env("CXA_CODEX_HOME", &self.codex_home)
            .env("CXA_ACTIVE_AUTH", &self.active_auth)
            .env("CXA_ACCOUNT_STORE", &self.store)
            .env(
                "CXA_SHARED_APP_SERVER_SOCKET",
                self.root.path().join("missing.sock"),
            )
            .env("CXA_SKIP_USAGE_REFRESH", "1")
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
fn multi_account_list_explains_that_inactive_usage_is_cached() {
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
        "Usage refreshes only for the selected account; switch accounts to refresh another account's usage."
    ));
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
        .env("CXA_TEST_WRITER_ON_CALL", "5")
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

#[test]
fn unknown_detached_credentials_are_preserved() {
    let case = Case::new();
    case.enroll(1, "one@example.com", "one", "account-one", "user-one");
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

    let output = case.run(&["1"]);
    assert!(!output.status.success());
    assert!(!session.is_symlink());
    assert_eq!(access_token(&session), "detached");
    assert_eq!(access_token(&case.active_auth), "one");
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
    for option in ["--with-api-key", "--with-access-token"] {
        let case = Case::new();
        let output = case.run(&["add", option]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("requires ChatGPT OAuth credentials")
        );
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
fn failed_rejection_revocation_is_preserved_and_retried() {
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

    let retried = case.run(&["status"]);
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
    let blocked_pending = case.profile(1).join("auth.json.cxa-pending");
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
