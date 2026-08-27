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
use crate::fs::{
    ExclusiveLock, OWNED_TEMP_MARKER, STAGING_WRITER_LOCK, atomic_write, private_dir,
    remove_file_if_exists,
};
use crate::process::{WriterStatus, codex_writer_status, mark_service_starting, writers_running};
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

    fn recover_locked(&self) -> Result<ExclusiveLock> {
        let lock = self.store.lock()?;
        self.store.recover()?;
        self.cleanup_orphaned_homes()?;
        Ok(lock)
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
            if !entry.path().join(OWNED_TEMP_MARKER).is_file() {
                continue;
            }
            if name.starts_with(".quota-") {
                fs::remove_dir_all(entry.path()).map_err(|error| Error::io(entry.path(), error))?;
                continue;
            }
            if !name.starts_with(".enroll-") || !entry.path().is_dir() {
                continue;
            }
            let auth = entry.path().join("auth.json");
            if auth.is_file() {
                let status =
                    logout_file_credentials(self.store.config.codex_binary(), &entry.path())
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
        let _lock = self.recover_locked()?;
        self.store.adopt_detached_session_selection()?;

        let profiles = self.store.profiles()?;
        if !profiles.is_empty() {
            if let Some(selected) = self.store.selected() {
                let profile = self.store.resolve(&selected.to_string())?;
                println!(
                    "{SUCCESS}✓{SUCCESS:#} cxa is already initialized with account {ACCENT}{selected}{ACCENT:#} ({EMPHASIS}{}{EMPHASIS:#}).",
                    profile.auth.identity.label()
                );
                if !self.store.config.session_links_to_active() {
                    print_relink_warning();
                }
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
            if !confirmation_is_interactive(io::stdin().is_terminal(), io::stdout().is_terminal()) {
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
        let barrier = self.store.begin_profile_commit()?;
        barrier.commit_profile(slot, session_path, &current, false, true, false)?;

        let linked = if writers_running(&self.store.config) {
            false
        } else {
            match self
                .store
                .begin_barrier()
                .and_then(|barrier| barrier.commit_switch(slot, true))
            {
                Ok(()) => true,
                Err(Error::WriterRunning | Error::RecoveryDeferred) => false,
                Err(error) => return Err(error),
            }
        };

        self.refresh_usage(slot)?;
        println!(
            "{SUCCESS}✓{SUCCESS:#} Imported {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{slot}{ACCENT:#}.",
            current.identity.label()
        );
        println!("{SUCCESS}✓{SUCCESS:#} Account {ACCENT}{slot}{ACCENT:#} is now selected.");
        propagate_codex_home(&self.store.config);
        let now = now_epoch();
        let usage_record = self.store.usage(slot);
        let usage = usage_record
            .as_ref()
            .map(|usage| usage.label(now))
            .unwrap_or_else(|| "usage unknown".into());
        let usage_style = usage_style(usage_record.as_ref(), now);
        println!("{ACCENT}Quota{ACCENT:#}: {usage_style}{usage}{usage_style:#}");
        if !linked {
            print_relink_warning();
        }
        println!("{MUTED}Next:{MUTED:#} add another account with {ACCENT}cxa add{ACCENT:#}");
        Ok(())
    }

    fn initialization_guidance(&self) -> String {
        match AuthDocument::read(&self.store.config.session_auth) {
            Ok(current) => format!(
                "Found the current Codex login: {}\ncxa is not initialized. Run: cxa init",
                current.identity.label()
            ),
            Err(_) => "cxa is not initialized. Run `codex login`, then run `cxa init`.".into(),
        }
    }

    fn list(&self) -> Result<()> {
        let _lock = self.recover_locked()?;
        self.store.adopt_detached_session_selection()?;
        self.store.reconcile_if_idle()?;
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
            .map(|profile| profile.auth.identity.label().len())
            .max()
            .unwrap_or_default();
        let now = now_epoch();
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
            let usage_record = self.store.usage(profile.slot);
            let usage = usage_record
                .as_ref()
                .map(|usage| usage.label(now))
                .unwrap_or_else(|| "usage unknown".into());
            let usage_style = usage_style(usage_record.as_ref(), now);
            println!(
                "{selected_style}{marker}{selected_style:#} {ACCENT}{}{ACCENT:#}  {EMPHASIS}{:width$}{EMPHASIS:#}  {usage_style}{usage}{usage_style:#}",
                profile.slot,
                profile.auth.identity.label()
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
        let _lock = self.recover_locked()?;
        self.store.adopt_detached_session_selection()?;
        if self.store.profiles()?.is_empty() {
            return Err(Error::Message(self.initialization_guidance()));
        }
        self.store.reconcile_if_idle()?;
        if refresh {
            if let Some(slot) = self.store.selected() {
                self.refresh_usage(slot)?;
            }
        }
        self.print_status_lines()?;
        if let Some(issue) = self.store.status_issue()? {
            return Err(Error::Message(issue));
        }
        Ok(())
    }

    fn print_status_lines(&self) -> Result<()> {
        let now = now_epoch();
        let usage = self
            .store
            .selected()
            .and_then(|slot| self.store.usage(slot));
        let quota_style = usage_style(usage.as_ref(), now);
        for line in self.store.status_lines()? {
            print_status_line(&line, quota_style);
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
            let session_slot = AuthDocument::read(&config.session_auth)
                .ok()
                .and_then(|auth| self.store.slot_for_identity(&auth.identity).ok().flatten());
            let active_slot = AuthDocument::read(&config.active_auth)
                .ok()
                .and_then(|auth| self.store.slot_for_identity(&auth.identity).ok().flatten());
            let live_slot = session_slot.or(active_slot).or(self.store.selected());
            if config.app_server_socket.exists() && active_slot == Some(slot) {
                touch_private(&config.usage_attempt(slot))?;
                let result = query_shared(&config.app_server_socket);
                write_usage_result(
                    previous.as_ref(),
                    &result.usage,
                    &config.profile_usage(slot),
                )?;
                return Ok(());
            }
            if live_slot == Some(slot) {
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
        let result = query_offline(config, home.path());
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
        if !refreshed.identity.same_account(&original.identity) {
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
        let commit = barrier.commit_profile(
            slot,
            &home.path().join("auth.json"),
            &refreshed,
            true,
            false,
            false,
        );
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
        let _lock = self.recover_locked()?;
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
                target.auth.identity.label(),
                self.store.usage(target.slot).unwrap().label(now_epoch())
            );
        }
        let barrier = self.store.begin_barrier()?;
        barrier.commit_switch(target.slot, true)?;
        propagate_codex_home(&self.store.config);
        self.print_status_lines()?;
        println!(
            "{SUCCESS}✓{SUCCESS:#} New Codex launches will use this account; session history remains shared."
        );
        Ok(())
    }

    fn refresh_usage_read_only(&self, slot: u32, previous: Option<&UsageRecord>) -> Result<()> {
        let config = &self.store.config;
        touch_private(&config.usage_attempt(slot))?;
        let source = config.profile_auth(slot);
        let original = AuthDocument::read(&source)?;
        let home = prepare_offline_home(config, &source)?;
        let auth_path = home.path().join("auth.json");
        let mut barrier = self.store.begin_profile_commit()?;
        barrier.mark_refreshing(Some(slot), auth_path.clone(), false, false, false)?;
        let result = query_offline_read_only(config, home.path());
        if result.refreshed_auth.is_some() {
            let refreshed = match AuthDocument::read(&auth_path) {
                Ok(refreshed) => refreshed,
                Err(error) => {
                    let _ = home.keep();
                    return Err(error);
                }
            };
            if !refreshed.identity.same_account(&original.identity) {
                let _ = home.keep();
                return Err(Error::Message(
                    "app server refreshed credentials for a different account".into(),
                ));
            }
            if !refreshed.same_credentials(&original) {
                if let Err(error) =
                    barrier.commit_profile(slot, &auth_path, &refreshed, false, false, false)
                {
                    let _ = home.keep();
                    return Err(error);
                }
            } else {
                barrier.rollback()?;
            }
        } else {
            barrier.rollback()?;
        }
        write_usage_result(previous, &result.usage, &config.profile_usage(slot))
    }

    fn add(&self, options: &[OsString]) -> Result<()> {
        reject_non_oauth(options)?;
        let _lock = self.recover_locked()?;
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
                fresh.identity.label()
            )));
        }
        let slot = self.store.next_slot()?;
        if let Err(error) =
            barrier.commit_profile(slot, login.auth_path(), &fresh, false, false, false)
        {
            login.preserve();
            return Err(error);
        }
        login.accept();
        println!(
            "\n{SUCCESS}✓{SUCCESS:#} Enrolled {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{slot}{ACCENT:#}.",
            fresh.identity.label()
        );
        println!("{MUTED}Next:{MUTED:#} switch to it with {ACCENT}cxa {slot}{ACCENT:#}");
        Ok(())
    }

    fn import(&self, auth_file: &Path) -> Result<()> {
        let _lock = self.recover_locked()?;
        let imported = AuthDocument::read(auth_file)?;
        if let Some(slot) = self.store.slot_for_identity(&imported.identity)? {
            return Err(Error::Message(format!(
                "{} is already enrolled as account {slot}; nothing was imported.",
                imported.identity.label()
            )));
        }
        let slot = self.store.next_slot()?;
        let barrier = self.store.begin_profile_commit()?;
        barrier.commit_profile(slot, auth_file, &imported, false, false, false)?;
        println!(
            "{SUCCESS}✓{SUCCESS:#} Imported {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{slot}{ACCENT:#}.",
            imported.identity.label()
        );
        println!("{MUTED}Next:{MUTED:#} switch to it with {ACCENT}cxa {slot}{ACCENT:#}");
        Ok(())
    }

    fn relogin(&self, selector: &str, options: &[OsString]) -> Result<()> {
        reject_non_oauth(options)?;
        let _lock = self.recover_locked()?;
        refuse_writers(&self.store.config)?;
        let target = self.store.resolve(selector)?;
        println!(
            "{ACCENT}Re-authenticating account {}{ACCENT:#} ({EMPHASIS}{}{EMPHASIS:#}). {MUTED}The shared session stays on the current account.{MUTED:#}\n",
            target.slot,
            target.auth.identity.label()
        );
        let mut login = StagedLogin::prepare(&self.store.config)?;
        let mut barrier = self.store.begin_barrier()?;
        let selected = self.store.selected();
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
        if !fresh.identity.same_account(&target.auth.identity) {
            let revoke = login.revoke();
            barrier.rollback()?;
            revoke?;
            return Err(Error::Message(format!(
                "Signed in to the wrong workspace for account {} ({}). Nothing was changed.",
                target.slot,
                target.auth.identity.label()
            )));
        }
        match barrier.commit_profile(
            target.slot,
            login.auth_path(),
            &fresh,
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
            propagate_codex_home(&self.store.config);
            println!(
                "{SUCCESS}✓{SUCCESS:#} Account {ACCENT}{}{ACCENT:#} is selected, so the shared active credentials were updated too.",
                target.slot
            );
        }
        println!(
            "\n{SUCCESS}✓{SUCCESS:#} Re-authenticated {EMPHASIS}{}{EMPHASIS:#} as account {ACCENT}{}{ACCENT:#}.",
            fresh.identity.label(),
            target.slot
        );
        Ok(())
    }

    fn relink(&self) -> Result<()> {
        let _lock = self.recover_locked()?;
        refuse_writers(&self.store.config)?;
        let barrier = self.store.begin_barrier()?;
        let selected = self
            .store
            .selected()
            .ok_or_else(|| Error::Message("No default Codex account is selected.".into()))?;
        barrier.commit_switch(selected, true)?;
        propagate_codex_home(&self.store.config);
        self.print_status_lines()?;
        Ok(())
    }

    fn service_guard(&self) -> Result<()> {
        let _lock = self.store.lock()?;
        if codex_writer_status(&self.store.config) != WriterStatus::Stopped {
            return Err(Error::Message(
                "Another Codex process is already running; refusing a second refresh owner.".into(),
            ));
        }
        remove_file_if_exists(&self.store.config.server_start_marker)?;
        self.store.recover()?;
        self.cleanup_orphaned_homes()?;
        if !self.store.config.session_links_to_active() {
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
        mark_service_starting(&self.store.config)
    }

    fn service_release(&self) -> Result<()> {
        let _lock = self.store.lock()?;
        remove_file_if_exists(&self.store.config.server_start_marker)
    }
}

fn print_relink_warning() {
    println!(
        "{WARNING}!{WARNING:#} Codex session credentials are detached; run {ACCENT}cxa relink{ACCENT:#} after Codex stops to enable switching."
    );
}

fn confirmation_is_interactive(stdin: bool, stdout: bool) -> bool {
    stdin && stdout
}

fn usage_style(usage: Option<&UsageRecord>, now: i64) -> Style {
    let Some(usage) = usage else {
        return WARNING;
    };
    let max_used = usage.max_current_used_percent(now);
    if usage.exhausted_now(now) || max_used.is_some_and(|percent| percent >= 100.0) {
        return ERROR;
    }
    if !usage.succeeded()
        || usage.credits_depleted()
        || max_used.is_some_and(|percent| percent >= 80.0)
    {
        WARNING
    } else {
        SUCCESS
    }
}

fn print_status_line(line: &str, quota_style: Style) {
    let Some((label, value)) = line.split_once(": ") else {
        println!("{line}");
        return;
    };
    let value_style = match label {
        "Default Codex account" => EMPHASIS,
        "Quota" => quota_style,
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
        let Some(option) = option.to_str() else {
            continue;
        };
        let name = option.split_once('=').map_or(option, |(name, _)| name);
        if matches!(name, "--with-api-key" | "--with-access-token") {
            return Err(Error::Message(format!(
                "cxa requires ChatGPT OAuth credentials; {} is not supported.",
                name
            )));
        }
    }
    Ok(())
}

struct StagedLogin {
    home: Option<tempfile::TempDir>,
    _writer_lock: ExclusiveLock,
    auth_path: PathBuf,
    codex_binary: PathBuf,
    accepted: bool,
}

impl StagedLogin {
    fn prepare(config: &Config) -> Result<Self> {
        let home = Builder::new()
            .prefix(".enroll-")
            .tempdir_in(&config.account_store)
            .map_err(|error| Error::io(&config.account_store, error))?;
        atomic_write(&home.path().join(OWNED_TEMP_MARKER), b"cxa\n", 0o600)?;
        let writer_lock =
            ExclusiveLock::acquire_inheritable(&home.path().join(STAGING_WRITER_LOCK))?;
        let source_config = config.codex_home.join("config.toml");
        if source_config.is_file() {
            fs::copy(&source_config, home.path().join("config.toml"))
                .map_err(|error| Error::io(&source_config, error))?;
        }
        let auth_path = home.path().join("auth.json");
        Ok(Self {
            home: Some(home),
            _writer_lock: writer_lock,
            auth_path,
            codex_binary: config.codex_binary().to_owned(),
            accepted: false,
        })
    }

    fn execute(&mut self, options: &[OsString]) -> Result<()> {
        let home = self
            .home
            .as_ref()
            .ok_or_else(|| Error::Message("staged login is no longer available".into()))?;
        let status = Command::new(&self.codex_binary)
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
        let status = logout_file_credentials(&self.codex_binary, home.path());
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
        let revoked = logout_file_credentials(&self.codex_binary, home.path())
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

fn logout_file_credentials(
    codex_binary: &Path,
    home: &Path,
) -> std::io::Result<std::process::ExitStatus> {
    Command::new(codex_binary)
        .arg("logout")
        .args(["-c", "cli_auth_credentials_store=\"file\""])
        .env("CODEX_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

use std::os::unix::fs::OpenOptionsExt;

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{App, confirmation_is_interactive, usage_style};
    use crate::account_store::{Store, UsageRecord, UsageWindow};
    use crate::config::Config;
    use crate::terminal::{ERROR, SUCCESS, WARNING};

    #[test]
    fn confirmation_requires_visible_interactive_input_and_output() {
        assert!(confirmation_is_interactive(true, true));
        assert!(!confirmation_is_interactive(false, true));
        assert!(!confirmation_is_interactive(true, false));
    }

    #[test]
    fn quota_style_uses_fractional_usage_from_every_window() {
        let usage = UsageRecord {
            primary_window: Some(UsageWindow {
                used_percent: Some(10.0),
                ..UsageWindow::default()
            }),
            secondary_window: Some(UsageWindow {
                used_percent: Some(99.5),
                ..UsageWindow::default()
            }),
            ..UsageRecord::default()
        };
        assert_eq!(usage_style(Some(&usage), 10_000), WARNING);

        let exhausted = UsageRecord {
            individual_window: Some(UsageWindow {
                used_percent: Some(100.0),
                ..UsageWindow::default()
            }),
            ..UsageRecord::default()
        };
        assert_eq!(usage_style(Some(&exhausted), 10_000), ERROR);

        let healthy = UsageRecord {
            primary_window: Some(UsageWindow {
                used_percent: Some(25.5),
                ..UsageWindow::default()
            }),
            ..UsageRecord::default()
        };
        assert_eq!(usage_style(Some(&healthy), 10_000), SUCCESS);

        let expired = UsageRecord {
            primary_window: Some(UsageWindow {
                used_percent: Some(100.0),
                resets_at: Some(9_999),
                ..UsageWindow::default()
            }),
            reached: true,
            ..UsageRecord::default()
        };
        assert_eq!(usage_style(Some(&expired), 10_000), SUCCESS);
        assert!(!expired.label(10_000).contains("EXHAUSTED"));

        let depleted = UsageRecord {
            has_credits: Some(false),
            unlimited: Some(false),
            balance: Some(serde_json::json!("25")),
            ..UsageRecord::default()
        };
        assert_eq!(usage_style(Some(&depleted), 10_000), WARNING);
        assert!(depleted.label(10_000).contains("no credits"));
        assert_eq!(usage_style(None, 10_000), WARNING);
    }

    #[test]
    fn recovery_returns_the_lock_held_for_the_calling_command() {
        let root = tempfile::tempdir().unwrap();
        let account_store = root.path().join("store");
        let config = Config {
            codex_home: root.path().join("codex"),
            codex_binary: None,
            active_auth: account_store.join("auth.json"),
            account_store: account_store.clone(),
            active_profile: account_store.join("active-profile"),
            switch_lock: account_store.join("switch.lock"),
            server_start_marker: account_store.join("starting"),
            app_server_socket: account_store.join("app-server.sock"),
            session_auth: root.path().join("codex/auth.json"),
            usage_ttl_seconds: 120,
            skip_usage_refresh: true,
        };
        let app = App::new(config.clone());

        let recovery_lock = app.recover_locked().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let contender = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _lock = Store::new(config).lock().unwrap();
            acquired_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        drop(recovery_lock);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        contender.join().unwrap();
    }
}
