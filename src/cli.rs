use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anstream::{eprintln, print, println};
use anstyle::Style;
use clap::{Parser, Subcommand};
use tempfile::Builder;

use crate::account_store::{Store, UsageRecord, now_epoch};
use crate::app_server::{
    prepare_offline_home, query_offline, query_offline_read_only, query_shared,
};
use crate::auth::AuthDocument;
use crate::config::Config;
use crate::fs::{private_dir, remove_file_if_exists};
use crate::process::{WriterStatus, codex_writer_status, writers_running};
use crate::terminal::{ACCENT, EMPHASIS, ERROR, MUTED, SUCCESS, WARNING};
use crate::{Error, Result};

#[derive(Debug, Parser)]
#[command(name = "cxa", version, about = "Crash-safe Codex account switcher")]
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
    /// List enrolled accounts and their last known quota.
    List,
    /// Show the selected account and shared credential state.
    Status,
    /// Switch by slot number or a unique part of the account email.
    Use { account: String },
    /// Enroll a new ChatGPT OAuth account.
    #[command(trailing_var_arg = true)]
    Add {
        #[arg(allow_hyphen_values = true)]
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
    /// Repair ~/.codex/auth.json so it points at the shared credential.
    Relink,
    #[command(hide = true)]
    ServiceGuard,
    #[command(hide = true)]
    ServiceRelease,
}

