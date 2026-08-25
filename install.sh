#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install_systemd=0
dry_run=0
login_user="${SUDO_USER:-$USER}"

usage() {
  cat <<'EOF'
Usage: ./install.sh [--systemd] [--user USER] [--dry-run]

  --systemd   Install the Linux helper binaries and system units.
  --user      Login user used by the systemd service examples.
  --dry-run   Print commands without changing files.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --systemd)
      install_systemd=1
      ;;
    --user)
      shift
      [[ $# -gt 0 ]] || { usage >&2; exit 2; }
      login_user="$1"
      ;;
    --dry-run)
      dry_run=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'Unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

run() {
  if [[ "$dry_run" == "1" ]]; then
    printf 'run:'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

command -v cargo >/dev/null 2>&1 || {
  printf 'cargo is required; install Rust 1.85 or newer first.\n' >&2
  exit 1
}

if [[ "$install_systemd" == "1" && "$(uname -s)" != "Linux" ]]; then
  printf -- '--systemd is supported only on Linux.\n' >&2
  exit 1
fi

if [[ "$install_systemd" == "1" ]]; then
  run cargo build --locked --release --manifest-path "$repo_root/Cargo.toml" --bins
else
  run cargo build --locked --release --manifest-path "$repo_root/Cargo.toml" --bin cxa
fi

run mkdir -p "$HOME/.local/bin"
run install -m 0755 "$repo_root/target/release/cxa" "$HOME/.local/bin/cxa"

if [[ "$install_systemd" == "1" ]]; then
  command -v sudo >/dev/null 2>&1 || {
    printf 'sudo is required for systemd installation.\n' >&2
    exit 1
  }
  getent group openclaw >/dev/null 2>&1 || {
    printf 'The openclaw group does not exist. Create it before installing systemd units.\n' >&2
    exit 1
  }
  run sudo install -D -m 0755 "$repo_root/target/release/codex-shared-socket" \
    /usr/local/libexec/codex-shared-socket
  run sudo install -D -m 0755 "$repo_root/target/release/codex-quota-proxy" \
    /usr/local/libexec/codex-quota-proxy
  run sudo install -m 0644 "$repo_root/systemd/codex-shared-app-server@.service" \
    /etc/systemd/system/codex-shared-app-server@.service
  run sudo install -m 0644 "$repo_root/systemd/codex-quota-proxy@.service" \
    /etc/systemd/system/codex-quota-proxy@.service
  run sudo install -m 0644 "$repo_root/systemd/codex-quota-proxy@.socket" \
    /etc/systemd/system/codex-quota-proxy@.socket
  run sudo install -d -m 0700 -o "$login_user" -g openclaw /var/lib/codex-auth
  run sudo systemctl daemon-reload
  printf 'Installed systemd units. Run cxa init before enabling them.\n'
fi

printf 'Installed cxa at %s/.local/bin/cxa\n' "$HOME"
printf 'Next step: cxa init\n'
