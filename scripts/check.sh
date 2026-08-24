#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --bins

if [[ "$(uname -s)" == "Linux" ]] && command -v systemd-analyze >/dev/null 2>&1; then
  verify_dir="$(mktemp -d)"
  cleanup() {
    rm -f -- "$verify_dir"/*
    rmdir -- "$verify_dir"
  }
  trap cleanup EXIT

  sed "s#/usr/local/libexec/codex-shared-socket#$repo_root/target/debug/codex-shared-socket#g" \
    systemd/codex-shared-app-server@.service \
    >"$verify_dir/codex-shared-app-server@.service"
  sed "s#/usr/local/libexec/codex-quota-proxy#$repo_root/target/debug/codex-quota-proxy#g" \
    systemd/codex-quota-proxy@.service \
    >"$verify_dir/codex-quota-proxy@.service"
  cp systemd/codex-quota-proxy@.socket "$verify_dir/"
  systemd-analyze verify "$verify_dir"/*

  cleanup
  trap - EXIT
fi