pub fn run(cli: Cli, config: Config) -> Result<()> {
    let app = App::new(config);
    match (cli.command, cli.account) {
        (Some(CliCommand::Init { yes }), _) => app.init(yes),
        (Some(CliCommand::List), _) => app.list(),
        (Some(CliCommand::Status), _) | (None, None) => app.status(true),
        (Some(CliCommand::Use { account }), _) | (None, Some(account)) => app.switch(&account),
        (Some(CliCommand::Add { options }), _) => app.add(&options),
        (Some(CliCommand::Import { auth_file }), _) => app.import(&auth_file),
        (Some(CliCommand::Relogin { account, options }), _) => app.relogin(&account, &options),
        (Some(CliCommand::Relink), _) => app.relink(),
        (Some(CliCommand::ServiceGuard), _) => app.service_guard(),
        (Some(CliCommand::ServiceRelease), _) => app.service_release(),
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

    fn recover_locked(&self) -> Result<()> {
        let _lock = self.store.lock()?;
        self.store.recover()?;
        self.cleanup_orphaned_homes()
    }

    fn cleanup_orphaned_homes(&self) -> Result<()> {
        let entries = match fs::read_dir(&self.store.config.account_store) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::io(&self.store.config.account_store, error)),
        };
        for entry in entries {
            let entry =
                entry.map_err(|error| Error::io(&self.store.config.account_store, error))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".quota-") {
                fs::remove_dir_all(entry.path()).map_err(|error| Error::io(entry.path(), error))?;
                continue;
            }
            if !name.starts_with(".enroll-") || !entry.path().is_dir() {
                continue;
            }
            let auth = entry.path().join("auth.json");
            if auth.is_file() {
                let status = Command::new("codex")
                    .arg("logout")
                    .env("CODEX_HOME", entry.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map_err(|error| Error::io("codex logout", error))?;
                if !status.success() {
                    return Err(Error::Message(format!(
                        "Codex logout still failed; rejected credentials remain at {} for retry.",
                        entry.path().display()
                    )));
                }
            }
            fs::remove_dir_all(entry.path()).map_err(|error| Error::io(entry.path(), error))?;
        }
        Ok(())
    }

    fn init(&self, assume_yes: bool) -> Result<()> {
        self.recover_locked()?;
        let _lock = self.store.lock()?;
        self.store.adopt_detached_session_selection()?;

        let profiles = self.store.profiles()?;
        if !profiles.is_empty() {
            if let Some(selected) = self.store.selected() {
                let profile = self.store.resolve(&selected.to_string())?;
                println!(
                    "{SUCCESS}✓{SUCCESS:#} cxa is already initialized with account {ACCENT}{selected}{ACCENT:#} ({EMPHASIS}{}{EMPHASIS:#}).",
                    profile.auth.identity.email
                );
                return Ok(());
            }
            return Err(Error::Message(
                "cxa already has enrolled accounts, but none is selected. Run `cxa list`, then select one with `cxa <account>`."
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
            if !io::stdin().is_terminal() {
                return Err(Error::Message(format!(
                    "Found the current Codex login: {}. Run `cxa init --yes` to import it non-interactively.",
                    current.identity.email
                )));
            }
            print!(
                "{ACCENT}?{ACCENT:#} Found the current Codex login: {EMPHASIS}{}{EMPHASIS:#}\n  Import it as account {ACCENT}1{ACCENT:#}? {MUTED}[Y/n]{MUTED:#} ",
                current.identity.email
            );
            io::stdout()
                .flush()
                .map_err(|error| Error::io("stdout", error))?;
            let mut answer = String::new();
            io::stdin()
                .read_line(&mut answer)
                .map_err(|error| Error::io("stdin", error))?;
            match answer.trim().to_ascii_lowercase().as_str() {
                "" | "y" | "yes" => {}
                "n" | "no" => {
                    println!("{MUTED}Initialization cancelled.{MUTED:#}");
                    return Ok(());
                }
                _ => {
                    return Err(Error::Message(
                        "Expected yes or no; nothing was changed.".into(),
                    ));
                }
            }
        }

        let slot = self.store.next_slot()?;
        let barrier = self.store.begin_enrollment(session_path.to_owned())?;
        barrier.commit_profile(slot, session_path, true, true, false)?;

        let linked = if writers_running(&self.store.config) {
            false
        } else {
            match self
                .store
                .begin_barrier()
                .and_then(|barrier| barrier.commit_switch(slot, true))
            {
                Ok(()) => true,
                Err(Error::WriterRunning) => false,
                Err(error) => return Err(error),
            }
        };

        self.refresh_usage(slot)?;
        println!(
            "{SUCCESS}✓{SUCCESS:#} Imported {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{slot}{ACCENT:#}.",
            current.identity.email
        );
        println!("{SUCCESS}✓{SUCCESS:#} Account {ACCENT}{slot}{ACCENT:#} is now selected.");
        let usage = self
            .store
            .usage(slot)
            .map(|usage| usage.label(now_epoch()))
            .unwrap_or_else(|| "usage unknown".into());
        let usage_style = usage_style(&usage);
        println!("{ACCENT}Quota{ACCENT:#}: {usage_style}{usage}{usage_style:#}");
        if !linked {
            println!(
                "{WARNING}!{WARNING:#} Codex is running, so its live credentials were left untouched; run {ACCENT}cxa relink{ACCENT:#} after Codex stops to enable switching."
            );
        }
        println!("{MUTED}Next:{MUTED:#} add another account with {ACCENT}cxa add{ACCENT:#}");
        Ok(())
    }

    fn initialization_guidance(&self) -> String {
        match AuthDocument::read(&self.store.config.session_auth) {
            Ok(current) => format!(
                "Found the current Codex login: {}\ncxa is not initialized. Run: cxa init",
                current.identity.email
            ),
            Err(_) => "cxa is not initialized. Run `codex login`, then run `cxa init`.".into(),
        }
    }

    fn list(&self) -> Result<()> {
        self.recover_locked()?;
        let _lock = self.store.lock()?;
        self.store.adopt_detached_session_selection()?;
        if !writers_running(&self.store.config) {
            self.store.sync_active_to_profile()?;
        }
        let profiles = self.store.profiles()?;
        if profiles.is_empty() {
            println!("{}", self.initialization_guidance());
            return Ok(());
        }
        for profile in &profiles {
            self.refresh_usage(profile.slot)?;
        }
        let selected = self.store.selected();
        let width = profiles
            .iter()
            .map(|profile| profile.auth.identity.email.len())
            .max()
            .unwrap_or_default();
        for profile in &profiles {
            let selected_style = if Some(profile.slot) == selected {
                SUCCESS
            } else {
                Style::new()
            };
            let marker = if Some(profile.slot) == selected {
                '*'
            } else {
                ' '
            };
            let usage = self
                .store
                .usage(profile.slot)
                .map(|usage| usage.label(now_epoch()))
                .unwrap_or_else(|| "usage unknown".into());
            let usage_style = usage_style(&usage);
            println!(
                "{selected_style}{marker}{selected_style:#} {ACCENT}{}{ACCENT:#}  {EMPHASIS}{:width$}{EMPHASIS:#}  {usage_style}{usage}{usage_style:#}",
                profile.slot, profile.auth.identity.email
            );
        }
        if profiles.len() > 1 {
            println!(
                "{MUTED}Usage refreshes without switching; relogin an account if its saved access token has expired.{MUTED:#}"
            );
        }
        Ok(())
    }

    fn status(&self, refresh: bool) -> Result<()> {
        self.recover_locked()?;
        let _lock = self.store.lock()?;
        self.store.adopt_detached_session_selection()?;
        if self.store.profiles()?.is_empty() {
            return Err(Error::Message(self.initialization_guidance()));
        }
        if !writers_running(&self.store.config) {
            self.store.sync_active_to_profile()?;
        }
        if refresh {
            if let Some(slot) = self.store.selected() {
                self.refresh_usage(slot)?;
            }
        }
        for line in self.store.status_lines()? {
            print_status_line(&line);
        }
        if let Some(issue) = self.store.status_issue()? {
            return Err(Error::Message(issue));
        }
        Ok(())
    }

    fn refresh_usage(&self, slot: u32) -> Result<()> {
        let config = &self.store.config;
        if config.skip_usage_refresh || self.store.usage_fresh(slot) {
            return Ok(());
        }
        let previous = self.store.usage(slot);
        if writers_running(config) {
            if config.app_server_socket.exists()
                && AuthDocument::read(&config.active_auth)
                    .ok()
                    .and_then(|active| {
                        self.store
                            .slot_for_identity(&active.identity)
                            .ok()
                            .flatten()
                    })
                    == Some(slot)
            {
                touch_private(&config.usage_attempt(slot))?;
                let result = query_shared(&config.app_server_socket);
                write_usage_result(
                    previous.as_ref(),
                    &result.usage,
                    &config.profile_usage(slot),
                )?;
                return Ok(());
            }
            return self.refresh_usage_read_only(slot, previous.as_ref());
        }
        if self.store.selected() != Some(slot) {
            return self.refresh_usage_read_only(slot, previous.as_ref());
        }

        touch_private(&config.usage_attempt(slot))?;
        let source = config.profile_auth(slot);
        let original = AuthDocument::read(&source)?;
        let home = prepare_offline_home(config, &source)?;
        let auth_path = home.path().join("auth.json");
        let mut barrier = self.store.begin_barrier()?;
        barrier.mark_refreshing(Some(slot), auth_path, true, false, false)?;
        let result = query_offline(home.path());
        let Some(_) = result.refreshed_auth else {
            barrier.rollback()?;
            write_usage_result(
                previous.as_ref(),
                &result.usage,
                &config.profile_usage(slot),
            )?;
            return Ok(());
        };
        let refreshed = match AuthDocument::read(home.path().join("auth.json")) {
            Ok(refreshed) => refreshed,
            Err(error) => {
                barrier.rollback()?;
                return Err(error);
            }
        };
        if refreshed.identity != original.identity {
            barrier.rollback()?;
            return Err(Error::Message(
                "app server refreshed credentials for a different account".into(),
            ));
        }
        if refreshed.same_credentials(&original) {
            barrier.rollback()?;
            write_usage_result(
                previous.as_ref(),
                &result.usage,
                &config.profile_usage(slot),
            )?;
            return Ok(());
        }
        let commit =
            barrier.commit_profile(slot, &home.path().join("auth.json"), true, false, false);
        match commit {
            Ok(()) => write_usage_result(
                previous.as_ref(),
                &result.usage,
                &config.profile_usage(slot),
            ),
            Err(Error::RecoveryDeferred) => {
                eprintln!(
                    "{WARNING}!{WARNING:#} Codex started after OAuth rotation; refreshed credentials will commit after it exits."
                );
                Ok(())
            }
            Err(error) => {
                let _ = home.keep();
                Err(error)
            }
        }
    }

    fn switch(&self, selector: &str) -> Result<()> {
        self.recover_locked()?;
        let _lock = self.store.lock()?;
        refuse_writers(&self.store.config)?;
        let target = self.store.resolve(selector)?;
        if self
            .store
            .usage(target.slot)
            .is_some_and(|usage| usage.exhausted_now(now_epoch()))
        {
            eprintln!(
                "{WARNING}warning{WARNING:#}: account {ACCENT}{}{ACCENT:#} ({EMPHASIS}{}{EMPHASIS:#}) was last seen exhausted -- {ERROR}{}{ERROR:#}",
                target.slot,
                target.auth.identity.email,
                self.store.usage(target.slot).unwrap().label(now_epoch())
            );
        }
        self.store.ensure_session_link()?;
        self.store.sync_or_restore_selected()?;
        let barrier = self.store.begin_barrier()?;
        barrier.commit_switch(target.slot, true)?;
        propagate_codex_home(&self.store.config);
        for line in self.store.status_lines()? {
            print_status_line(&line);
        }
        println!(
            "{SUCCESS}✓{SUCCESS:#} New Codex launches will use this account; session history remains shared."
        );
        Ok(())
    }

    fn refresh_usage_read_only(&self, slot: u32, previous: Option<&UsageRecord>) -> Result<()> {
        let config = &self.store.config;
        touch_private(&config.usage_attempt(slot))?;
        let source = config.profile_auth(slot);
        let home = prepare_offline_home(config, &source)?;
        let result = query_offline_read_only(home.path());
        write_usage_result(previous, &result.usage, &config.profile_usage(slot))
    }

    fn add(&self, options: &[OsString]) -> Result<()> {
        reject_non_oauth(options)?;
        self.recover_locked()?;
        let _lock = self.store.lock()?;
        let mut login = StagedLogin::prepare(&self.store.config)?;
        let barrier = self.store.begin_enrollment(login.auth_path().to_owned())?;
        if let Err(error) = login.execute(options) {
            let revoke = login.revoke();
            barrier.rollback()?;
            revoke?;
            return Err(error);
        }
        let fresh = match AuthDocument::read(login.auth_path()) {
            Ok(fresh) => fresh,
            Err(error) => {
                let revoke = login.revoke();
                barrier.rollback()?;
                revoke?;
                return Err(error);
            }
        };
        if let Some(slot) = self.store.slot_for_identity(&fresh.identity)? {
            let revoke = login.revoke();
            barrier.rollback()?;
            revoke?;
            return Err(Error::Message(format!(
                "{} is already enrolled as account {slot}; nothing was added.",
                fresh.identity.email
            )));
        }
        let slot = self.store.next_slot()?;
        if let Err(error) = barrier.commit_profile(slot, login.auth_path(), false, false, false) {
            login.preserve();
            return Err(error);
        }
        login.accept();
        println!(
            "\n{SUCCESS}✓{SUCCESS:#} Enrolled {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{slot}{ACCENT:#}.",
            fresh.identity.email
        );
        println!("{MUTED}Next:{MUTED:#} switch to it with {ACCENT}cxa {slot}{ACCENT:#}");
        Ok(())
    }

    fn import(&self, auth_file: &Path) -> Result<()> {
        self.recover_locked()?;
        let _lock = self.store.lock()?;
        let imported = AuthDocument::read(auth_file)?;
        if let Some(slot) = self.store.slot_for_identity(&imported.identity)? {
            return Err(Error::Message(format!(
                "{} is already enrolled as account {slot}; nothing was imported.",
                imported.identity.email
            )));
        }
        let slot = self.store.next_slot()?;
        let barrier = self.store.begin_enrollment(auth_file.to_owned())?;
        barrier.commit_profile(slot, auth_file, false, false, false)?;
        println!(
            "{SUCCESS}✓{SUCCESS:#} Imported {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{slot}{ACCENT:#}.",
            imported.identity.email
        );
        println!("{MUTED}Next:{MUTED:#} switch to it with {ACCENT}cxa {slot}{ACCENT:#}");
        Ok(())
    }

    fn relogin(&self, selector: &str, options: &[OsString]) -> Result<()> {
        reject_non_oauth(options)?;
        self.recover_locked()?;
        let _lock = self.store.lock()?;
        refuse_writers(&self.store.config)?;
        self.store.sync_or_restore_selected()?;
        let target = self.store.resolve(selector)?;
        let selected = self.store.selected();
        println!(
            "{ACCENT}Re-authenticating account {}{ACCENT:#} ({EMPHASIS}{}{EMPHASIS:#}). {MUTED}The shared session stays on the current account.{MUTED:#}\n",
            target.slot, target.auth.identity.email
        );
        let mut login = StagedLogin::prepare(&self.store.config)?;
        let mut barrier = self.store.begin_barrier()?;
        let first_selection = selected.is_none();
        let activate = selected == Some(target.slot) || first_selection;
        barrier.mark_refreshing(
            Some(target.slot),
            login.auth_path().to_owned(),
            activate,
            first_selection,
            true,
        )?;
        if let Err(error) = login.execute(options) {
            let revoke = login.revoke();
            barrier.rollback()?;
            revoke?;
            return Err(error);
        }
        let fresh = match AuthDocument::read(login.auth_path()) {
            Ok(fresh) => fresh,
            Err(error) => {
                let revoke = login.revoke();
                barrier.rollback()?;
                revoke?;
                return Err(error);
            }
        };
        if fresh.identity != target.auth.identity {
            let revoke = login.revoke();
            barrier.rollback()?;
            revoke?;
            return Err(Error::Message(format!(
                "Signed in to the wrong workspace for account {} ({}). Nothing was changed.",
                target.slot, target.auth.identity.email
            )));
        }
        match barrier.commit_profile(
            target.slot,
            login.auth_path(),
            activate,
            first_selection,
            true,
        ) {
            Ok(()) => {}
            Err(Error::RecoveryDeferred) => {
                login.accept();
                eprintln!(
                    "{WARNING}!{WARNING:#} Codex started after OAuth rotation; refreshed credentials will commit after it exits."
                );
                return Ok(());
            }
            Err(error) => {
                login.preserve();
                return Err(error);
            }
        }
        login.accept();
        remove_file_if_exists(&self.store.config.profile_usage(target.slot))?;
        remove_file_if_exists(&self.store.config.usage_attempt(target.slot))?;
        if activate {
            println!(
                "{SUCCESS}✓{SUCCESS:#} Account {ACCENT}{}{ACCENT:#} is selected, so the shared active credentials were updated too.",
                target.slot
            );
        }
        println!(
            "\n{SUCCESS}✓{SUCCESS:#} Re-authenticated {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{}{ACCENT:#}.",
            fresh.identity.email, target.slot
        );
        Ok(())
    }

    fn relink(&self) -> Result<()> {
        self.recover_locked()?;
        let _lock = self.store.lock()?;
        refuse_writers(&self.store.config)?;
        self.store.ensure_session_link()?;
        self.store.sync_or_restore_selected()?;
        let selected = self
            .store
            .selected()
            .ok_or_else(|| Error::Message("No default Codex account is selected.".into()))?;
        let barrier = self.store.begin_barrier()?;
        barrier.commit_switch(selected, true)?;
        for line in self.store.status_lines()? {
            print_status_line(&line);
        }
        Ok(())
    }

    fn service_guard(&self) -> Result<()> {
        let _lock = self.store.lock()?;
        if codex_writer_status() == WriterStatus::Running {
            return Err(Error::Message(
                "Another Codex process is already running; refusing a second refresh owner.".into(),
            ));
        }
        remove_file_if_exists(&self.store.config.server_start_marker)?;
        self.store.recover()?;
        if fs::read_link(&self.store.config.session_auth)
            .ok()
            .as_deref()
            != Some(&self.store.config.active_auth)
        {
            return Err(Error::Message(
                "Session credentials are detached from the shared active file. Stop Codex processes and run: cxa relink"
                    .into(),
            ));
        }
        let selected = self
            .store
            .selected()
            .ok_or_else(|| Error::Message("No default Codex account is selected".into()))?;
        let active = AuthDocument::read(&self.store.config.active_auth)?;
        if self.store.slot_for_identity(&active.identity)? != Some(selected) {
            return Err(Error::Message(
                "Active credentials do not match the selected account; shared app server not started."
                    .into(),
            ));
        }
        touch_private(&self.store.config.server_start_marker)
    }

    fn service_release(&self) -> Result<()> {
        let _lock = self.store.lock()?;
        remove_file_if_exists(&self.store.config.server_start_marker)
    }
}

fn usage_style(label: &str) -> Style {
    if label.contains("EXHAUSTED") || label.contains("100% used") {
        return ERROR;
    }
    let primary_percent = label
        .split_whitespace()
        .find_map(|word| word.strip_suffix('%'))
        .and_then(|percent| percent.parse::<u8>().ok());
    if label.contains("unknown") || primary_percent.is_some_and(|percent| percent >= 80) {
        WARNING
    } else {
        SUCCESS
    }
}

fn print_status_line(line: &str) {
    let Some((label, value)) = line.split_once(": ") else {
        println!("{line}");
        return;
    };
    let value_style = match label {
        "Default Codex account" => EMPHASIS,
        "Quota" => usage_style(value),
        "Session credentials"
            if value.contains("DETACHED")
                || value.contains("MISSING")
                || value.contains("does not match") =>
        {
            WARNING
        }
        "Session credentials" => SUCCESS,
        "Active credentials" if value.contains("does not match") => WARNING,
        _ => Style::new(),
    };
    println!("{ACCENT}{label}{ACCENT:#}: {value_style}{value}{value_style:#}");
}

fn write_usage_result(
    previous: Option<&UsageRecord>,
    next: &UsageRecord,
    path: &Path,
) -> Result<()> {
    if next.succeeded() || previous.is_none_or(|usage| !usage.succeeded()) {
        next.write(path)?;
    }
    Ok(())
}

fn touch_private(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::io(path, error))?;
    file.write_all(b"").map_err(|error| Error::io(path, error))
}

