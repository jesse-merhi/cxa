use std::ffi::OsString;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anstream::{eprintln, print, println};
use anstyle::Style;
use clap::{Parser, Subcommand};
use tempfile::Builder;

use crate::account_store::{Store, UsageRecord, now_epoch};
use crate::app_server::{query_profile, require_file_credentials};
use crate::auth::AuthDocument;
use crate::config::Config;
use crate::fs::{ExclusiveLock, atomic_copy, private_dir, remove_file_if_exists};
use crate::terminal::{ACCENT, EMPHASIS, ERROR, MUTED, SUCCESS, WARNING};
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
    /// List enrolled accounts and their last known quota.
    List,
    /// Show the selected account and credential-file state.
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
}

pub fn run(cli: Cli, config: Config) -> Result<()> {
    require_file_credentials(&config)?;
    let app = App::new(config);
    match (cli.command, cli.account) {
        (Some(CliCommand::Init { yes }), _) => app.init(yes),
        (Some(CliCommand::List), _) => app.list(),
        (Some(CliCommand::Status), _) | (None, None) => app.status(true),
        (Some(CliCommand::Use { account }), _) | (None, Some(account)) => app.switch(&account),
        (Some(CliCommand::Add { options }), _) => app.add(&options),
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

    fn list(&self) -> Result<()> {
        let _lock = self.locked()?;
        let profiles = self.store.profiles()?;
        if profiles.is_empty() {
            println!("{}", self.initialization_guidance());
            return Ok(());
        }
        let mut session_changed = false;
        for profile in &profiles {
            session_changed |= self.refresh_usage(profile.slot)?;
        }
        let selected = self.store.selected();
        for profile in profiles {
            let marker = if selected == Some(profile.slot) {
                "*"
            } else {
                " "
            };
            let usage = self
                .store
                .usage(profile.slot)
                .map(|usage| usage.label(now_epoch()))
                .unwrap_or_else(|| "usage unknown".into());
            println!(
                "{ACCENT}{marker} {}{ACCENT:#}  {EMPHASIS}{}{EMPHASIS:#}  {MUTED}{usage}{MUTED:#}",
                profile.slot,
                profile.auth.identity.label()
            );
        }
        if session_changed {
            restart_notice();
        }
        Ok(())
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
        let session_changed = refresh && self.refresh_usage(selected)?;
        self.print_status_lines()?;
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
        if self.store.config.skip_usage_refresh || self.store.usage_fresh(slot) {
            return Ok(false);
        }
        let previous = self.store.usage(slot);
        let (next, session_changed) =
            query_profile(&self.store.config, &self.store.config.profile_auth(slot));
        write_usage_result(
            previous.as_ref(),
            &next,
            &self.store.config.profile_usage(slot),
        )?;
        Ok(session_changed)
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

    fn add(&self, options: &[OsString]) -> Result<()> {
        reject_non_oauth(options)?;
        let _lock = self.locked()?;
        let login = StagedLogin::run(&self.store.config, options)?;
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

    fn print_status_lines(&self) -> Result<()> {
        let selected = self.store.selected();
        let usage = selected.and_then(|slot| self.store.usage(slot));
        let quota_style = usage_style(usage.as_ref(), now_epoch());
        for line in self.store.status_lines()? {
            print_status_line(&line, quota_style);
        }
        Ok(())
    }
}

fn restart_notice() {
    println!(
        "{WARNING}!{WARNING:#} Restart Codex or ChatGPT before expecting an existing session to use this account."
    );
}

fn usage_style(usage: Option<&UsageRecord>, now: i64) -> Style {
    let Some(usage) = usage else {
        return WARNING;
    };
    let max_used = usage.max_current_used_percent(now);
    if usage.exhausted_now(now) || max_used.is_some_and(|percent| percent >= 100.0) {
        return ERROR;
    }
    if !usage.succeeded() || max_used.is_some_and(|percent| percent >= 80.0) {
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
        "Selected Codex account" => EMPHASIS,
        "Quota" => quota_style,
        "Credential file" if value.starts_with("matches") => SUCCESS,
        "Credential file" => WARNING,
        _ => Style::new(),
    };
    println!("{ACCENT}{label}{ACCENT:#}: {value_style}{value}{value_style:#}");
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
}
