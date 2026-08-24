use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{AuthDocument, Identity};
use crate::config::Config;
use crate::fs::{ExclusiveLock, atomic_copy, atomic_write, private_dir, remove_file_if_exists};
use crate::process::writers_running;
use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct Profile {
    pub slot: u32,
    pub auth: AuthDocument,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UsageWindow {
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub window_minutes: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UsageRecord {
    pub observed_at: i64,
    pub primary_window: Option<UsageWindow>,
    pub secondary_window: Option<UsageWindow>,
    pub individual_window: Option<UsageWindow>,
    #[serde(default)]
    pub reached: bool,
    pub reached_type: Option<String>,
    #[serde(default)]
    pub spend_control_reached: bool,
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<Value>,
    pub plan_type: Option<String>,
    pub error: Option<String>,
}

impl UsageRecord {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).map_err(|error| Error::io(path, error))?;
        serde_json::from_slice(&bytes).map_err(|error| Error::json(path, error))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| Error::Message(format!("could not encode usage: {error}")))?;
        atomic_write(path, &bytes, 0o600)
    }

    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }

    pub fn label(&self, now: i64) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        let mut bits = Vec::new();
        for (label, window) in [
            ("primary", self.primary_window.as_ref()),
            ("secondary", self.secondary_window.as_ref()),
            ("individual", self.individual_window.as_ref()),
        ] {
            let Some(window) = window else { continue };
            if let Some(used) = window.used_percent {
                bits.push(format!("{label} {}% used", format_percent(used)));
            }
            if let Some(reset) = window.resets_at {
                let when = DateTime::from_timestamp(reset, 0)
                    .map(|value| {
                        value
                            .with_timezone(&Local)
                            .format("%b %d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_else(|| reset.to_string());
                let suffix = if reset > now {
                    format!(" (in {}h)", (reset - now) / 3600)
                } else {
                    " (passed)".to_owned()
                };
                bits.push(format!("{label} resets {when}{suffix}"));
            }
        }
        if self.reached {
            bits.push("EXHAUSTED".into());
        }
        if let Some(plan) = &self.plan_type {
            bits.push(plan.clone());
        }
        if self.has_credits == Some(false) && balance_is_zero(self.balance.as_ref()) {
            bits.push("no credits".into());
        }
        let age = now.saturating_sub(self.observed_at);
        bits.push(if age < 3600 {
            "seen just now".into()
        } else {
            format!("seen {}h ago", age / 3600)
        });
        if bits.is_empty() {
            "usage unknown".into()
        } else {
            bits.join(", ")
        }
    }

    pub fn exhausted_now(&self, now: i64) -> bool {
        if !self.reached {
            return false;
        }
        if matches!(
            self.reached_type.as_deref(),
            Some("workspace_owner_credits_depleted" | "workspace_member_credits_depleted")
        ) {
            return true;
        }
        let exhausted: Vec<&UsageWindow> = [
            self.primary_window.as_ref(),
            self.secondary_window.as_ref(),
            self.individual_window.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|window| window.used_percent.unwrap_or_default() >= 100.0)
        .collect();
        exhausted.is_empty()
            || exhausted.iter().any(|window| window.resets_at.is_none())
            || exhausted
                .iter()
                .filter_map(|window| window.resets_at)
                .max()
                .is_some_and(|reset| reset > now)
    }
}

fn format_percent(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

fn balance_is_zero(balance: Option<&Value>) -> bool {
    match balance {
        Some(Value::String(value)) => value == "0",
        Some(Value::Number(value)) => value.as_f64() == Some(0.0),
        _ => false,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum TransactionMode {
    Rollback,
    Refreshing,
    Commit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TransactionState {
    mode: TransactionMode,
    slot: Option<u32>,
    activate: bool,
    select: bool,
    link_session: bool,
    profile_pending: Option<PathBuf>,
    #[serde(default)]
    recovery_source: Option<PathBuf>,
}

pub struct Store {
    pub config: Config,
}

impl Store {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub fn lock(&self) -> Result<ExclusiveLock> {
        private_dir(&self.config.account_store)?;
        ExclusiveLock::acquire(&self.config.switch_lock)
    }

    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let mut profiles = Vec::new();
        let entries = match fs::read_dir(&self.config.account_store) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(profiles),
            Err(error) => return Err(Error::io(&self.config.account_store, error)),
        };
        for entry in entries {
            let entry = entry.map_err(|error| Error::io(&self.config.account_store, error))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(slot) = name
                .strip_prefix("profile-")
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            if let Ok(auth) = AuthDocument::read(entry.path().join("auth.json")) {
                profiles.push(Profile { slot, auth });
            }
        }
        profiles.sort_by_key(|profile| profile.slot);
        Ok(profiles)
    }

    pub fn selected(&self) -> Option<u32> {
        fs::read_to_string(&self.config.active_profile)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .filter(|slot| self.config.profile_dir(*slot).is_dir())
    }

    pub fn resolve(&self, selector: &str) -> Result<Profile> {
        let profiles = self.profiles()?;
        if let Ok(slot) = selector.parse::<u32>() {
            if let Some(profile) = profiles.into_iter().find(|profile| profile.slot == slot) {
                return Ok(profile);
            }
            if self.config.profile_dir(slot).is_dir() {
                return Err(Error::Message(format!(
                    "Codex account {slot} exists, but its credentials are malformed."
                )));
            }
            return Err(Error::Message(format!("No such Codex account: {selector}")));
        }
        let selector = selector.to_lowercase();
        let mut matches = profiles.into_iter().filter(|profile| {
            profile
                .auth
                .identity
                .email
                .to_lowercase()
                .contains(&selector)
        });
        let first = matches
            .next()
            .ok_or_else(|| Error::Message(format!("No Codex account matches: {selector}")))?;
        if matches.next().is_some() {
            return Err(Error::Message(format!(
                "Ambiguous account \"{selector}\"; use its slot number"
            )));
        }
        Ok(first)
    }

    pub fn slot_for_identity(&self, identity: &Identity) -> Result<Option<u32>> {
        Ok(self.profiles()?.into_iter().find_map(|profile| {
            (profile.auth.identity.account_id == identity.account_id
                && profile.auth.identity.user_id == identity.user_id)
                .then_some(profile.slot)
        }))
    }

    pub fn next_slot(&self) -> Result<u32> {
        Ok(self
            .profiles()?
            .into_iter()
            .map(|profile| profile.slot)
            .max()
            .unwrap_or(0)
            + 1)
    }

    pub fn usage(&self, slot: u32) -> Option<UsageRecord> {
        UsageRecord::read(&self.config.profile_usage(slot)).ok()
    }

    pub fn usage_fresh(&self, slot: u32) -> bool {
        let newest = [
            &self.config.profile_usage(slot),
            &self.config.usage_attempt(slot),
        ]
        .into_iter()
        .filter_map(|path| path.metadata().ok())
        .filter_map(|metadata| metadata.modified().ok())
        .max();
        newest
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age < Duration::from_secs(self.config.usage_ttl_seconds))
    }

    pub fn sync_active_to_profile(&self) -> Result<()> {
        if !self.config.active_auth.is_file() {
            return Ok(());
        }
        let active = AuthDocument::read(&self.config.active_auth)?;
        let Some(slot) = self.slot_for_identity(&active.identity)? else {
            return Err(Error::Message(format!(
                "Active credentials ({}) match no enrolled account; not saving them.",
                active.identity.email
            )));
        };
        let target_path = self.config.profile_auth(slot);
        let target = AuthDocument::read(&target_path)?;
        if active.refresh_ns > target.refresh_ns {
            atomic_copy(&self.config.active_auth, &target_path, 0o600)?;
        } else if target.refresh_ns > active.refresh_ns {
            atomic_copy(&target_path, &self.config.active_auth, 0o600)?;
        } else if !active.same_credentials(&target) {
            return Err(Error::Message(
                "Active and stored credentials have the same refresh time but different contents; not choosing one."
                    .into(),
            ));
        }
        if self.selected() != Some(slot) {
            atomic_write(
                &self.config.active_profile,
                format!("{slot}\n").as_bytes(),
                0o600,
            )?;
        }
        Ok(())
    }

    pub fn sync_or_restore_selected(&self) -> Result<()> {
        match self.sync_active_to_profile() {
            Ok(()) => Ok(()),
            Err(Error::Json { .. } | Error::InvalidAuth(_)) => {
                let selected = self.selected().ok_or_else(|| {
                    Error::Message(
                        "Active credentials cannot be identified and no valid account is selected."
                            .into(),
                    )
                })?;
                let profile = self.config.profile_auth(selected);
                AuthDocument::read(&profile)?;
                atomic_copy(&profile, &self.config.active_auth, 0o600)
            }
            Err(error) => Err(error),
        }
    }

    pub fn ensure_session_link(&self) -> Result<()> {
        private_dir(&self.config.codex_home)?;
        if fs::read_link(&self.config.session_auth).ok().as_deref()
            == Some(&self.config.active_auth)
        {
            return Ok(());
        }
        if self.config.session_auth.exists() {
            if let Ok(detached) = AuthDocument::read(&self.config.session_auth) {
                let Some(slot) = self.slot_for_identity(&detached.identity)? else {
                    return Err(Error::Message(format!(
                        "Detached credentials for {} are not enrolled; run cxa add before relinking.",
                        detached.identity.email
                    )));
                };
                let stored = AuthDocument::read(self.config.profile_auth(slot))?;
                if detached.refresh_ns > stored.refresh_ns {
                    atomic_copy(
                        &self.config.session_auth,
                        &self.config.profile_auth(slot),
                        0o600,
                    )?;
                }
            }
            fs::remove_file(&self.config.session_auth)
                .map_err(|error| Error::io(&self.config.session_auth, error))?;
        }
        let temporary = self.config.session_auth.with_extension("json.cxa-link");
        remove_file_if_exists(&temporary)?;
        symlink(&self.config.active_auth, &temporary)
            .map_err(|error| Error::io(&temporary, error))?;
        fs::rename(&temporary, &self.config.session_auth)
            .map_err(|error| Error::io(&self.config.session_auth, error))
    }

    pub fn recover(&self) -> Result<()> {
        let state_path = self.config.transaction_state();
        if !state_path.exists() {
            return Ok(());
        }
        if writers_running(&self.config) {
            return Err(Error::RecoveryDeferred);
        }
        let state = read_state(&state_path)?;
        match state.mode {
            TransactionMode::Rollback => self.restore_hold()?,
            TransactionMode::Refreshing => self.recover_refresh(&state)?,
            TransactionMode::Commit => self.finish_commit(&state)?,
        }
        self.cleanup_transaction(&state)
    }

    pub fn begin_barrier(&self) -> Result<AuthBarrier> {
        if self.config.transaction_state().exists() {
            self.recover()?;
        }
        if writers_running(&self.config) {
            return Err(Error::WriterRunning);
        }
        remove_file_if_exists(&self.config.active_hold())?;
        let state = TransactionState {
            mode: TransactionMode::Rollback,
            slot: None,
            activate: false,
            select: false,
            link_session: false,
            profile_pending: None,
            recovery_source: None,
        };
        write_state(&self.config.transaction_state(), &state)?;
        if self.config.active_auth.exists() {
            fs::rename(&self.config.active_auth, self.config.active_hold())
                .map_err(|error| Error::io(&self.config.active_auth, error))?;
        }
        if writers_running(&self.config) {
            self.restore_hold()?;
            remove_file_if_exists(&self.config.transaction_state())?;
            return Err(Error::WriterRunning);
        }
        Ok(AuthBarrier {
            config: self.config.clone(),
            finished: false,
            irreversible: false,
        })
    }

    fn restore_hold(&self) -> Result<()> {
        if self.config.active_hold().exists() {
            if self.config.active_auth.exists() {
                remove_file_if_exists(&self.config.active_auth)?;
            }
            fs::rename(self.config.active_hold(), &self.config.active_auth)
                .map_err(|error| Error::io(&self.config.active_auth, error))?;
        }
        remove_file_if_exists(&self.config.active_pending())
    }

    fn recover_refresh(&self, state: &TransactionState) -> Result<()> {
        let Some(source) = state.recovery_source.as_deref() else {
            return self.restore_hold();
        };
        let Ok(fresh) = AuthDocument::read(source) else {
            return self.restore_hold();
        };
        let slot = if let Some(slot) = state.slot {
            let stored = AuthDocument::read(self.config.profile_auth(slot))?;
            if fresh.identity != stored.identity || fresh.same_credentials(&stored) {
                return self.restore_hold();
            }
            slot
        } else if let Some(existing) = self.slot_for_identity(&fresh.identity)? {
            existing
        } else {
            self.next_slot()?
        };
        let target = self.config.profile_auth(slot);
        let pending = target.with_extension("json.cxa-pending");
        atomic_copy(source, &pending, 0o600)?;
        let commit = TransactionState {
            mode: TransactionMode::Commit,
            slot: Some(slot),
            activate: state.activate,
            select: state.select,
            link_session: state.link_session,
            profile_pending: Some(pending),
            recovery_source: state.recovery_source.clone(),
        };
        write_state(&self.config.transaction_state(), &commit)?;
        self.finish_commit(&commit)
    }

    fn finish_commit(&self, state: &TransactionState) -> Result<()> {
        let slot = state
            .slot
            .ok_or_else(|| Error::Message("transaction has no account slot".into()))?;
        let profile_path = self.config.profile_auth(slot);
        if let Some(pending) = &state.profile_pending {
            if pending.exists() {
                fs::rename(pending, &profile_path)
                    .map_err(|error| Error::io(&profile_path, error))?;
            }
        }
        if state.activate {
            atomic_copy(&profile_path, &self.config.active_auth, 0o600)?;
        } else {
            self.restore_hold()?;
        }
        if state.select {
            atomic_write(
                &self.config.active_profile,
                format!("{slot}\n").as_bytes(),
                0o600,
            )?;
        }
        if state.link_session {
            self.ensure_session_link()?;
        }
        Ok(())
    }

    fn cleanup_transaction(&self, state: &TransactionState) -> Result<()> {
        if let Some(pending) = &state.profile_pending {
            remove_file_if_exists(pending)?;
        }
        remove_file_if_exists(&self.config.active_pending())?;
        remove_file_if_exists(&self.config.active_hold())?;
        if let Some(source) = &state.recovery_source {
            self.cleanup_recovery_source(source)?;
        }
        remove_file_if_exists(&self.config.transaction_state())
    }

    fn cleanup_recovery_source(&self, source: &Path) -> Result<()> {
        let Some(directory) = source.parent() else {
            return Ok(());
        };
        let generated = directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".quota-") || name.starts_with(".enroll-"));
        if directory.parent() != Some(self.config.account_store.as_path()) || !generated {
            return Ok(());
        }
        match fs::remove_dir_all(directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io(directory, error)),
        }
    }

    pub fn status_lines(&self) -> Result<Vec<String>> {
        let selected = self
            .selected()
            .ok_or_else(|| Error::Message("No default Codex account is selected.".into()))?;
        let profile = AuthDocument::read(self.config.profile_auth(selected))?;
        let mut lines = vec![format!(
            "Default Codex account: {selected}  {}",
            profile.identity.email
        )];
        lines.push(format!(
            "Quota: {}",
            self.usage(selected)
                .map(|usage| usage.label(now_epoch()))
                .unwrap_or_else(|| "usage unknown".into())
        ));
        if let Ok(active) = AuthDocument::read(&self.config.active_auth) {
            if active.identity != profile.identity {
                if let Some(slot) = self.slot_for_identity(&active.identity)? {
                    lines.push(format!(
                        "Active credentials: account {slot} ({}; does not match the selected account)",
                        active.identity.email
                    ));
                }
            }
        }
        lines.push(format!(
            "Shared session home: {}",
            self.config.codex_home.display()
        ));
        if fs::read_link(&self.config.session_auth).ok().as_deref()
            == Some(&self.config.active_auth)
            && self.config.active_auth.is_file()
        {
            lines.push(format!(
                "Session credentials: linked to {}",
                self.config.active_auth.display()
            ));
        } else if self.config.session_auth.exists() {
            lines.push(format!(
                "Session credentials: DETACHED ({} is a real file). Run cxa relink.",
                self.config.session_auth.display()
            ));
        } else {
            lines.push(format!(
                "Session credentials: MISSING ({}). Run cxa relink.",
                self.config.session_auth.display()
            ));
        }
        Ok(lines)
    }

    pub fn status_issue(&self) -> Result<Option<String>> {
        let selected = self
            .selected()
            .ok_or_else(|| Error::Message("No default Codex account is selected.".into()))?;
        let profile = AuthDocument::read(self.config.profile_auth(selected))?;
        if fs::read_link(&self.config.session_auth).ok().as_deref()
            != Some(&self.config.active_auth)
            || !self.config.active_auth.is_file()
        {
            return Ok(Some(
                "Shared session credentials are not linked; run cxa relink.".into(),
            ));
        }
        let active = AuthDocument::read(&self.config.active_auth)?;
        if active.identity != profile.identity {
            return Ok(Some(
                "Active credentials do not match the selected account.".into(),
            ));
        }
        Ok(None)
    }
}

