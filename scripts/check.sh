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
debug_binary="$check_target_dir/$host_target/debug/cxa"
toolchain_cargo_home="${CARGO_HOME:-$HOME/.cargo}"
toolchain_rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"

cargo fmt -- --check
cargo clippy --locked --all-targets --target "$host_target" --target-dir "$check_target_dir" -- -D warnings
cargo test --locked --target "$host_target" --target-dir "$check_target_dir" --lib --bins
cargo test --locked --target "$host_target" --target-dir "$check_target_dir" --test cxa_cli -- --test-threads=1
cargo build --locked --target "$host_target" --target-dir "$check_target_dir" --bin cxa
if command -v actionlint >/dev/null 2>&1; then
  actionlint .github/workflows/*.yml
fi
./scripts/check-homebrew-formula.sh
./scripts/check-release-workflow.sh

archive_dir="$(mktemp -d)"
cleanup() {
  rm -f -- "$archive_dir/release/cxa" "$archive_dir/release/install.sh"
  rmdir -- "$archive_dir/release" "$archive_dir"
}
trap cleanup EXIT
mkdir "$archive_dir/release"
install -m 0755 "$debug_binary" "$archive_dir/release/cxa"
install -m 0755 install.sh "$archive_dir/release/install.sh"
archive_output="$(
  CARGO_HOME="$toolchain_cargo_home" RUSTUP_HOME="$toolchain_rustup_home" \
    HOME="$archive_dir/home" "$archive_dir/release/install.sh" --dry-run
)"
grep -Fq "$archive_dir/release/cxa" <<<"$archive_output"
grep -Fq "Next step: $archive_dir/home/.local/bin/cxa init" <<<"$archive_output"
cleanup
trap - EXIT

source_output="$(
  CARGO_HOME="$toolchain_cargo_home" RUSTUP_HOME="$toolchain_rustup_home" \
    CARGO_TARGET_DIR="$archive_dir/redirected" HOME="$archive_dir/home" \
    ./install.sh --dry-run
)"
grep -Fq -- "--target $host_target" <<<"$source_output"
grep -Fq -- "--target-dir $repo_root/target/cxa-install" <<<"$source_output"
grep -Fq "$repo_root/target/cxa-install/$host_target/release/cxa" <<<"$source_output"
