#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
test_dir="$(mktemp -d)"

cleanup() {
  rm -f -- "$test_dir/SHA256SUMS" "$test_dir/cxa.rb"
  rmdir -- "$test_dir"
}
trap cleanup EXIT

checksums=(
  "$(printf '1%.0s' {1..64})  cxa-macos-aarch64.tar.gz"
  "$(printf '2%.0s' {1..64})  cxa-macos-x86_64.tar.gz"
  "$(printf '3%.0s' {1..64})  cxa-linux-aarch64.tar.gz"
  "$(printf '4%.0s' {1..64})  cxa-linux-x86_64.tar.gz"
)
printf '%s\n' "${checksums[@]}" >"$test_dir/SHA256SUMS"

"$repo_root/scripts/generate-homebrew-formula.sh" \
  v1.2.3 \
  "$test_dir/SHA256SUMS" \
  "$test_dir/cxa.rb"

grep -Fq 'version "1.2.3"' "$test_dir/cxa.rb"
grep -Fq '/releases/download/v1.2.3/cxa-macos-aarch64.tar.gz' "$test_dir/cxa.rb"
grep -Fq "$(printf '1%.0s' {1..64})" "$test_dir/cxa.rb"
grep -Fq "$(printf '4%.0s' {1..64})" "$test_dir/cxa.rb"

if grep -Fq '@' "$test_dir/cxa.rb"; then
  echo "error: generated formula still contains a template placeholder" >&2
  exit 1
fi

if "$repo_root/scripts/generate-homebrew-formula.sh" \
  invalid-tag \
  "$test_dir/SHA256SUMS" \
  "$test_dir/cxa.rb" 2>/dev/null; then
  echo "error: generator accepted an invalid release tag" >&2
  exit 1
fi
