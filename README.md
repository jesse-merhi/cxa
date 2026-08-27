# cxa

`cxa` is a fast account switcher for Codex ChatGPT OAuth accounts. It stores
multiple logins, copies the selected account into the normal Codex home, and
records quota for each account.

## Install

After the first stable release is published, install with Homebrew:

```sh
brew install jesse-merhi/tap/cxa
```

Install from source:

```sh
cargo install --locked --git https://github.com/jesse-merhi/cxa --bin cxa
```

Or clone the repository and install into `~/.local/bin`:

```sh
git clone https://github.com/jesse-merhi/cxa.git
cd cxa
./install.sh
```

Then import the Codex account that is already signed in:

```sh
cxa init
```

Use `cxa init --yes` when input or output is redirected.

Tagged releases contain native archives for Linux x86-64/ARM64 and macOS
x86-64/Apple silicon, plus a `SHA256SUMS` file. Extract the archive for your
platform and run `./install.sh` to install the bundled binary without Rust.
Linux archives target glibc 2.35 (Ubuntu 22.04 or newer). Windows is not
currently supported.

## Use

Initialize from the current Codex login, enroll another account, and switch:

```sh
cxa init
cxa add
cxa 2
```

Common commands:

```sh
cxa list
cxa status
cxa use 2
cxa import /path/to/auth.json
cxa relogin 2
```

`cxa` can switch accounts while Codex or ChatGPT is running. It directly
replaces `$CODEX_HOME/auth.json`; restart Codex or ChatGPT before expecting an
existing session to use the newly selected account.

Quota reads run through a separate `codex app-server` process in an isolated
temporary home for each saved profile. They do not switch the active account or
refresh its OAuth token. If a saved access token has expired, run `cxa relogin
<account>`.

Profiles live under `~/.codex-auth/profile-N`. The selected account is inferred
from the credentials currently stored at `$CODEX_HOME/auth.json`. The following
absolute-path overrides are supported:

- `CODEX_HOME`
- `CXA_ACCOUNT_STORE`
- `CXA_CODEX_BIN`
- `CXA_USAGE_TTL`
- `CXA_SKIP_USAGE_REFRESH=1`

## Credential model

Selecting an account atomically copies its saved profile to
`$CODEX_HOME/auth.json`. cxa infers the selection from that file, serializes its
own writes with a local lock, and does not coordinate with running Codex or
ChatGPT processes.

Account identity includes the ChatGPT user ID and, when present, the workspace
account ID, not only the email address. This keeps different workspaces with the
same email distinct.

## Development

The project requires Rust 1.85 or newer.

```sh
./scripts/check.sh
cargo build --locked --release --bin cxa
```

Pull requests run formatting, clippy with warnings denied, tests, and builds on
Linux and macOS. A matching version tag publishes native release archives and
updates `Formula/cxa.rb` in
[`jesse-merhi/homebrew-tap`](https://github.com/jesse-merhi/homebrew-tap).

## License

MIT