pub struct AuthBarrier {
    config: Config,
    finished: bool,
    irreversible: bool,
}

impl AuthBarrier {
    pub fn mark_refreshing(
        &mut self,
        slot: Option<u32>,
        source: PathBuf,
        activate: bool,
        select: bool,
        link_session: bool,
    ) -> Result<()> {
        let state = TransactionState {
            mode: TransactionMode::Refreshing,
            slot,
            activate,
            select,
            link_session,
            profile_pending: None,
            recovery_source: Some(source),
        };
        write_state(&self.config.transaction_state(), &state)?;
        self.irreversible = true;
        Ok(())
    }

    pub fn rollback(mut self) -> Result<()> {
        let store = Store::new(self.config.clone());
        store.restore_hold()?;
        remove_file_if_exists(&self.config.transaction_state())?;
        self.finished = true;
        Ok(())
    }

    pub fn commit_switch(mut self, slot: u32, link_session: bool) -> Result<()> {
        if writers_running(&self.config) {
            return self.rollback().and(Err(Error::WriterRunning));
        }
        let state = TransactionState {
            mode: TransactionMode::Commit,
            slot: Some(slot),
            activate: true,
            select: true,
            link_session,
            profile_pending: None,
            recovery_source: None,
        };
        write_state(&self.config.transaction_state(), &state)?;
        self.irreversible = true;
        let store = Store::new(self.config.clone());
        store.finish_commit(&state)?;
        store.cleanup_transaction(&state)?;
        self.finished = true;
        Ok(())
    }

