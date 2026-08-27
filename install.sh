#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install_systemd=0
dry_run=0
login_user="${SUDO_USER:-${USER:-}}"
if [[ -z "$login_user" ]]; then
  login_user="$(id -un)"
fi

usage() {
  cat <<'EOF'
Usage: ./install.sh [--systemd] [--user USER] [--dry-run]

  --systemd   Install the Linux helper binaries and system units.
  --user      Login user who receives cxa and owns the systemd service.
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

systemd_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '"%s"' "$value"
}

contains_control_character() {
  local LC_ALL=C
  [[ "$1" == *[$'\001'-$'\037'$'\177']* ]]
}

render_service_environment() {
  printf 'HOME=%s\n' "$(systemd_escape "$login_home")"
  printf 'PATH=%s\n' "$(systemd_escape "$login_home/.local/bin:/home/linuxbrew/.linuxbrew/bin:/usr/local/bin:/usr/bin:/bin")"
  printf 'CXA_CLI_BIN=%s\n' "$(systemd_escape "$login_home/.local/bin/cxa")"
  printf 'CODEX_HOME=%s\n' "$(systemd_escape "$service_codex_home")"
  printf 'CXA_ACCOUNT_STORE=%s\n' "$(systemd_escape "$service_account_store")"
  printf 'CXA_CODEX_BIN=%s\n' "$(systemd_escape "$service_codex_binary")"
  printf 'CXA_ACTIVE_AUTH=%s\n' "$(systemd_escape "$service_active_auth")"
  printf 'CXA_SHARED_APP_SERVER_SOCKET=%s\n' "$(systemd_escape "$service_app_server_socket")"
}

write_service_environment() {
  local user_target="$login_home/.config/cxa/service.env"
  local system_target="/etc/cxa/$login_user.env"
  local temporary
  temporary="$(mktemp)"
  render_service_environment >"$temporary"
  if [[ "$dry_run" == "1" ]]; then
    printf 'write: %s\n' "$user_target"
    printf 'write: %s\n' "$system_target"
    sed 's/^/environment: /' "$temporary"
    rm -f -- "$temporary"
    return
  fi
  if [[ "$login_user" == "$current_user" ]]; then
    install -d -m 0700 "$login_home/.config/cxa"
    install -m 0600 "$temporary" "$user_target"
  else
    sudo install -d -m 0700 -o "$login_user" -g "$login_group" "$login_home/.config/cxa"
    sudo install -m 0600 -o "$login_user" -g "$login_group" "$temporary" "$user_target"
  fi
  sudo install -d -m 0755 -o root -g root /etc/cxa
  sudo install -m 0600 -o root -g root "$temporary" "$system_target"
  rm -f -- "$temporary"
}

validate_service_state_owner() {
  local directory=/var/lib/codex-auth
  service_state_published=0
  [[ ! -e "$directory" ]] || [[ -d "$directory" && ! -L "$directory" ]] || {
    printf '%s must be a real directory.\n' "$directory" >&2
    exit 1
  }
  [[ ! -d "$directory" ]] && return
  local actual_gid actual_mode actual_uid expected_gid expected_uid
  actual_uid="$(stat -c %u "$directory")"
  actual_gid="$(stat -c %g "$directory")"
  actual_mode="$(stat -c %a "$directory")"
  expected_uid="$(id -u "$login_user")"
  expected_gid="$(getent group openclaw | cut -d: -f3)"
  if [[ "$actual_uid" == "$expected_uid" ]]; then
    return
  fi
  if [[ "$actual_uid" == "0" && "$actual_gid" == "$expected_gid" && "$actual_mode" == "2511" ]]; then
    local socket="$directory/app-server.sock"
    if [[ -S "$socket" ]] \
      && [[ "$(stat -c %u "$socket")" == "$expected_uid" ]] \
      && [[ "$(stat -c %a "$socket")" == "600" ]]; then
      service_state_published=1
      return
    fi
  fi
  printf '%s has unexpected owner or published state; refusing to transfer singleton service state to %s.\n' "$directory" "$login_user" >&2
  exit 1
}