fn refuse_writers(config: &Config) -> Result<()> {
    if !writers_running(config) {
        return Ok(());
    }
    if config.app_server_socket.exists() {
        eprintln!(
            "{WARNING}!{WARNING:#} Stop the codex-quota-proxy socket, then the codex-shared-app-server service, before changing credentials."
        );
    }
    Err(Error::WriterRunning)
}

fn reject_non_oauth(options: &[OsString]) -> Result<()> {
    for option in options {
        if matches!(
            option.to_str(),
            Some("--with-api-key" | "--with-access-token")
        ) {
            return Err(Error::Message(format!(
                "cxa requires ChatGPT OAuth credentials; {} is not supported.",
                option.to_string_lossy()
            )));
        }
    }
    Ok(())
}

struct StagedLogin {
    home: Option<tempfile::TempDir>,
    auth_path: PathBuf,
    accepted: bool,
}

impl StagedLogin {
    fn prepare(config: &Config) -> Result<Self> {
        let home = Builder::new()
            .prefix(".enroll-")
            .tempdir_in(&config.account_store)
            .map_err(|error| Error::io(&config.account_store, error))?;
        let source_config = config.codex_home.join("config.toml");
        if source_config.is_file() {
            fs::copy(&source_config, home.path().join("config.toml"))
                .map_err(|error| Error::io(&source_config, error))?;
        }
        let auth_path = home.path().join("auth.json");
        Ok(Self {
            home: Some(home),
            auth_path,
            accepted: false,
        })
    }

