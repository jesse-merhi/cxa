#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
host_target="$(rustc -vV | sed -n 's/^host: //p')"
[[ -n "$host_target" ]] || {
  printf 'Could not determine the Rust host target.\n' >&2
  exit 1
}
check_target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
debug_binary_dir="$check_target_dir/$host_target/debug"
toolchain_cargo_home="${CARGO_HOME:-$HOME/.cargo}"
toolchain_rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"

cargo fmt -- --check
cargo clippy --locked --all-targets --target "$host_target" --target-dir "$check_target_dir" -- -D warnings
cargo test --locked --target "$host_target" --target-dir "$check_target_dir" --lib --bins
cargo test --locked --target "$host_target" --target-dir "$check_target_dir" --test cxa_cli -- --test-threads=1
cargo build --locked --target "$host_target" --target-dir "$check_target_dir" --bins
if command -v actionlint >/dev/null 2>&1; then
  actionlint .github/workflows/*.yml
fi
./scripts/check-homebrew-formula.sh
./scripts/check-release-workflow.sh
grep -Fq 'BindsTo=codex-shared-app-server@%i.service' systemd/codex-quota-proxy@.service
grep -Fq 'EnvironmentFile=/etc/cxa/%i.env' systemd/codex-shared-app-server@.service
grep -Fq 'Requires=cxa-service-guard@%i.service' systemd/codex-shared-app-server@.service
grep -Fq 'After=cxa-service-guard@%i.service' systemd/codex-shared-app-server@.service
grep -Fq 'EnvironmentFile=/etc/cxa/%i.env' systemd/cxa-service-guard@.service
grep -Fq 'RemainAfterExit=no' systemd/cxa-service-guard@.service
grep -Fq 'ExecStart=+/usr/local/libexec/codex-shared-socket recover-owner %i' \
  systemd/cxa-service-guard@.service
grep -Fq 'service-guard' systemd/cxa-service-guard@.service
if grep -Eq '^(PartOf=|RemainAfterExit=yes)' systemd/cxa-service-guard@.service; then
  printf 'credential guard must return to inactive so automatic restarts rerun it\n' >&2
  exit 1
fi
if grep -Fq 'BindReadOnlyPaths' systemd/cxa-service-guard@.service; then
  printf 'credential recovery must run outside the read-only auth mount\n' >&2
  exit 1
fi
if grep -Fq 'exec "$CXA_CLI_BIN" service-guard' systemd/codex-shared-app-server@.service; then
  printf 'credential recovery must not run inside the app-server unit namespace\n' >&2
  exit 1
fi
grep -Fq 'Environment=CODEX_REFRESH_TOKEN_URL_OVERRIDE=http://127.0.0.1:0/cxa-refresh-disabled' \
  systemd/codex-shared-app-server@.service
if grep -Fq 'BindReadOnlyPaths=-/var/lib/codex-auth/auth.json' \
  systemd/codex-shared-app-server@.service; then
  printf 'file-level auth bind mounts pin stale credential inodes\n' >&2
  exit 1
fi
if grep -Fq '%h' systemd/codex-shared-app-server@.service; then
  printf 'system service must not resolve paths through manager %%h\n' >&2
  exit 1
fi
grep -Fq "\${CXA_CODEX_BIN}" systemd/codex-shared-app-server@.service

archive_dir="$(mktemp -d)"
archive_cleanup() {
  rm -f -- "$archive_dir/release/cxa" "$archive_dir/release/install.sh"
  rmdir -- "$archive_dir/release" "$archive_dir"
}
trap archive_cleanup EXIT
mkdir "$archive_dir/release"
install -m 0755 "$debug_binary_dir/cxa" "$archive_dir/release/cxa"
install -m 0755 install.sh "$archive_dir/release/install.sh"
archive_output="$(
  CARGO_HOME="$toolchain_cargo_home" RUSTUP_HOME="$toolchain_rustup_home" \
    HOME="$archive_dir/home" "$archive_dir/release/install.sh" --dry-run
)"
grep -Fq "$archive_dir/release/cxa" <<<"$archive_output"
grep -Fq "Next step: $archive_dir/home/.local/bin/cxa init" <<<"$archive_output"
archive_cleanup
trap - EXIT

source_output="$(
  CARGO_HOME="$toolchain_cargo_home" RUSTUP_HOME="$toolchain_rustup_home" \
    CARGO_TARGET_DIR="$archive_dir/redirected" HOME="$archive_dir/home" \
    ./install.sh --dry-run
)"
grep -Fq -- "--target $host_target" <<<"$source_output"
grep -Fq -- "--target-dir $repo_root/target/cxa-install" <<<"$source_output"
grep -Fq "$repo_root/target/cxa-install/$host_target/release/cxa" <<<"$source_output"

if [[ "$(uname -s)" == "Linux" ]]; then
  systemd_dry_run="$(
    env -u USER -u SUDO_USER CODEX_HOME=/tmp/cxa-test-codex CXA_ACCOUNT_STORE=/tmp/cxa-test-store CXA_CODEX_BIN=/opt/codex/bin/codex ./install.sh --systemd --dry-run
  )"
  grep -Fq 'Service CODEX_HOME: /tmp/cxa-test-codex' <<<"$systemd_dry_run"
  grep -Fq 'Service CXA_ACCOUNT_STORE: /tmp/cxa-test-store' <<<"$systemd_dry_run"
  grep -Fq 'Service Codex binary: /opt/codex/bin/codex' <<<"$systemd_dry_run"
  grep -Fq "write: /etc/cxa/$(id -un).env" <<<"$systemd_dry_run"
  grep -Fq 'environment: CODEX_HOME="/tmp/cxa-test-codex"' <<<"$systemd_dry_run"
  grep -Fq 'environment: CXA_ACCOUNT_STORE="/tmp/cxa-test-store"' <<<"$systemd_dry_run"
  grep -Fq 'environment: CXA_CODEX_BIN="/opt/codex/bin/codex"' <<<"$systemd_dry_run"
  grep -Fq "environment: CXA_CLI_BIN=\"$HOME/.local/bin/cxa\"" <<<"$systemd_dry_run"
  grep -Fq '/etc/systemd/system/cxa-service-guard@.service' <<<"$systemd_dry_run"

  control_error="$(mktemp)"
  if env -u USER -u SUDO_USER CODEX_HOME=$'/tmp/cxa-test\tcontrol' CXA_CODEX_BIN=/opt/codex/bin/codex ./install.sh --systemd --dry-run >/dev/null 2>"$control_error"; then
    printf 'systemd installation accepted a control character in CODEX_HOME\n' >&2
    rm -f -- "$control_error"
    exit 1
  fi
  grep -Fq 'contain no control characters' "$control_error"
  rm -f -- "$control_error"

  store_error="$(mktemp)"
  if env -u USER -u SUDO_USER CXA_ACCOUNT_STORE=/var/lib/../lib/codex-auth CXA_CODEX_BIN=/opt/codex/bin/codex ./install.sh --systemd --dry-run >/dev/null 2>"$store_error"; then
    printf 'systemd installation accepted the published socket directory as its account store\n' >&2
    rm -f -- "$store_error"
    exit 1
  fi
  grep -Fq 'must stay outside the published socket directory' "$store_error"
  rm -f -- "$store_error"

  codex_home_error="$(mktemp)"
  if env -u USER -u SUDO_USER CODEX_HOME=/var/lib/../lib/codex-auth CXA_CODEX_BIN=/opt/codex/bin/codex ./install.sh --systemd --dry-run >/dev/null 2>"$codex_home_error"; then
    printf 'systemd installation accepted the published socket directory as CODEX_HOME\n' >&2
    rm -f -- "$codex_home_error"
    exit 1
  fi
  grep -Fq 'CODEX_HOME must stay outside the published socket directory' "$codex_home_error"
  rm -f -- "$codex_home_error"

  active_auth_error="$(mktemp)"
  if env -u USER -u SUDO_USER CXA_ACTIVE_AUTH=/tmp/cxa-active-auth CXA_CODEX_BIN=/opt/codex/bin/codex ./install.sh --systemd --dry-run >/dev/null 2>"$active_auth_error"; then
    printf 'systemd installation accepted a custom active credential path\n' >&2
    rm -f -- "$active_auth_error"
    exit 1
  fi
  grep -Fq 'Systemd requires CXA_ACTIVE_AUTH=/var/lib/codex-auth/auth.json' "$active_auth_error"
  rm -f -- "$active_auth_error"

  socket_error="$(mktemp)"
  if env -u USER -u SUDO_USER CXA_SHARED_APP_SERVER_SOCKET=/tmp/cxa-app-server.sock CXA_CODEX_BIN=/opt/codex/bin/codex ./install.sh --systemd --dry-run >/dev/null 2>"$socket_error"; then
    printf 'systemd installation accepted a custom shared app-server socket\n' >&2
    rm -f -- "$socket_error"
    exit 1
  fi
  grep -Fq 'Systemd requires CXA_SHARED_APP_SERVER_SOCKET=/var/lib/codex-auth/app-server.sock' "$socket_error"
  rm -f -- "$socket_error"
fi

if [[ "$(uname -s)" == "Linux" ]] && command -v systemd-analyze >/dev/null 2>&1; then
  verify_dir="$(mktemp -d)"
  cleanup() {
    rm -f -- "$verify_dir"/*
    rmdir -- "$verify_dir"
  }
  trap cleanup EXIT

  sed "s#/usr/local/libexec/codex-shared-socket#$debug_binary_dir/codex-shared-socket#g" \
    systemd/codex-shared-app-server@.service \
    >"$verify_dir/codex-shared-app-server@.service"
  sed "s#/usr/local/libexec/codex-shared-socket#$debug_binary_dir/codex-shared-socket#g" \
    systemd/cxa-service-guard@.service \
    >"$verify_dir/cxa-service-guard@.service"
  sed "s#/usr/local/libexec/codex-quota-proxy#$debug_binary_dir/codex-quota-proxy#g" \
    systemd/codex-quota-proxy@.service \
    >"$verify_dir/codex-quota-proxy@.service"
  cp systemd/codex-quota-proxy@.socket "$verify_dir/"
  systemd-analyze verify "$verify_dir"/*

  cleanup
  trap - EXIT
fi
