#!/usr/bin/env bash
set -euo pipefail

ruby -ryaml <<'RUBY'
workflow = YAML.load_file(".github/workflows/release.yml")
jobs = workflow.fetch("jobs")

def assert_contract(condition, message)
  abort("release workflow: #{message}") unless condition
end

def scripts(job)
  job.fetch("steps").map { |step| step["run"] }.compact.join("\n")
end

def named_step(job, name)
  job.fetch("steps").find { |step| step["name"] == name } || {}
end

assert_contract(workflow.dig("concurrency", "queue") == "max", "release runs must queue")

tag_verification = named_step(jobs.fetch("verify"), "Verify tag matches Cargo version").fetch("run", "")
assert_contract(
  tag_verification.include?('[[ "$tag_version" != "$version" ]]'),
  "release verification must require the full Cargo version"
)
assert_contract(
  tag_verification.include?('[[ "$tag_version" == *+* ]]'),
  "release verification must reject build metadata before publishing"
)

linux_build = jobs.fetch("build-linux")
assert_contract(linux_build["runs-on"] == "ubuntu-22.04", "Linux archives must target Ubuntu 22.04")
assert_contract(
  scripts(linux_build).include?('cargo build --locked --release --target "$TARGET" --bin cxa'),
  "Linux release must build cxa"
)

linux_validation = jobs.fetch("validate-linux")
assert_contract(linux_validation["needs"] == "build-linux", "Linux validation must consume Linux builds")
assert_contract(
  linux_validation.dig("container", "image") == "ubuntu:22.04@sha256:2edbbc5dc405e9612ba3584ce95480277e3eb374407b5505fe26f17df77c7dbc",
  "Linux validation image must stay pinned"
)
assert_contract(
  scripts(linux_validation).include?('"dist/$ASSET/cxa" --version'),
  "Linux archives must run on the oldest supported runtime"
)

mac_build = jobs.fetch("build-macos")
mac_assets = mac_build.dig("strategy", "matrix", "include").map { |entry| entry.fetch("asset") }
assert_contract(
  mac_assets.sort == %w[cxa-macos-aarch64 cxa-macos-x86_64],
  "both macOS architectures must be built"
)

mac_validation = jobs.fetch("validate-macos")
validated_mac_assets = mac_validation.dig("strategy", "matrix", "include").map { |entry| entry.fetch("asset") }
assert_contract(mac_validation["needs"] == "build-macos", "macOS validation must consume macOS builds")
assert_contract(validated_mac_assets.sort == mac_assets.sort, "every macOS archive must be validated")
assert_contract(
  scripts(mac_validation).include?('"$archive_root/$ASSET/cxa" --version'),
  "macOS validation must execute the archived binary"
)
assert_contract(
  scripts(mac_validation).include?('"$archive_root/$ASSET/install.sh" --dry-run'),
  "macOS validation must exercise the archived installer"
)

publish_needs = jobs.fetch("publish").fetch("needs")
assert_contract(
  publish_needs.sort == %w[validate-linux validate-macos],
  "publishing must wait for Linux and macOS archive validation"
)

brew_validation = jobs.fetch("homebrew-validate")
brew_runners = brew_validation.dig("strategy", "matrix", "runner")
assert_contract(
  brew_runners.sort == %w[macos-15 ubuntu-24.04],
  "the formula must be validated on Linux and macOS"
)
assert_contract(
  named_step(brew_validation, "Update formula").fetch("run", "").include?("mkdir -p homebrew-tap/Formula"),
  "formula validation must create the tap directory"
)
assert_contract(
  named_step(brew_validation, "Validate formula").fetch("run", "").include?('mkdir -p "$(dirname "$tap_formula")"'),
  "formula validation must create the tapped Formula directory"
)

brew_publish = jobs.fetch("homebrew")
assert_contract(brew_publish["needs"] == "homebrew-validate", "formula publication must wait for validation")
assert_contract(
  named_step(brew_publish, "Publish formula").fetch("run", "").include?("mkdir -p homebrew-tap/Formula"),
  "formula publication must create the tap directory"
)
assert_contract(
  named_step(brew_publish, "Publish formula").fetch("run", "").include?("if [[ -f homebrew-tap/Formula/cxa.rb ]]"),
  "initial formula publication must guard the missing tap formula"
)
RUBY
