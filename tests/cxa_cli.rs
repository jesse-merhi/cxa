use std::fs::{self, File};
use std::io::Write as _;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
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
