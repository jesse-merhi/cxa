#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 3 ]]; then
  echo "usage: $0 <vX.Y.Z tag> <SHA256SUMS> <output formula>" >&2
  exit 2
fi

tag="$1"
checksums_file="$2"
output_file="$3"

if [[ ! "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
  echo "error: release tag must look like v1.2.3: $tag" >&2
  exit 1
fi

if [[ ! -f "$checksums_file" ]]; then
  echo "error: checksum file does not exist: $checksums_file" >&2
  exit 1
fi

version="${BASH_REMATCH[1]}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
template="$repo_root/packaging/homebrew/cxa.rb.in"

checksum_for() {
  local asset="$1"
  local checksum
  checksum="$(awk -v asset="$asset" '$2 == asset { print $1 }' "$checksums_file")"

  if [[ ! "$checksum" =~ ^[0-9a-f]{64}$ ]]; then
    echo "error: expected one SHA-256 checksum for $asset" >&2
    exit 1
  fi

  printf '%s' "$checksum"
}

macos_aarch64_sha256="$(checksum_for cxa-macos-aarch64.tar.gz)"
macos_x86_64_sha256="$(checksum_for cxa-macos-x86_64.tar.gz)"
linux_aarch64_sha256="$(checksum_for cxa-linux-aarch64.tar.gz)"
linux_x86_64_sha256="$(checksum_for cxa-linux-x86_64.tar.gz)"

mkdir -p "$(dirname "$output_file")"
sed \
  -e "s/@VERSION@/$version/g" \
  -e "s/@MACOS_AARCH64_SHA256@/$macos_aarch64_sha256/g" \
  -e "s/@MACOS_X86_64_SHA256@/$macos_x86_64_sha256/g" \
  -e "s/@LINUX_AARCH64_SHA256@/$linux_aarch64_sha256/g" \
  -e "s/@LINUX_X86_64_SHA256@/$linux_x86_64_sha256/g" \
  "$template" >"$output_file"