current_user="$(id -un)"
login_home="$HOME"
login_group="$(id -gn "$login_user")"
if [[ "$login_user" != "$current_user" ]]; then
  if [[ "$(uname -s)" == "Darwin" ]]; then
    command -v dscacheutil >/dev/null 2>&1 || {
      printf 'dscacheutil is required when installing for another user on macOS.\n' >&2
      exit 1
    }
    login_home="$(dscacheutil -q user -a name "$login_user" | awk '
      /^[[:space:]]*dir:[[:space:]]*/ {
        sub(/^[[:space:]]*dir:[[:space:]]*/, "")
        print
        exit
      }
    ')"
  else
    command -v getent >/dev/null 2>&1 || {
      printf 'getent is required when installing for another user on Linux.\n' >&2
      exit 1
    }
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

if [[ "$install_systemd" == "1" && "$(uname -s)" != "Linux" ]]; then
  printf -- '--systemd is supported only on Linux.\n' >&2
  exit 1
fi

service_codex_home="${CODEX_HOME:-$login_home/.codex}"
service_account_store="${CXA_ACCOUNT_STORE:-$login_home/.codex-auth}"
service_active_auth="${CXA_ACTIVE_AUTH:-/var/lib/codex-auth/auth.json}"
service_app_server_socket="${CXA_SHARED_APP_SERVER_SOCKET:-/var/lib/codex-auth/app-server.sock}"
for service_path in "$login_home" "$service_codex_home" "$service_account_store" "$service_active_auth" "$service_app_server_socket"; do
  if [[ "$service_path" != /* ]] || contains_control_character "$service_path"; then
    printf 'Systemd service paths must be absolute and contain no control characters: %q\n' "$service_path" >&2
    exit 1
  fi
done
if [[ "$install_systemd" == "1" ]] \
  && [[ "$(realpath -m -- "$service_active_auth")" != "/var/lib/codex-auth/auth.json" ]]; then
  printf 'Systemd requires CXA_ACTIVE_AUTH=/var/lib/codex-auth/auth.json.\n' >&2
  exit 1
fi
if [[ "$install_systemd" == "1" ]] \
  && [[ "$(realpath -m -- "$service_app_server_socket")" != "/var/lib/codex-auth/app-server.sock" ]]; then
  printf 'Systemd requires CXA_SHARED_APP_SERVER_SOCKET=/var/lib/codex-auth/app-server.sock.\n' >&2
  exit 1
fi
if [[ "$install_systemd" == "1" ]]; then
  service_active_auth=/var/lib/codex-auth/auth.json
  service_app_server_socket=/var/lib/codex-auth/app-server.sock
fi
if [[ "$install_systemd" == "1" ]] \
  && [[ "$(realpath -m -- "$service_account_store")" == "/var/lib/codex-auth" ]]; then
  printf 'CXA_ACCOUNT_STORE must stay outside the published socket directory /var/lib/codex-auth.\n' >&2
  exit 1
fi
if [[ "$install_systemd" == "1" ]] \
  && [[ "$(realpath -m -- "$service_codex_home")" == "/var/lib/codex-auth" ]]; then
  printf 'CODEX_HOME must stay outside the published socket directory /var/lib/codex-auth.\n' >&2
  exit 1
fi
service_codex_binary="${CXA_CODEX_BIN:-}"
if [[ "$install_systemd" == "1" ]]; then
  service_state_published=0
  if [[ "$login_user" != "$current_user" && -z "$service_codex_binary" ]]; then
    printf 'CXA_CODEX_BIN is required when installing systemd for another user.\n' >&2
    exit 1
  fi
  if [[ -z "$service_codex_binary" ]]; then
    service_codex_binary="$(command -v codex || true)"
  fi
  if [[ "$service_codex_binary" != /* ]] || contains_control_character "$service_codex_binary"; then
    printf 'Install Codex first or set CXA_CODEX_BIN to its absolute path.\n' >&2
    exit 1
  fi
  if [[ "$dry_run" != "1" ]]; then
    command -v getent >/dev/null 2>&1 || {
      printf 'getent is required for systemd installation.\n' >&2
      exit 1
    }
    openclaw_gid="$(getent group openclaw | cut -d: -f3)"
    [[ -n "$openclaw_gid" ]] || {
      printf 'The openclaw group does not exist. Create it before installing systemd units.\n' >&2
      exit 1
    }
    login_group_ids=" $(id -G "$login_user") "
    if [[ "$login_group_ids" != *" $openclaw_gid "* ]]; then
      printf '%s must belong to the openclaw group before installing systemd units.\n' "$login_user" >&2
      exit 1
    fi
  fi
fi

if [[ -x "$repo_root/cxa" && ! -f "$repo_root/Cargo.toml" ]]; then
  cxa_binary="$repo_root/cxa"
  shared_socket_binary="$repo_root/codex-shared-socket"
  quota_proxy_binary="$repo_root/codex-quota-proxy"
else
  command -v cargo >/dev/null 2>&1 || {
    printf 'cargo is required; install Rust 1.85 or newer first.\n' >&2
    exit 1
  }
  command -v rustc >/dev/null 2>&1 || {
    printf 'rustc is required; install Rust 1.85 or newer first.\n' >&2
    exit 1
  }
  host_target="$(rustc -vV | sed -n 's/^host: //p')"
  [[ -n "$host_target" ]] || {
    printf 'Could not determine the Rust host target.\n' >&2
    exit 1
  }
  build_dir="$repo_root/target/cxa-install"
  if [[ "$install_systemd" == "1" ]]; then
    run cargo build --locked --release --manifest-path "$repo_root/Cargo.toml" \
      --target "$host_target" --target-dir "$build_dir" --bins
  else
    run cargo build --locked --release --manifest-path "$repo_root/Cargo.toml" \
      --target "$host_target" --target-dir "$build_dir" --bin cxa
  fi
  binary_dir="$build_dir/$host_target/release"
  cxa_binary="$binary_dir/cxa"
  shared_socket_binary="$binary_dir/codex-shared-socket"
  quota_proxy_binary="$binary_dir/codex-quota-proxy"
fi

if [[ "$login_user" == "$current_user" ]]; then
  run mkdir -p "$login_home/.local/bin"
  run install -m 0755 "$cxa_binary" "$login_home/.local/bin/cxa"
else
  run sudo install -d -m 0755 -o "$login_user" -g "$login_group" "$login_home/.local/bin"
  run sudo install -m 0755 -o "$login_user" -g "$login_group" \
    "$cxa_binary" "$login_home/.local/bin/cxa"
fi

if [[ "$install_systemd" == "1" ]]; then
  if [[ "$dry_run" != "1" ]]; then
    command -v sudo >/dev/null 2>&1 || {
      printf 'sudo is required for systemd installation.\n' >&2
      exit 1
    }
    sudo -u "$login_user" test -x "$service_codex_binary" || {
      printf 'Codex is not executable by %s at %s.\n' "$login_user" "$service_codex_binary" >&2
      exit 1
    }
    [[ -x "$shared_socket_binary" && -x "$quota_proxy_binary" ]] || {
      printf 'This archive does not contain the Linux systemd helper binaries.\n' >&2
      exit 1
    }
    validate_service_state_owner
  fi
  run sudo install -D -m 0755 "$shared_socket_binary" \
    /usr/local/libexec/codex-shared-socket
  run sudo install -D -m 0755 "$quota_proxy_binary" \
    /usr/local/libexec/codex-quota-proxy
  run sudo install -m 0644 "$repo_root/systemd/codex-shared-app-server@.service" \
    /etc/systemd/system/codex-shared-app-server@.service
  run sudo install -m 0644 "$repo_root/systemd/cxa-service-guard@.service" \
    /etc/systemd/system/cxa-service-guard@.service
  run sudo install -m 0644 "$repo_root/systemd/codex-quota-proxy@.service" \
    /etc/systemd/system/codex-quota-proxy@.service
  run sudo install -m 0644 "$repo_root/systemd/codex-quota-proxy@.socket" \
    /etc/systemd/system/codex-quota-proxy@.socket
  write_service_environment
  if [[ "$dry_run" == "1" || ! -d /var/lib/codex-auth ]]; then
    run sudo install -d -m 0700 -o "$login_user" -g openclaw /var/lib/codex-auth
  elif [[ "$service_state_published" == "0" ]]; then
    run sudo chmod 0700 /var/lib/codex-auth
    run sudo chgrp openclaw /var/lib/codex-auth
  else
    printf 'Preserving the live published state in /var/lib/codex-auth.\n'
  fi
  run sudo systemctl daemon-reload
  printf 'Installed systemd units. Run cxa init before enabling them.\n'
  printf 'Service CODEX_HOME: %s\n' "$service_codex_home"
  printf 'Service CXA_ACCOUNT_STORE: %s\n' "$service_account_store"
  printf 'Service Codex binary: %s\n' "$service_codex_binary"
fi

printf 'Installed cxa at %s/.local/bin/cxa\n' "$login_home"
printf 'Next step: %s/.local/bin/cxa init\n' "$login_home"
