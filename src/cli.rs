use std::ffi::OsString;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anstream::{eprintln, print, println};
use clap::{Parser, Subcommand};
use tempfile::Builder;

use crate::account_store::{Profile, Store, UsageRecord, now_epoch};
use crate::app_server::{query_profile, require_file_credentials};
use crate::auth::AuthDocument;
use crate::config::Config;
use crate::fs::{ExclusiveLock, atomic_copy, private_dir, remove_file_if_exists};
use crate::terminal::{
    ACCENT, EMPHASIS, FetchSpinner, LiveRegion, MUTED, SUCCESS, WARNING, WatchTerminal,
    print_usage, usage_plan, usage_recency, watch_exit_requested,
};
use crate::{Error, Result};

#[derive(Debug, Parser)]
#[command(name = "cxa", version, about = "Fast Codex account switcher")]
pub struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
    #[arg(value_name = "ACCOUNT")]
    account: Option<String>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Import the current Codex login and select it as the first account.
    Init {
        /// Import without asking for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// List enrolled accounts and their quota.
    List {
        /// Keep the live list open and refresh it periodically.
        #[arg(short, long)]
        watch: bool,
        /// Seconds between refreshes in watch mode.
        #[arg(
            long,
            default_value_t = 60,
            requires = "watch",
            value_parser = clap::value_parser!(u64).range(5..)
        )]
        interval: u64,
    },
    /// Keep the live quota dashboard open.
    Watch {
        /// Seconds between refreshes.
        #[arg(
            long,
            default_value_t = 60,
            value_parser = clap::value_parser!(u64).range(5..)
        )]
        interval: u64,
    },
    /// Show the selected account and credential-file state.
    Status,
    /// Switch by slot number or a unique part of the account email.
    Use { account: String },
    /// Enroll a new ChatGPT OAuth account.
    #[command(trailing_var_arg = true)]
    Add {
        /// Sign in with Codex's device-code flow.
        #[arg(long)]
        device_auth: bool,
        #[arg(allow_hyphen_values = true, hide = true)]
        options: Vec<OsString>,
    },
    /// Import an existing Codex auth.json file.
    Import { auth_file: PathBuf },
    /// Re-authenticate an existing account.
    #[command(trailing_var_arg = true)]
    Relogin {
        account: String,
        #[arg(allow_hyphen_values = true)]
        options: Vec<OsString>,
    },
}

pub fn run(cli: Cli, config: Config) -> Result<()> {
    require_file_credentials(&config)?;
    let app = App::new(config);
    match (cli.command, cli.account) {
        (Some(CliCommand::Init { yes }), _) => app.init(yes),
        (Some(CliCommand::List { watch, interval }), _) => app.list(watch, interval),
        (Some(CliCommand::Watch { interval }), _) => app.list(true, interval),
        (Some(CliCommand::Status), _) | (None, None) => app.status(true),
        (Some(CliCommand::Use { account }), _) | (None, Some(account)) => app.switch(&account),
        (
            Some(CliCommand::Add {
                device_auth,
                options,
            }),
            _,
        ) => app.add(device_auth, &options),
        (Some(CliCommand::Import { auth_file }), _) => app.import(&auth_file),
        (Some(CliCommand::Relogin { account, options }), _) => app.relogin(&account, &options),
    }
}

struct App {
    store: Store,
}

impl App {
    fn new(config: Config) -> Self {
        Self {
            store: Store::new(config),
        }
    }

    fn locked(&self) -> Result<ExclusiveLock> {
        let lock = self.store.lock()?;
        self.store.sync_session_profile()?;
        Ok(lock)
    }

