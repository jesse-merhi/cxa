# cxa

[![CI](https://github.com/jesse-merhi/cxa/actions/workflows/ci.yml/badge.svg)](https://github.com/jesse-merhi/cxa/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/jesse-merhi/cxa)](https://github.com/jesse-merhi/cxa/releases/latest)
[![License](https://img.shields.io/github/license/jesse-merhi/cxa)](LICENSE)

`cxa` is a fast CLI for switching between multiple ChatGPT accounts in Codex
and viewing each account's quota.

It uses Codex's normal credential file, so you can keep launching Codex exactly
as you do today.

## Requirements

- The [Codex CLI](https://developers.openai.com/codex/cli) installed and
  available as `codex`
- A ChatGPT OAuth login; API-key authentication is not supported
- macOS or Linux

## Install

### Homebrew

```sh
brew install jesse-merhi/tap/cxa
```

### From source

Requires Rust 1.85 or newer.

```sh
cargo install --locked --git https://github.com/jesse-merhi/cxa --bin cxa
```

Prebuilt binaries for Intel and Apple silicon Macs, plus x86-64 and ARM64 Linux,
are available from
[GitHub Releases](https://github.com/jesse-merhi/cxa/releases/latest). Linux
binaries require glibc 2.35 or newer. Windows is not currently supported.

## Quick start

Import the account already signed in to Codex:

```sh
cxa init
```

Add another account. `cxa` will open the normal Codex login flow:

```sh
cxa add
```

List your accounts and their latest known quota:

```sh
cxa list
```

Switch accounts by number or by a unique part of the email address:

```sh
cxa 2
cxa use work@example.com
```

Restart any running Codex or ChatGPT session after switching so it loads the
new account.

## Example

```text
$ cxa list
* 1  personal@example.com  primary 18% used, secondary 41% used, seen just now
  2  work@example.com  primary 63% used, secondary 9% used, seen just now

$ cxa 2
✓ Account 2 (work@example.com) is now selected.
! Restart Codex or ChatGPT before expecting an existing session to use this account.
```

The `*` marks the account currently selected in Codex.

## Commands

| Command | Description |
| --- | --- |
| `cxa` | Show the selected account and credential state |
| `cxa init` | Import the current Codex login as account 1 |
| `cxa add` | Sign in and add another account |
| `cxa list` | List accounts and their latest known quota |
| `cxa <account>` | Switch by account number or email |
| `cxa use <account>` | Switch using the explicit command form |
| `cxa status` | Show the selected account and credential state |
| `cxa relogin <account>` | Re-authenticate a saved account |
| `cxa import <auth.json>` | Import an existing Codex credential file |

Use `cxa --help` or `cxa <command> --help` for the complete CLI reference.

## How it works

Each account is stored as a profile under `~/.codex-auth`. When you switch,
`cxa` atomically copies that profile to `$CODEX_HOME/auth.json`, which is the
standard file-backed credential store used by Codex.

Codex keeps credentials in memory while it is running. Switching is safe, but
an existing Codex or ChatGPT process will continue using its previous account
until you restart it.

To read quota, `cxa` runs `codex app-server` with the saved account in an
isolated temporary home. This does not change the selected account. Codex owns
OAuth token refresh; if it refreshes a token during a quota read, `cxa` verifies
the account identity before saving the updated credentials.

Account identity includes the ChatGPT user ID and, when available, the
workspace ID. Accounts and workspaces that share an email address remain
distinct.

## Credential storage

`cxa` supports Codex's default file-backed credential store. If you configured
`cli_auth_credentials_store` as `keyring`, `auto`, or `ephemeral`, change it to
`file` and run `codex login` again before using `cxa`:

```toml
# ~/.codex/config.toml
cli_auth_credentials_store = "file"
```

If an account's refresh token is no longer valid, re-authenticate it:

```sh
cxa relogin <account>
```

## Configuration

| Variable | Purpose |
| --- | --- |
| `CODEX_HOME` | Override the Codex home directory |
| `CXA_ACCOUNT_STORE` | Override the account profile directory |
| `CXA_CODEX_BIN` | Override the Codex executable used for login and quota reads |
| `CXA_USAGE_TTL` | Set the quota cache lifetime in seconds (default: `120`) |
| `CXA_SKIP_USAGE_REFRESH=1` | Show cached quota without refreshing it |

Values supplied to path variables must be absolute.

For non-interactive setup, use `cxa init --yes`.

## Development

```sh
./scripts/check.sh
cargo build --locked --release --bin cxa
```

CI checks formatting, Clippy, tests, release packaging, and Linux and macOS
builds. Version tags publish native archives and update
[`jesse-merhi/homebrew-tap`](https://github.com/jesse-merhi/homebrew-tap).

## License

[MIT](LICENSE)
