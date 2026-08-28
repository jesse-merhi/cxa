#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dry_run=0
login_user="${SUDO_USER:-${USER:-$(id -un)}}"

usage() {
  cat <<'EOF'
Usage: ./install.sh [--user USER] [--dry-run]

  --user      User who receives cxa.
  --dry-run   Print commands without changing files.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
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

current_user="$(id -un)"
login_home="$HOME"
if [[ "$login_user" != "$current_user" ]]; then
  if [[ "$(uname -s)" == "Darwin" ]]; then
    login_home="$(dscacheutil -q user -a name "$login_user" | awk '/^[[:space:]]*dir:/ {sub(/^[[:space:]]*dir:[[:space:]]*/, ""); print; exit}')"
  else
    login_home="$(getent passwd "$login_user" | cut -d: -f6)"
  fi
  [[ -n "$login_home" ]] || {
    printf 'Could not resolve the home directory for %s.\n' "$login_user" >&2
    exit 1
  }
  command -v sudo >/dev/null 2>&1 || {
    printf 'sudo is required when installing for another user.\n' >&2
    exit 1
  }
fi

if [[ ! -f "$repo_root/Cargo.toml" && -x "$repo_root/cxa" ]]; then
  cxa_binary="$repo_root/cxa"
else
  command -v cargo >/dev/null 2>&1 || {
    printf 'cargo is required; install Rust 1.85 or newer first.\n' >&2
    exit 1
  }
  host_target="$(rustc -vV | sed -n 's/^host: //p')"
  [[ -n "$host_target" ]] || {
    printf 'Could not determine the Rust host target.\n' >&2
    exit 1
  }
  build_dir="$repo_root/target/cxa-install"
  run cargo build --locked --release --manifest-path "$repo_root/Cargo.toml" \
    --target "$host_target" --target-dir "$build_dir" --bin cxa
  cxa_binary="$build_dir/$host_target/release/cxa"
fi

if [[ "$login_user" == "$current_user" ]]; then
  run mkdir -p "$login_home/.local/bin"
  run install -m 0755 "$cxa_binary" "$login_home/.local/bin/cxa"
else
  login_group="$(id -gn "$login_user")"
  run sudo install -d -m 0755 -o "$login_user" -g "$login_group" "$login_home/.local/bin"
  run sudo install -m 0755 -o "$login_user" -g "$login_group" \
    "$cxa_binary" "$login_home/.local/bin/cxa"
fi

printf 'Installed cxa at %s/.local/bin/cxa\n' "$login_home"
printf 'Next step: %s/.local/bin/cxa init\n' "$login_home"