    fn init(&self, assume_yes: bool) -> Result<()> {
        let _lock = self.locked()?;
        let profiles = self.store.profiles()?;
        if !profiles.is_empty() {
            if let Some(selected) = self.store.selected() {
                let profile = self.store.resolve(&selected.to_string())?;
                println!(
                    "{SUCCESS}✓{SUCCESS:#} cxa is already initialized with account {ACCENT}{selected}{ACCENT:#} ({EMPHASIS}{}{EMPHASIS:#}).",
                    profile.auth.identity.label()
                );
                return Ok(());
            }
            return Err(Error::Message(
                "cxa has enrolled accounts but none is selected. Run `cxa list`, then `cxa <account>`."
                    .into(),
            ));
        }

        let session_path = &self.store.config.session_auth;
        if !session_path.is_file() {
            return Err(Error::Message(
                "No current Codex login was found. Run `codex login`, then run `cxa init`.".into(),
            ));
        }
        let current = AuthDocument::read(session_path)?;
        if !assume_yes {
            if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
                return Err(Error::Message(format!(
                    "Found the current Codex login: {}. Run `cxa init --yes` when input or output is redirected.",
                    current.identity.label()
                )));
            }
            print!(
                "{ACCENT}?{ACCENT:#} Found the current Codex login: {EMPHASIS}{}{EMPHASIS:#}\n  Import it as account {ACCENT}1{ACCENT:#}? {MUTED}[Y/n]{MUTED:#} ",
                current.identity.label()
            );
            io::stdout()
                .flush()
                .map_err(|error| Error::io("stdout", error))?;
            let mut answer = String::new();
            io::stdin()
                .read_line(&mut answer)
                .map_err(|error| Error::io("stdin", error))?;
            if matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no") {
                return Err(Error::Message("Initialization cancelled.".into()));
            }
        }

        let profile = self.store.enroll(session_path)?;
        println!(
            "{SUCCESS}✓{SUCCESS:#} Imported {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{}{ACCENT:#}.",
            profile.auth.identity.label(),
            profile.slot
        );
        println!(
            "{SUCCESS}✓{SUCCESS:#} Account {ACCENT}{}{ACCENT:#} is now selected.",
            profile.slot
        );
        println!("{MUTED}Next:{MUTED:#} add another account with {ACCENT}cxa add{ACCENT:#}");
        Ok(())
    }

    fn list(&self, watch: bool, interval: u64) -> Result<()> {
        let mut region = LiveRegion::new();
        if watch && !(region.is_active() && io::stdin().is_terminal()) {
            return Err(Error::Message(
                "Watch mode requires an interactive terminal.".into(),
            ));
        }
        if !watch {
            let ListRefresh::Completed { session_changed } =
                self.refresh_list(&mut region, false, false)?
            else {
                unreachable!("one-shot lists do not read keyboard input");
            };
            if session_changed {
                restart_notice();
            }
            return Ok(());
        }

        let _terminal = WatchTerminal::enter().map_err(|error| Error::io("terminal", error))?;
        let mut restart_required = false;
        loop {
            match self.refresh_list(&mut region, true, true)? {
                ListRefresh::Completed { session_changed } => {
                    restart_required |= session_changed;
                }
                ListRefresh::ExitRequested => return Ok(()),
            }
            for remaining in (1..=interval).rev() {
                let status = watch_status(remaining, restart_required);
                region
                    .write_status(&status)
                    .map_err(|error| Error::io("stdout", error))?;
                if watch_exit_requested(Duration::from_secs(1))
                    .map_err(|error| Error::io("terminal input", error))?
                {
                    return Ok(());
                }
            }
        }
    }

    fn refresh_list(
        &self,
        region: &mut LiveRegion,
        force_refresh: bool,
        watch: bool,
    ) -> Result<ListRefresh> {
        let _lock = self.locked()?;
        let profiles = self.store.profiles()?;
        if profiles.is_empty() {
            region
                .redraw(|| {
                    println!("{}", self.initialization_guidance());
                    1
                })
                .map_err(|error| Error::io("stdout", error))?;
            return Ok(ListRefresh::Completed {
                session_changed: false,
            });
        }
        let selected = self.store.selected();
        let mut states: Vec<ProfileUsage> = profiles
            .iter()
            .map(|profile| {
                if !self.store.config.skip_usage_refresh
                    && (force_refresh || self.needs_usage_refresh(profile.slot))
                {
                    ProfileUsage::Loading
                } else {
                    ProfileUsage::Ready(self.store.usage(profile.slot))
                }
            })
            .collect();
        let refresh_slots: Vec<u32> = profiles
            .iter()
            .zip(&states)
            .filter_map(|(profile, state)| state.is_loading().then_some(profile.slot))
            .collect();
        let mut frame = 0;
        if region.is_active() && !refresh_slots.is_empty() {
            region
                .redraw(|| print_profile_list(&profiles, selected, &states, now_epoch(), frame))
                .map_err(|error| Error::io("stdout", error))?;
        }

        let (sender, receiver) = mpsc::channel();
        let mut workers = Vec::new();
        for slot in refresh_slots {
            let sender = sender.clone();
            let config = self.store.config.clone();
            workers.push(thread::spawn(move || {
                let result = refresh_usage_for_config(&config, slot, force_refresh);
                let _ = sender.send((slot, result));
            }));
        }
        drop(sender);

        let mut session_changed = false;
        let mut first_error = None;
        let refresh_total = workers.len();
        let mut completed = 0;
        while completed < refresh_total {
            let received = if region.is_active() {
                match receiver.recv_timeout(Duration::from_millis(80)) {
                    Ok(result) => Some(result),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                receiver.recv().ok()
            };
            if let Some((slot, result)) = received {
                completed += 1;
                match result {
                    Ok(changed) => session_changed |= changed,
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
                if let Some((index, _)) = profiles
                    .iter()
                    .enumerate()
                    .find(|(_, profile)| profile.slot == slot)
                {
                    states[index] = ProfileUsage::Ready(self.store.usage(slot));
                }
            }
            if region.is_active() {
                frame += 1;
                region
                    .redraw(|| print_profile_list(&profiles, selected, &states, now_epoch(), frame))
                    .map_err(|error| Error::io("stdout", error))?;
            }
            if watch
                && watch_exit_requested(Duration::ZERO)
                    .map_err(|error| Error::io("terminal input", error))?
            {
                return Ok(ListRefresh::ExitRequested);
            }
        }
        for worker in workers {
            if worker.join().is_err() && first_error.is_none() {
                first_error = Some(Error::Message("usage refresh worker failed".into()));
            }
        }
        if region.is_active() {
            region
                .redraw(|| print_profile_list(&profiles, selected, &states, now_epoch(), frame))
                .map_err(|error| Error::io("stdout", error))?;
        } else {
            print_profile_list(&profiles, selected, &states, now_epoch(), frame);
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(ListRefresh::Completed { session_changed })
    }

    fn status(&self, refresh: bool) -> Result<()> {
        let _lock = self.locked()?;
        if self.store.profiles()?.is_empty() {
            println!("{}", self.initialization_guidance());
            return Ok(());
        }
        let selected = self
            .store
            .selected()
            .ok_or_else(|| Error::Message("No Codex account is selected.".into()))?;
        let profile = self.store.resolve(&selected.to_string())?;
        let session_changed = if refresh && self.needs_usage_refresh(selected) {
            let spinner = FetchSpinner::start(format!(
                "Fetching usage [1/1] {}",
                profile.auth.identity.label()
            ));
            let changed = self.refresh_usage(selected)?;
            spinner.finish();
            changed
        } else {
            false
        };
        let usage = self.store.usage(selected);
        print_profile(&profile, true, usage.as_ref(), now_epoch());
        let credential = self.store.credential_status(selected)?;
        let style = if credential.starts_with("matches") {
            SUCCESS
        } else {
            WARNING
        };
        println!("    {ACCENT}Credential{ACCENT:#}  {style}{credential}{style:#}");
        if session_changed {
            restart_notice();
        }
        Ok(())
    }

    fn initialization_guidance(&self) -> String {
        match AuthDocument::read(&self.store.config.session_auth) {
            Ok(auth) => format!(
                "Found the current Codex login: {}\ncxa is not initialized. Run: cxa init",
                auth.identity.label()
            ),
            Err(_) => "cxa is not initialized. Run `codex login`, then run `cxa init`.".into(),
        }
    }

    fn refresh_usage(&self, slot: u32) -> Result<bool> {
        refresh_usage_for_config(&self.store.config, slot, false)
    }

    fn needs_usage_refresh(&self, slot: u32) -> bool {
        !self.store.config.skip_usage_refresh && !self.store.usage_fresh(slot)
    }

    fn switch(&self, selector: &str) -> Result<()> {
        let _lock = self.locked()?;
        let target = self.store.resolve(selector)?;
        if self
            .store
            .usage(target.slot)
            .is_some_and(|usage| usage.exhausted_now(now_epoch()))
        {
            eprintln!(
                "{WARNING}warning{WARNING:#}: account {ACCENT}{}{ACCENT:#} ({EMPHASIS}{}{EMPHASIS:#}) was last seen exhausted",
                target.slot,
                target.auth.identity.label()
            );
        }
        let selected = self.store.select(target.slot)?;
        println!(
            "{SUCCESS}✓{SUCCESS:#} Account {ACCENT}{}{ACCENT:#} ({EMPHASIS}{}{EMPHASIS:#}) is now selected.",
            selected.slot,
            selected.auth.identity.label()
        );
        restart_notice();
        Ok(())
    }

    fn add(&self, device_auth: bool, options: &[OsString]) -> Result<()> {
        let mut options = options.to_vec();
        if device_auth {
            options.push("--device-auth".into());
        }
        reject_non_oauth(&options)?;
        let _lock = self.locked()?;
        let login = StagedLogin::run(&self.store.config, &options)?;
        let profile = self.store.enroll(&login.auth_path())?;
        println!(
            "\n{SUCCESS}✓{SUCCESS:#} Enrolled {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{}{ACCENT:#}.",
            profile.auth.identity.label(),
            profile.slot
        );
        println!(
            "{MUTED}Next:{MUTED:#} switch to it with {ACCENT}cxa {}{ACCENT:#}",
            profile.slot
        );
        Ok(())
    }

    fn import(&self, auth_file: &Path) -> Result<()> {
        let _lock = self.locked()?;
        let profile = self.store.enroll(auth_file)?;
        println!(
            "{SUCCESS}✓{SUCCESS:#} Imported {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{}{ACCENT:#}.",
            profile.auth.identity.label(),
            profile.slot
        );
        println!(
            "{MUTED}Next:{MUTED:#} switch to it with {ACCENT}cxa {}{ACCENT:#}",
            profile.slot
        );
        Ok(())
    }

    fn relogin(&self, selector: &str, options: &[OsString]) -> Result<()> {
        reject_non_oauth(options)?;
        let _lock = self.locked()?;
        let target = self.store.resolve(selector)?;
        println!(
            "{ACCENT}Re-authenticating account {}{ACCENT:#} ({EMPHASIS}{}{EMPHASIS:#}).\n",
            target.slot,
            target.auth.identity.label()
        );
        let login = StagedLogin::run(&self.store.config, options)?;
        let profile = self.store.replace(target.slot, &login.auth_path())?;
        remove_file_if_exists(&self.store.config.profile_usage(target.slot))?;
        if self.store.selected() == Some(target.slot) {
            self.store.select(target.slot)?;
            restart_notice();
        }
        println!(
            "{SUCCESS}✓{SUCCESS:#} Re-authenticated {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{}{ACCENT:#}.",
            profile.auth.identity.label(),
            profile.slot
        );
        Ok(())
    }
}

enum ListRefresh {
    Completed { session_changed: bool },
    ExitRequested,
}

enum ProfileUsage {
    Loading,
    Ready(Option<UsageRecord>),
}

impl ProfileUsage {
    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }
}

const LOADING_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn watch_status(remaining: u64, restart_required: bool) -> String {
    let remaining = interval_label(remaining);
    if restart_required {
        format!(
            "{WARNING}! Restart Codex/ChatGPT{WARNING:#} {MUTED}· refresh in {remaining} · Ctrl-C to exit{MUTED:#}"
        )
    } else {
        format!("{MUTED}Watching · refresh in {remaining} · Ctrl-C to exit{MUTED:#}")
    }
}

fn interval_label(seconds: u64) -> String {
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn print_profile_list(
    profiles: &[Profile],
    selected: Option<u32>,
    states: &[ProfileUsage],
    now: i64,
    frame: usize,
) -> usize {
    let mut lines = 0;
    for (index, (profile, state)) in profiles.iter().zip(states).enumerate() {
        if index > 0 {
            println!();
            lines += 1;
        }
        match state {
            ProfileUsage::Loading => {
                print_loading_profile(profile, selected == Some(profile.slot), frame);
                lines += 1;
            }
            ProfileUsage::Ready(usage) => {
                print_profile(profile, selected == Some(profile.slot), usage.as_ref(), now);
                lines += profile_line_count(usage.as_ref());
            }
        }
    }
    lines
}

fn print_loading_profile(profile: &Profile, selected: bool, frame: usize) {
    let marker = if selected { "*" } else { " " };
    let spinner = LOADING_FRAMES[frame % LOADING_FRAMES.len()];
    println!(
        "{ACCENT}{marker} {}{ACCENT:#}  {EMPHASIS}{}{EMPHASIS:#}  {ACCENT}{spinner}{ACCENT:#} {MUTED}loading{MUTED:#}",
        profile.slot,
        profile.auth.identity.label()
    );
}

fn profile_line_count(usage: Option<&UsageRecord>) -> usize {
    let usage_lines = match usage {
        None => 1,
        Some(usage) if usage.error.is_some() || usage.buckets.is_empty() => 1,
        Some(usage) => usage
            .buckets
            .iter()
            .map(|bucket| 1 + bucket.windows().count())
            .sum(),
    };
    1 + usage_lines
}

fn refresh_usage_for_config(config: &Config, slot: u32, force_refresh: bool) -> Result<bool> {
    let store = Store::new(config.clone());
    if config.skip_usage_refresh || (!force_refresh && store.usage_fresh(slot)) {
        return Ok(false);
    }
    let previous = store.usage(slot);
    let (next, session_changed) = query_profile(config, &config.profile_auth(slot));
    write_usage_result(previous.as_ref(), &next, &config.profile_usage(slot))?;
    Ok(session_changed)
}

fn print_profile(profile: &Profile, selected: bool, usage: Option<&UsageRecord>, now: i64) {
    let marker = if selected { "*" } else { " " };
    let plan = usage_plan(usage);
    let recency = usage_recency(usage, now);
    if let (Some(plan), Some(recency)) = (plan.as_deref(), recency.as_deref()) {
        println!(
            "{ACCENT}{marker} {}{ACCENT:#}  {EMPHASIS}{}{EMPHASIS:#}  {MUTED}{plan} · {recency}{MUTED:#}",
            profile.slot,
            profile.auth.identity.label()
        );
    } else if let Some(plan) = plan {
        println!(
            "{ACCENT}{marker} {}{ACCENT:#}  {EMPHASIS}{}{EMPHASIS:#}  {MUTED}{plan}{MUTED:#}",
            profile.slot,
            profile.auth.identity.label()
        );
    } else if let Some(recency) = recency {
        println!(
            "{ACCENT}{marker} {}{ACCENT:#}  {EMPHASIS}{}{EMPHASIS:#}  {MUTED}{recency}{MUTED:#}",
            profile.slot,
            profile.auth.identity.label()
        );
    } else {
        println!(
            "{ACCENT}{marker} {}{ACCENT:#}  {EMPHASIS}{}{EMPHASIS:#}",
            profile.slot,
            profile.auth.identity.label()
        );
    }
    print_usage(usage, now);
}

fn restart_notice() {
    println!(
        "{WARNING}!{WARNING:#} Restart Codex or ChatGPT before expecting an existing session to use this account."
    );
}

fn write_usage_result(
    previous: Option<&UsageRecord>,
    next: &UsageRecord,
    path: &Path,
) -> Result<()> {
    if !next.succeeded() {
        if let Some(previous) = previous.filter(|usage| usage.succeeded()) {
            let mut retained = previous.clone();
            retained.last_attempted_at = next.last_attempted_at.max(next.observed_at);
            return retained.write(path);
        }
    }
    next.write(path)
}

fn reject_non_oauth(options: &[OsString]) -> Result<()> {
    for option in options {
        let Some(option) = option.to_str() else {
            continue;
        };
        let name = option.split_once('=').map_or(option, |(name, _)| name);
        if matches!(name, "--with-api-key" | "--with-access-token") {
            return Err(Error::Message(format!(
                "cxa requires ChatGPT OAuth credentials; {name} is not supported."
            )));
        }
    }
    Ok(())
}

struct StagedLogin {
    home: tempfile::TempDir,
}

impl StagedLogin {
    fn run(config: &Config, options: &[OsString]) -> Result<Self> {
        private_dir(&config.account_store)?;
        let home = Builder::new()
            .prefix(".login-")
            .tempdir_in(&config.account_store)
            .map_err(|error| Error::io(&config.account_store, error))?;
        let source_config = config.codex_home.join("config.toml");
        if source_config.is_file() {
            atomic_copy(&source_config, &home.path().join("config.toml"), 0o600)?;
        }
        let status = Command::new(config.codex_binary())
            .arg("login")
            .args(options)
            .args(["-c", "cli_auth_credentials_store=\"file\""])
            .env("CODEX_HOME", home.path())
            .status()
            .map_err(|error| Error::io("codex login", error))?;
        if !status.success() || !home.path().join("auth.json").is_file() {
            return Err(Error::Message(
                "Codex login failed; nothing was changed.".into(),
            ));
        }
        Ok(Self { home })
    }

    fn auth_path(&self) -> PathBuf {
        self.home.path().join("auth.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_refresh_preserves_successful_usage() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("usage.json");
        let previous = UsageRecord {
            observed_at: 1,
            last_attempted_at: 1,
            ..UsageRecord::default()
        };
        previous.write(&path).unwrap();
        let failed = UsageRecord {
            observed_at: 2,
            last_attempted_at: 2,
            error: Some("quota unavailable (Timeout)".into()),
            ..UsageRecord::default()
        };

        write_usage_result(Some(&previous), &failed, &path).unwrap();

        let retained = UsageRecord::read(&path).unwrap();
        assert_eq!(retained.observed_at, 1);
        assert_eq!(retained.last_attempted_at, 2);
        assert!(retained.succeeded());
    }

    #[test]
    fn watch_countdown_uses_compact_time() {
        assert_eq!(interval_label(5), "5s");
        assert_eq!(interval_label(60), "1m 00s");
        assert_eq!(interval_label(125), "2m 05s");
    }
}
