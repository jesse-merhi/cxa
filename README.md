# cxa

`cxa` is a fast, crash-safe account switcher for Codex ChatGPT OAuth accounts.
It keeps credentials for multiple accounts, promotes one account into a shared
Codex home, records quota for each account, and refuses unsafe credential
changes while another Codex process may be refreshing tokens.

The repository also includes an optional Linux service setup:

- `codex-shared-app-server` is the sole owner allowed to refresh the active
  credential.
- `codex-quota-proxy` exposes only account and rate-limit methods to another
  local service account.
- `codex-shared-socket` safely prepares and publishes the private Unix socket.

## Install

Install the CLI from source:

```sh
cargo install --locked --git https://github.com/jesse-merhi/cxa --bin cxa
```

Or clone the repository and install into `~/.local/bin`:

```sh
git clone https://github.com/jesse-merhi/cxa.git
cd cxa
./install.sh
```

Tagged releases contain native archives for Linux x86-64/ARM64 and macOS
x86-64/Apple silicon, plus a `SHA256SUMS` file.

The CLI builds on macOS, but the shared app-server, privileged socket helper,
and systemd integration are Linux-only.

## Use

Enroll an account, then select it:

```sh
cxa add
cxa 1
```

Common commands:

```sh
cxa list
cxa status
cxa use 2
cxa import /path/to/auth.json
cxa relogin 2
cxa relink
```

`cxa add` can enroll an account while Codex is running because OAuth is staged
in an isolated home. `cxa import` copies and validates an existing Codex
`auth.json`; it leaves the source and active credentials unchanged.

`cxa list` refreshes quota for every enrolled account without rotating saved
tokens. If an inactive account's access token has expired, relogin to refresh
its credentials.

By default, account profiles live under `~/.codex-auth/profile-N` and the
selected slot is stored in `~/.codex-auth/active-profile`. On macOS the promoted
credential is `~/.codex-auth/auth.json`; Linux service installations use
`/var/lib/codex-auth/auth.json`. These paths can be overridden with:

- `CXA_ACCOUNT_STORE`
- `CXA_ACTIVE_AUTH`
- `CXA_CODEX_HOME`
- `CXA_SHARED_APP_SERVER_SOCKET`
- `CXA_USAGE_TTL`
- `CXA_SKIP_USAGE_REFRESH=1`

## Safety model

Credential updates use a durable transaction record and atomic file promotion.
The active credential is hidden while OAuth login or an offline quota refresh
can rotate it. If `cxa` is killed or the machine loses power, the next run
either restores the previous credential or completes promotion of the rotated
credential.

Account identity includes the workspace account ID and ChatGPT user ID, not
just the email address. This keeps users and workspaces with the same email
distinct.

`cxa` treats failed writer detection conservatively. If it cannot prove that
Codex is stopped, it refuses credential changes.

## Linux shared app server

The optional service setup assumes a local `openclaw` group and installs three
system units. Build and install everything with:

```sh
./install.sh --systemd
```

Provision the credential directory and link the login user's Codex home:

```sh
sudo install -d -m 0700 -o "$USER" -g openclaw /var/lib/codex-auth
install -d -m 0700 "$HOME/.codex"
ln -sfn /var/lib/codex-auth/auth.json "$HOME/.codex/auth.json"
```

After enrolling and selecting the first account, start the services:

```sh
sudo systemctl enable --now "codex-shared-app-server@$USER.service"
sudo systemctl enable --now "codex-quota-proxy@$USER.socket"
```

The restricted bridge listens at `/run/codex-quota-proxy.sock`. It forwards
only:

- `initialize`
- `initialized`
- `account/read`, forced to `refreshToken: false`
- `account/rateLimits/read`
- `account/rateLimits/updated` notifications

Thread, turn, command, and file methods are rejected locally and never reach
the private app server.

Stop both services before changing credentials:

```sh
sudo systemctl stop "codex-quota-proxy@$USER.socket"
sudo systemctl stop "codex-shared-app-server@$USER.service"
cxa 2
sudo systemctl start "codex-shared-app-server@$USER.service"
sudo systemctl start "codex-quota-proxy@$USER.socket"
```

## Development

The project requires Rust 1.85 or newer.

```sh
./scripts/check.sh
cargo build --locked --release --bins
```

Pull requests run formatting, clippy with warnings denied, the full test suite,
and binary builds on Linux and macOS. Pushing a tag matching the Cargo version,
such as `v0.1.0`, runs the same checks and publishes release archives with
checksums.

## License

MIT