    pub fn commit_profile(
        mut self,
        slot: u32,
        source: &Path,
        activate: bool,
        select: bool,
        link_session: bool,
    ) -> Result<()> {
        let target = self.config.profile_auth(slot);
        let pending = target.with_extension("json.cxa-pending");
        atomic_copy(source, &pending, 0o600)?;
        let state = TransactionState {
            mode: TransactionMode::Commit,
            slot: Some(slot),
            activate,
            select,
            link_session,
            profile_pending: Some(pending),
            recovery_source: Some(source.to_owned()),
        };
        write_state(&self.config.transaction_state(), &state)?;
        self.irreversible = true;
        if writers_running(&self.config) {
            return Err(Error::RecoveryDeferred);
        }
        let store = Store::new(self.config.clone());
        store.finish_commit(&state)?;
        store.cleanup_transaction(&state)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for AuthBarrier {
    fn drop(&mut self) {
        if self.finished || self.irreversible {
            return;
        }
        let store = Store::new(self.config.clone());
        let _ = store.restore_hold();
        let _ = remove_file_if_exists(&self.config.transaction_state());
    }
}

fn read_state(path: &Path) -> Result<TransactionState> {
    let bytes = fs::read(path).map_err(|error| Error::io(path, error))?;
    serde_json::from_slice(&bytes).map_err(|error| Error::json(path, error))
}

fn write_state(path: &Path, state: &TransactionState) -> Result<()> {
    let bytes = serde_json::to_vec(state)
        .map_err(|error| Error::Message(format!("could not encode transaction: {error}")))?;
    atomic_write(path, &bytes, 0o600)
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
