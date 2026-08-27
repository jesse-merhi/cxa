# cxa

`cxa` is a fast, crash-safe account switcher for Codex ChatGPT OAuth accounts.
It keeps credentials for multiple accounts, promotes one account into a shared
Codex home, records quota for each account, and refuses unsafe credential
changes while another Codex process may be refreshing tokens.

The repository also includes an optional Linux service setup:

- `codex-shared-app-server` serves quota reads through a read-only view of the
  active credential, so it cannot compete with ordinary Codex token refreshes.
- `codex-quota-proxy` exposes only account and rate-limit methods to another
  local service account.
- `codex-shared-socket` safely prepares and publishes the private Unix socket.

## Install

After the first stable release is published, install with Homebrew:

```sh
brew install jesse-merhi/tap/cxa
```

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

Then initialize `cxa` from the Codex account that is already signed in:

```sh
cxa init
```

`cxa init` confirms the account before importing it. Use `cxa init --yes` in a
non-interactive setup.

Tagged releases contain native archives for Linux x86-64/ARM64 and macOS
x86-64/Apple silicon, plus a `SHA256SUMS` file. Extract the archive for your
platform and run `./install.sh` to install the bundled binary without Rust.
Linux archives target glibc 2.35 (Ubuntu 22.04 or newer) and are smoke-tested
on both supported architectures. Windows is not currently supported.

The CLI builds on macOS, but the shared app-server, privileged socket helper,
and systemd integration are Linux-only.

## Use

Initialize from the current Codex login, then enroll any additional accounts:

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
cxa relink
```

`cxa init` imports and selects the current Codex login. If Codex is running, its
live credentials stay detached and unchanged until it is safe to run `cxa
relink`. `cxa add` can enroll another account while Codex is running because
OAuth is staged in an isolated home. `cxa import` copies and validates an
existing Codex `auth.json`; it leaves the source and active credentials
unchanged.

`cxa list` refreshes inactive accounts without switching. While Codex is
running, the live account uses the shared app server when available and
otherwise keeps its cached quota so only one process can own token refresh. The
shared app server reloads externally rotated credentials before reading quota,
but its refresh endpoint is disabled so ordinary Codex remains the only OAuth
refresh owner. If Codex rotates OAuth credentials while cxa reads an inactive
account's quota, cxa safely saves the new credentials to that profile. If an
inactive account's access token has expired, relogin to refresh its credentials.

By default, account profiles live under `~/.codex-auth/profile-N`, the selected
slot is stored in `~/.codex-auth/active-profile`, and the promoted credential is
`~/.codex-auth/auth.json`. The optional Linux systemd units explicitly use
`/var/lib/codex-auth/auth.json`. These paths can be overridden with absolute
paths:

- `CXA_ACCOUNT_STORE`
- `CXA_ACTIVE_AUTH`
- `CODEX_HOME`
- `CXA_SHARED_APP_SERVER_SOCKET`
- `CXA_USAGE_TTL`
- `CXA_SKIP_USAGE_REFRESH=1`

The legacy `CXA_CODEX_HOME` override is accepted only when `CODEX_HOME` is
set to the same path. This prevents cxa from switching a different session than
ordinary Codex launches use.

## Safety model

Credential updates use a durable transaction record and atomic file promotion.
The active credential is hidden while OAuth login or an offline quota refresh
can rotate it. If `cxa` is killed or the machine loses power, the next run
either restores the previous credential or completes promotion of the rotated
credential.

Account identity includes the ChatGPT user ID and, when present, the workspace
account ID—not just the email address. This keeps users and workspaces with the
same email distinct while supporting personal accounts without a workspace ID.

`cxa` treats failed writer detection conservatively. If it cannot prove that
Codex is stopped, it refuses credential changes.

## Linux shared app server

The optional service setup requires the login user to belong to a local
`openclaw` group and installs four
system units. The installer records the Codex home, account store, service
credential and socket paths, and absolute Codex executable in
`~/.config/cxa/service.env` so the system service and CLI use the same paths and
installation. Build and install everything with:

```sh
sudo groupadd --system openclaw 2>/dev/null || true
sudo usermod -aG openclaw "$USER"
```

Start a new login session after adding the group, then build and install:

```sh
./install.sh --systemd
```

Provision the credential directory and link the login user's Codex home:

```sh
sudo install -d -m 0700 -o "$USER" -g openclaw /var/lib/codex-auth
CXA_ACTIVE_AUTH=/var/lib/codex-auth/auth.json cxa relink
```

Run the installer and migration as the login user; it invokes `sudo` only for
the system files. `cxa relink` copies the selected credential into the service
path before atomically linking the Codex session to it.

If either persistent path changes later, rerun the installer with the new
environment values before restarting the service.

The shared socket and active credential under `/var/lib/codex-auth` are
singleton machine state. Systemd installation therefore requires
`CXA_ACTIVE_AUTH=/var/lib/codex-auth/auth.json` and
`CXA_SHARED_APP_SERVER_SOCKET=/var/lib/codex-auth/app-server.sock`; custom
values for those two overrides are rejected. The installer refuses to transfer an existing
directory to a different login user. While the app server is running, its
verified socket directory is root-owned and non-writable; rerunning the
installer recognizes and preserves that published state. The service reloads
`auth.json` through Codex's guarded account refresh before each quota read, and
its refresh-token endpoint is redirected to an unbindable local address so it
cannot rotate credentials. Ordinary Codex remains the only OAuth refresh owner.

After enrolling and selecting the first account, start the services:

```sh
sudo systemctl enable --now "codex-shared-app-server@$USER.service"
sudo systemctl enable --now "codex-quota-proxy@$USER.socket"
```

The restricted bridge listens at `/run/codex-quota-proxy.sock`. It forwards
only:

- `initialize`
- `initialized`
- `account/read`, forced to `refreshToken: true` so Codex reloads credentials
  changed by the ordinary Codex process before reading quota
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
checksums. Stable releases also update `Formula/cxa.rb` in
[`jesse-merhi/homebrew-tap`](https://github.com/jesse-merhi/homebrew-tap).

## License

MIT