    fn execute(&mut self, options: &[OsString]) -> Result<()> {
        let home = self
            .home
            .as_ref()
            .ok_or_else(|| Error::Message("staged login is no longer available".into()))?;
        let status = Command::new("codex")
            .arg("login")
            .args(options)
            .args(["-c", "cli_auth_credentials_store=\"file\""])
            .env("CODEX_HOME", home.path())
            .status()
            .map_err(|error| Error::io("codex login", error))?;
        if !status.success() || !self.auth_path.is_file() {
            return Err(Error::Message(
                "Codex login failed; nothing was changed.".into(),
            ));
        }
        Ok(())
    }

    fn auth_path(&self) -> &Path {
        &self.auth_path
    }

    fn accept(&mut self) {
        self.accepted = true;
    }

    fn preserve(&mut self) {
        self.accepted = true;
        if let Some(home) = self.home.take() {
            let _ = home.keep();
        }
    }

    fn revoke(mut self) -> Result<()> {
        let Some(home) = self.home.take() else {
            return Ok(());
        };
        if !self.auth_path.exists() {
            return Ok(());
        }
        let status = Command::new("codex")
            .arg("logout")
            .env("CODEX_HOME", home.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => {
                let path = home.keep();
                Err(Error::Message(format!(
                    "Codex logout failed; rejected credentials were preserved at {} for safe cleanup.",
                    path.display()
                )))
            }
            Err(error) => {
                let path = home.keep();
                Err(Error::Message(format!(
                    "Could not run Codex logout ({error}); rejected credentials were preserved at {} for safe cleanup.",
                    path.display()
                )))
            }
        }
    }
}

impl Drop for StagedLogin {
    fn drop(&mut self) {
        let Some(home) = self.home.take() else {
            return;
        };
        if self.accepted || !self.auth_path.exists() {
            return;
        }
        let revoked = Command::new("codex")
            .arg("logout")
            .env("CODEX_HOME", home.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !revoked {
            let _ = home.keep();
        }
    }
}

fn propagate_codex_home(config: &Config) {
    let value = config.codex_home.to_string_lossy();
    let _ = Command::new("systemctl")
        .args(["--user", "set-environment", &format!("CODEX_HOME={value}")])
        .status();
    if Command::new("tmux")
        .arg("list-sessions")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        let _ = Command::new("tmux")
            .args(["set-environment", "-g", "CODEX_HOME", &value])
            .status();
    }
}

use std::os::unix::fs::OpenOptionsExt;
