use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{AuthDocument, Identity};
use crate::config::{Config, same_path_entry};
use crate::fs::{
    ExclusiveLock, OWNED_TEMP_MARKER, STAGING_WRITER_LOCK, atomic_copy, atomic_copy_if_absent,
    atomic_write, hard_link_entry_if_absent, private_dir, remove_file_if_exists, sync_parent,
};
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
        if self.exhausted_now(now) {
            bits.push("EXHAUSTED".into());
        }
        if let Some(plan) = &self.plan_type {
            bits.push(plan.clone());
        }
        if self.credits_depleted() {
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
        if self.spend_control_reached {
            return self
                .individual_window
                .as_ref()
                .and_then(|window| window.resets_at)
                .is_none_or(|reset| reset > now);
        }
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

    pub fn max_current_used_percent(&self, now: i64) -> Option<f64> {
        [
            self.primary_window.as_ref(),
            self.secondary_window.as_ref(),
            self.individual_window.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|window| window.resets_at.is_none_or(|reset| reset > now))
        .filter_map(|window| window.used_percent)
        .reduce(f64::max)
    }

    pub fn credits_depleted(&self) -> bool {
        self.has_credits == Some(false) && self.unlimited == Some(false)
    }
}

fn format_percent(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
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
    #[serde(default = "default_hold_active")]
    hold_active: bool,
    profile_pending: Option<PathBuf>,
    #[serde(default)]
    recovery_source: Option<PathBuf>,
}

fn default_hold_active() -> bool {
    true
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

    pub fn adopt_detached_session_selection(&self) -> Result<()> {
        if self.selected().is_some() {
            return Ok(());
        }
        let Ok(metadata) = fs::symlink_metadata(&self.config.session_auth) else {
            return Ok(());
        };
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        let Ok(session) = AuthDocument::read(&self.config.session_auth) else {
            return Ok(());
        };
        let Some(slot) = self.slot_for_identity(&session.identity)? else {
            return Ok(());
        };
        atomic_write(
            &self.config.active_profile,
            format!("{slot}\n").as_bytes(),
            0o600,
        )
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
                .as_deref()
                .is_some_and(|email| email.to_lowercase().contains(&selector))
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
            profile
                .auth
                .identity
                .same_account(identity)
                .then_some(profile.slot)
        }))
    }

    pub fn next_slot(&self) -> Result<u32> {
        let entries = match fs::read_dir(&self.config.account_store) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(1),
            Err(error) => return Err(Error::io(&self.config.account_store, error)),
        };
        let mut highest = 0;
        for entry in entries {
            let entry = entry.map_err(|error| Error::io(&self.config.account_store, error))?;
            let name = entry.file_name();
            let Some(slot) = name
                .to_string_lossy()
                .strip_prefix("profile-")
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            highest = highest.max(slot);
        }
        highest
            .checked_add(1)
            .ok_or_else(|| Error::Message("No free Codex account slots remain.".into()))
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

    fn reconcile_active_and_profile(&self, active_path: &Path) -> Result<()> {
        if !active_path.is_file() {
            return Ok(());
        }
        let active = AuthDocument::read(active_path)?;
        let Some(slot) = self.slot_for_identity(&active.identity)? else {
            return Err(Error::Message(format!(
                "Active credentials ({}) match no enrolled account; not saving them.",
                active.identity.label()
            )));
        };
        let target_path = self.config.profile_auth(slot);
        let target = AuthDocument::read(&target_path)?;
        if active.refresh_ns > target.refresh_ns {
            atomic_copy(active_path, &target_path, 0o600)?;
        } else if target.refresh_ns > active.refresh_ns {
            atomic_copy(&target_path, active_path, 0o600)?;
        } else if target.refresh_ns == active.refresh_ns && !active.same_credentials(&target) {
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

    pub fn reconcile_if_idle(&self) -> Result<()> {
        if !self.config.active_auth.is_file() {
            return Ok(());
        }
        AuthDocument::read(&self.config.active_auth)?;
        match self.begin_barrier() {
            Ok(barrier) => barrier.rollback(),
            Err(Error::WriterRunning | Error::RecoveryDeferred) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn ensure_session_link(&self, selected_slot: Option<u32>) -> Result<()> {
        private_dir(&self.config.codex_home)?;
        self.preserve_session_hold(selected_slot)?;
        if self.config.session_links_to_active() {
            return self.discard_session_hold();
        }
        match fs::symlink_metadata(&self.config.session_auth) {
            Ok(_) => {
                fs::rename(&self.config.session_auth, self.config.session_hold())
                    .map_err(|error| Error::io(&self.config.session_auth, error))?;
                sync_parent(&self.config.session_auth)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(&self.config.session_auth, error)),
        }
        if let Err(error) = self.preserve_session_hold(selected_slot) {
            if matches!(&error, Error::RecoveryDeferred) {
                return Err(error);
            }
            return self.restore_rejected_session(error);
        }
        let temporary = self.config.session_link_pending();
        remove_file_if_exists(&temporary)?;
        symlink(&self.config.active_auth, &temporary)
            .map_err(|error| Error::io(&temporary, error))?;
        let installed = hard_link_entry_if_absent(&temporary, &self.config.session_auth);
        remove_file_if_exists(&temporary)?;
        if !installed? || writers_running(&self.config) {
            return Err(Error::RecoveryDeferred);
        }
        self.discard_session_hold()
    }

    fn validate_session_link(&self) -> Result<()> {
        let hold = self.config.session_hold();
        if hold.exists() {
            self.validate_detached_credentials(&hold)?;
        }
        if self.config.session_links_to_active() {
            return Ok(());
        }
        match fs::symlink_metadata(&self.config.session_auth) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                self.validate_detached_credentials(&self.config.session_auth)
            }
            Ok(_) => self.validate_detached_credentials(&self.config.session_auth),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io(&self.config.session_auth, error)),
        }
    }

    fn validate_detached_credentials(&self, path: &Path) -> Result<()> {
        if writers_running(&self.config) {
            return Err(Error::RecoveryDeferred);
        }
        let detached = AuthDocument::read(path)?;
        let Some(slot) = self.slot_for_identity(&detached.identity)? else {
            return Err(Error::Message(format!(
                "Detached credentials for {} are not enrolled; run cxa add before relinking.",
                detached.identity.label()
            )));
        };
        let stored_path = self.config.profile_auth(slot);
        let stored = AuthDocument::read(&stored_path)?;
        if detached.refresh_ns == stored.refresh_ns && !detached.same_credentials(&stored) {
            return Err(Error::Message(
                "Detached and stored credentials have the same refresh time but different contents; not relinking."
                    .into(),
            ));
        }
        Ok(())
    }

    fn preserve_session_hold(&self, selected_slot: Option<u32>) -> Result<()> {
        let hold = self.config.session_hold();
        if !hold.exists() {
            return Ok(());
        }
        if writers_running(&self.config) {
            return Err(Error::RecoveryDeferred);
        }
        let detached = AuthDocument::read(&hold)?;
        let Some(slot) = self.slot_for_identity(&detached.identity)? else {
            return Err(Error::Message(format!(
                "Detached credentials for {} are not enrolled; run cxa add before relinking.",
                detached.identity.label()
            )));
        };
        let stored_path = self.config.profile_auth(slot);
        let stored = AuthDocument::read(&stored_path)?;
        let promote_active = detached.refresh_ns > stored.refresh_ns && selected_slot == Some(slot);
        reconcile_credentials(&hold, &detached, &stored_path, &stored).map_err(|error| {
            if matches!(error, Error::Message(_)) {
                Error::Message(
                    "Detached and stored credentials have the same refresh time but different contents; not relinking."
                        .into(),
                )
            } else {
                error
            }
        })?;
        if promote_active {
            if self.config.active_auth.is_file() {
                let active = AuthDocument::read(&self.config.active_auth)?;
                reconcile_credentials(&hold, &detached, &self.config.active_auth, &active)?;
            } else {
                atomic_copy(&hold, &self.config.active_auth, 0o600)?;
            }
        }
        Ok(())
    }

    fn discard_session_hold(&self) -> Result<()> {
        let hold = self.config.session_hold();
        remove_file_if_exists(&hold)?;
        sync_parent(&hold)
    }

    fn restore_rejected_session(&self, error: Error) -> Result<()> {
        self.restore_session_hold()?;
        Err(error)
    }

    fn restore_session_hold(&self) -> Result<()> {
        let hold = self.config.session_hold();
        match fs::symlink_metadata(&hold) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::io(&hold, error)),
        }
        if !hard_link_entry_if_absent(&hold, &self.config.session_auth)? {
            return Err(Error::RecoveryDeferred);
        }
        remove_file_if_exists(&hold)?;
        sync_parent(&hold)
    }

    pub fn recover(&self) -> Result<()> {
        let state_path = self.config.transaction_state();
        if !state_path.exists() {
            return Ok(());
        }
        let state = read_state(&state_path)?;
        if let Some(directory) = state.recovery_source.as_deref().and_then(Path::parent) {
            if ExclusiveLock::is_held(&directory.join(STAGING_WRITER_LOCK))? {
                return Err(Error::RecoveryDeferred);
            }
        }
        if state.hold_active && writers_running(&self.config) {
            return Err(Error::RecoveryDeferred);
        }
        let cleanup_source = match state.mode {
            TransactionMode::Rollback => {
                self.restore_active_if_held(&state)?;
                true
            }
            TransactionMode::Refreshing => self.recover_refresh(&state)?,
            TransactionMode::Commit => {
                self.finish_commit(&state)?;
                true
            }
        };
        if cleanup_source {
            self.cleanup_transaction(&state)
        } else {
            remove_file_if_exists(&self.config.transaction_state())
        }
    }

    pub fn begin_barrier(&self) -> Result<AuthBarrier> {
        if self.config.transaction_state().exists() {
            self.recover()?;
        }
        if writers_running(&self.config) {
            return Err(Error::WriterRunning);
        }
        let selected = self.selected();
        let replace_invalid_active = if self.config.active_auth.is_file() {
            match AuthDocument::read(&self.config.active_auth) {
                Ok(_) => false,
                Err(Error::Json { .. } | Error::InvalidAuth(_)) if selected.is_some() => true,
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        remove_file_if_exists(&self.config.active_hold())?;
        let state = TransactionState {
            mode: TransactionMode::Rollback,
            slot: None,
            activate: false,
            select: false,
            link_session: false,
            hold_active: true,
            profile_pending: None,
            recovery_source: None,
        };
        write_state(&self.config.transaction_state(), &state)?;
        if replace_invalid_active {
            fs::rename(&self.config.active_auth, self.config.active_pending())
                .map_err(|error| Error::io(&self.config.active_auth, error))?;
            atomic_copy(
                &self.config.profile_auth(selected.unwrap()),
                &self.config.active_hold(),
                0o600,
            )?;
        } else if self.config.active_auth.exists() {
            fs::rename(&self.config.active_auth, self.config.active_hold())
                .map_err(|error| Error::io(&self.config.active_auth, error))?;
        } else if let Some(selected) = selected {
            atomic_copy(
                &self.config.profile_auth(selected),
                &self.config.active_hold(),
                0o600,
            )?;
        }
        if writers_running(&self.config) {
            return Err(Error::RecoveryDeferred);
        }
        let barrier = AuthBarrier {
            config: self.config.clone(),
            finished: false,
            irreversible: false,
            hold_active: true,
        };
        self.reconcile_active_and_profile(&self.config.active_hold())?;
        Ok(barrier)
    }

    pub fn begin_enrollment(&self, source: PathBuf) -> Result<AuthBarrier> {
        if self.config.transaction_state().exists() {
            self.recover()?;
        }
        let state = TransactionState {
            mode: TransactionMode::Refreshing,
            slot: None,
            activate: false,
            select: false,
            link_session: false,
            hold_active: false,
            profile_pending: None,
            recovery_source: Some(source),
        };
        write_state(&self.config.transaction_state(), &state)?;
        Ok(AuthBarrier {
            config: self.config.clone(),
            finished: false,
            irreversible: false,
            hold_active: false,
        })
    }

    pub fn begin_profile_commit(&self) -> Result<AuthBarrier> {
        if self.config.transaction_state().exists() {
            self.recover()?;
        }
        Ok(AuthBarrier {
            config: self.config.clone(),
            finished: false,
            irreversible: false,
            hold_active: false,
        })
    }

    fn restore_active_if_held(&self, state: &TransactionState) -> Result<()> {
        if state.hold_active {
            self.restore_hold()?;
        }
        Ok(())
    }

    fn restore_hold(&self) -> Result<()> {
        let hold_path = self.config.active_hold();
        let pending_path = self.config.active_pending();
        if !hold_path.exists() && pending_path.exists() && !self.config.active_auth.exists() {
            let source = self
                .selected()
                .map(|slot| self.config.profile_auth(slot))
                .filter(|path| path.is_file())
                .unwrap_or_else(|| pending_path.clone());
            restore_copy_if_absent(&source, &self.config.active_auth)?;
        }
        if hold_path.exists() {
            self.preserve_reappeared_active(None)?;
            let hold = AuthDocument::read(&hold_path)?;
            let selected_profile = self.selected().map(|slot| self.config.profile_auth(slot));
            if let Some((profile_path, profile)) = selected_profile
                .as_ref()
                .and_then(|path| AuthDocument::read(path).ok().map(|auth| (path, auth)))
                .filter(|(_, profile)| profile.identity.same_account(&hold.identity))
            {
                if profile.refresh_ns > hold.refresh_ns {
                    restore_copy_if_absent(profile_path, &self.config.active_auth)?;
                    remove_file_if_exists(&hold_path)?;
                    remove_file_if_exists(&self.config.active_pending())?;
                    return Ok(());
                }
                reconcile_credentials(&hold_path, &hold, profile_path, &profile)?;
            }
            restore_copy_if_absent(&hold_path, &self.config.active_auth)?;
            remove_file_if_exists(&hold_path)?;
            sync_parent(&hold_path)?;
        }
        if pending_path.exists() && !self.config.active_auth.exists() {
            return Err(Error::RecoveryDeferred);
        }
        remove_file_if_exists(&pending_path)
    }

    fn recover_refresh(&self, state: &TransactionState) -> Result<bool> {
        let Some(source) = state.recovery_source.as_deref() else {
            self.restore_active_if_held(state)?;
            return Ok(true);
        };
        let Ok(fresh) = AuthDocument::read(source) else {
            self.restore_active_if_held(state)?;
            return Ok(true);
        };
        let slot = if let Some(slot) = state.slot {
            let stored = AuthDocument::read(self.config.profile_auth(slot))?;
            if !fresh.identity.same_account(&stored.identity) {
                self.restore_active_if_held(state)?;
                return Ok(!staged_enrollment(source));
            }
            if fresh.same_credentials(&stored) {
                self.restore_active_if_held(state)?;
                return Ok(true);
            }
            slot
        } else if self.slot_for_identity(&fresh.identity)?.is_some() {
            self.restore_active_if_held(state)?;
            return Ok(false);
        } else {
            self.next_slot()?
        };
        let pending = self.config.profile_pending(slot);
        atomic_copy(source, &pending, 0o600)?;
        let commit = TransactionState {
            mode: TransactionMode::Commit,
            slot: Some(slot),
            activate: state.activate,
            select: state.select,
            link_session: state.link_session,
            hold_active: state.hold_active,
            profile_pending: Some(pending),
            recovery_source: state.recovery_source.clone(),
        };
        write_state(&self.config.transaction_state(), &commit)?;
        self.finish_commit(&commit)?;
        Ok(true)
    }

    fn finish_commit(&self, state: &TransactionState) -> Result<()> {
        if state.link_session {
            if let Err(error) = self.validate_session_link() {
                if matches!(error, Error::RecoveryDeferred) {
                    return Err(error);
                }
                self.restore_active_if_held(state)?;
                let mut cleanup = state.clone();
                cleanup.recovery_source = None;
                self.cleanup_transaction(&cleanup)?;
                return Err(error);
            }
        }
        let active_already_restored = self.inactive_commit_active_already_restored(state)?;
        if state.hold_active && !active_already_restored {
            self.preserve_reappeared_active(state.profile_pending.as_deref())?;
        }
        let slot = state
            .slot
            .ok_or_else(|| Error::Message("transaction has no account slot".into()))?;
        let profile_path = self.config.profile_auth(slot);
        if let Some(pending) = &state.profile_pending {
            if pending.exists() {
                private_dir(&self.config.profile_dir(slot))?;
                let backup = self.config.profile_backup(slot);
                if profile_path.exists() && !backup.exists() {
                    atomic_copy(&profile_path, &backup, 0o600)?;
                }
                atomic_copy(pending, &profile_path, 0o600)?;
            }
        }
        if state.activate {
            if state.hold_active {
                if !atomic_copy_if_absent(&profile_path, &self.config.active_auth, 0o600)? {
                    return Err(Error::RecoveryDeferred);
                }
            } else {
                atomic_copy(&profile_path, &self.config.active_auth, 0o600)?;
            }
        } else if state.hold_active && !active_already_restored {
            self.restore_hold()?;
        }
        if state.link_session {
            let selected_slot = if state.select {
                Some(slot)
            } else {
                self.selected()
            };
            if let Err(error) = self.ensure_session_link(selected_slot) {
                if matches!(error, Error::RecoveryDeferred) {
                    return Err(error);
                }
                self.rollback_promoted_active(state)?;
                self.rollback_profile_promotion(state)?;
                self.restore_active_if_held(state)?;
                self.restore_session_hold()?;
                let mut cleanup = state.clone();
                cleanup.recovery_source = None;
                self.cleanup_transaction(&cleanup)?;
                return Err(error);
            }
        }
        if state.select {
            atomic_write(
                &self.config.active_profile,
                format!("{slot}\n").as_bytes(),
                0o600,
            )?;
        }
        Ok(())
    }

    fn rollback_promoted_active(&self, state: &TransactionState) -> Result<()> {
        if !state.activate || !self.config.active_auth.exists() {
            return Ok(());
        }
        let installed = state
            .profile_pending
            .as_deref()
            .filter(|path| path.exists())
            .map(Path::to_owned)
            .or_else(|| state.slot.map(|slot| self.config.profile_auth(slot)))
            .ok_or(Error::RecoveryDeferred)?;
        let active = AuthDocument::read(&self.config.active_auth)?;
        let promoted = AuthDocument::read(installed)?;
        if !active.same_credentials(&promoted) {
            return Err(Error::RecoveryDeferred);
        }
        remove_file_if_exists(&self.config.active_auth)?;
        sync_parent(&self.config.active_auth)
    }

    fn rollback_profile_promotion(&self, state: &TransactionState) -> Result<()> {
        if state.profile_pending.is_none() {
            return Ok(());
        }
        let Some(slot) = state.slot else {
            return Err(Error::RecoveryDeferred);
        };
        let Some(pending) = state
            .profile_pending
            .as_deref()
            .filter(|path| path.exists())
        else {
            return Err(Error::RecoveryDeferred);
        };
        let profile = self.config.profile_auth(slot);
        let backup = self.config.profile_backup(slot);
        if profile.exists() {
            let current = AuthDocument::read(&profile)?;
            let promoted = AuthDocument::read(pending)?;
            if !current.same_credentials(&promoted) {
                if backup.exists() && current.same_credentials(&AuthDocument::read(&backup)?) {
                    return Ok(());
                }
                return Err(Error::RecoveryDeferred);
            }
        }
        if backup.exists() {
            atomic_copy(&backup, &profile, 0o600)
        } else if profile.exists() {
            remove_file_if_exists(&profile)?;
            sync_parent(&profile)
        } else {
            Ok(())
        }
    }

    fn inactive_commit_active_already_restored(&self, state: &TransactionState) -> Result<bool> {
        if state.activate
            || !state.hold_active
            || self.config.active_hold().exists()
            || !self.config.active_auth.is_file()
        {
            return Ok(false);
        }
        let Some(selected) = self.selected() else {
            return Ok(false);
        };
        let active = AuthDocument::read(&self.config.active_auth)?;
        let profile_path = self.config.profile_auth(selected);
        let profile = AuthDocument::read(&profile_path)?;
        if !active.identity.same_account(&profile.identity) {
            return Ok(false);
        }
        reconcile_credentials(&self.config.active_auth, &active, &profile_path, &profile)?;
        Ok(true)
    }

    fn preserve_reappeared_active(&self, pending: Option<&Path>) -> Result<()> {
        let claimed = self.config.active_reappeared();
        if !claimed.exists() {
            match fs::rename(&self.config.active_auth, &claimed) {
                Ok(()) => sync_parent(&claimed)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(Error::io(&self.config.active_auth, error)),
            }
        }
        let active = AuthDocument::read(&claimed)?;
        if let Some(pending) = pending.filter(|path| path.exists()) {
            let staged = AuthDocument::read(pending)?;
            if active.identity.same_account(&staged.identity) {
                reconcile_credentials(&claimed, &active, pending, &staged)?;
                remove_file_if_exists(&claimed)?;
                return if self.config.active_auth.exists() {
                    Err(Error::RecoveryDeferred)
                } else {
                    Ok(())
                };
            }
        }
        let slot = self
            .slot_for_identity(&active.identity)?
            .unwrap_or(self.next_slot()?);
        let target = self.config.profile_auth(slot);
        if target.exists() {
            let stored = AuthDocument::read(&target)?;
            reconcile_credentials(&claimed, &active, &target, &stored)?;
        } else {
            private_dir(&self.config.profile_dir(slot))?;
            atomic_copy(&claimed, &target, 0o600)?;
        }
        remove_file_if_exists(&claimed)?;
        if self.config.active_auth.exists() {
            Err(Error::RecoveryDeferred)
        } else {
            Ok(())
        }
    }

    fn cleanup_transaction(&self, state: &TransactionState) -> Result<()> {
        if let Some(pending) = &state.profile_pending {
            remove_file_if_exists(pending)?;
            if let Some(slot) = state.slot {
                remove_file_if_exists(&self.config.profile_backup(slot))?;
            }
        }
        if state.hold_active {
            remove_file_if_exists(&self.config.active_pending())?;
            remove_file_if_exists(&self.config.active_hold())?;
        }
        if let Some(source) = &state.recovery_source {
            self.cleanup_recovery_source(source)?;
        }
        let transaction = self.config.transaction_state();
        remove_file_if_exists(&transaction)?;
        sync_parent(&transaction)
    }

    fn cleanup_recovery_source(&self, source: &Path) -> Result<()> {
        let Some(directory) = source.parent() else {
            return Ok(());
        };
        let generated = directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".quota-") || name.starts_with(".enroll-"))
            && directory.join(OWNED_TEMP_MARKER).is_file();
        if directory.parent() != Some(self.config.account_store.as_path()) || !generated {
            return Ok(());
        }
        match fs::remove_dir_all(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(directory, error)),
        }
        sync_parent(directory)
    }

    pub fn status_lines(&self) -> Result<Vec<String>> {
        let selected = self
            .selected()
            .ok_or_else(|| Error::Message("No default Codex account is selected.".into()))?;
        let profile = AuthDocument::read(self.config.profile_auth(selected))?;
        let mut lines = vec![format!(
            "Default Codex account: {selected}  {}",
            profile.identity.label()
        )];
        lines.push(format!(
            "Quota: {}",
            self.usage(selected)
                .map(|usage| usage.label(now_epoch()))
                .unwrap_or_else(|| "usage unknown".into())
        ));
        if let Ok(active) = AuthDocument::read(&self.config.active_auth) {
            if !active.identity.same_account(&profile.identity) {
                if let Some(slot) = self.slot_for_identity(&active.identity)? {
                    lines.push(format!(
                        "Active credentials: account {slot} ({}; does not match the selected account)",
                        active.identity.label()
                    ));
                }
            }
        }
        lines.push(format!(
            "Shared session home: {}",
            self.config.codex_home.display()
        ));
        if self.config.session_links_to_active() && self.config.active_auth.is_file() {
            lines.push(format!(
                "Session credentials: linked to {}",
                self.config.active_auth.display()
            ));
        } else if self.detached_session_matches(&profile.identity) && writers_running(&self.config)
        {
            lines.push(
                "Session credentials: live account matches the selection; relink after Codex stops to enable switching."
                    .into(),
            );
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
        if self.detached_session_matches(&profile.identity) && writers_running(&self.config) {
            return Ok(None);
        }
        if !self.config.session_links_to_active() || !self.config.active_auth.is_file() {
            return Ok(Some(
                "Shared session credentials are not linked; run cxa relink.".into(),
            ));
        }
        let active = AuthDocument::read(&self.config.active_auth)?;
        if !active.identity.same_account(&profile.identity) {
            return Ok(Some(
                "Active credentials do not match the selected account.".into(),
            ));
        }
        Ok(None)
    }

    fn detached_session_matches(&self, identity: &Identity) -> bool {
        fs::symlink_metadata(&self.config.session_auth)
            .ok()
            .is_some_and(|metadata| !metadata.file_type().is_symlink())
            && AuthDocument::read(&self.config.session_auth)
                .ok()
                .is_some_and(|session| session.identity.same_account(identity))
    }
}

fn reconcile_credentials(
    source_path: &Path,
    source: &AuthDocument,
    target_path: &Path,
    target: &AuthDocument,
) -> Result<()> {
    if source.refresh_ns > target.refresh_ns {
        atomic_copy(source_path, target_path, 0o600)
    } else if source.refresh_ns == target.refresh_ns && !source.same_credentials(target) {
        Err(Error::Message(format!(
            "Credentials at {} and {} have the same refresh time but different contents; neither was overwritten.",
            source_path.display(),
            target_path.display()
        )))
    } else {
        Ok(())
    }
}

fn restore_copy_if_absent(source: &Path, target: &Path) -> Result<()> {
    if atomic_copy_if_absent(source, target, 0o600)? {
        Ok(())
    } else {
        Err(Error::RecoveryDeferred)
    }
}

fn staged_enrollment(source: &Path) -> bool {
    source
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".enroll-"))
}

pub struct AuthBarrier {
    config: Config,
    finished: bool,
    irreversible: bool,
    hold_active: bool,
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
            hold_active: self.hold_active,
            profile_pending: None,
            recovery_source: Some(source),
        };
        write_state(&self.config.transaction_state(), &state)?;
        self.irreversible = true;
        Ok(())
    }

    pub fn rollback(mut self) -> Result<()> {
        self.irreversible = true;
        let store = Store::new(self.config.clone());
        if self.hold_active {
            store.restore_hold()?;
        }
        remove_file_if_exists(&self.config.transaction_state())?;
        self.finished = true;
        Ok(())
    }

    pub fn commit_switch(mut self, slot: u32, link_session: bool) -> Result<()> {
        if writers_running(&self.config) {
            self.irreversible = true;
            return Err(Error::RecoveryDeferred);
        }
        let state = TransactionState {
            mode: TransactionMode::Commit,
            slot: Some(slot),
            activate: true,
            select: true,
            link_session,
            hold_active: self.hold_active,
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
        expected: &AuthDocument,
        activate: bool,
        select: bool,
        link_session: bool,
    ) -> Result<()> {
        let pending = self.config.profile_pending(slot);
        let backup = self.config.profile_backup(slot);
        let staging_overlaps_source = same_path_entry(source, &pending)
            || same_path_entry(source, &backup)
            || matches!(
                (fs::metadata(source), fs::metadata(&pending)),
                (Ok(source), Ok(pending))
                    if source.dev() == pending.dev() && source.ino() == pending.ino()
            );
        if staging_overlaps_source {
            return Err(Error::Message(format!(
                "Import source {} overlaps cxa transaction staging; choose another path.",
                source.display()
            )));
        }
        remove_file_if_exists(&backup)?;
        atomic_copy(source, &pending, 0o600)?;
        let staged_matches = AuthDocument::read(&pending)
            .map(|staged| staged.raw == expected.raw)
            .unwrap_or(false);
        if !staged_matches {
            remove_file_if_exists(&pending)?;
            return Err(Error::Message(format!(
                "Credentials at {} changed or became invalid while being staged; nothing was committed.",
                source.display()
            )));
        }
        let state = TransactionState {
            mode: TransactionMode::Commit,
            slot: Some(slot),
            activate,
            select,
            link_session,
            hold_active: self.hold_active,
            profile_pending: Some(pending),
            recovery_source: Some(source.to_owned()),
        };
        write_state(&self.config.transaction_state(), &state)?;
        self.irreversible = true;
        if self.hold_active && writers_running(&self.config) {
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
        let restored = !self.hold_active || store.restore_hold().is_ok();
        if restored {
            let _ = remove_file_if_exists(&self.config.transaction_state());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;
    use std::io::Write as _;

    fn auth_bytes(token: &str, account: &str, user: &str, refresh_second: u32) -> Vec<u8> {
        let claims = serde_json::json!({
            "email": format!("{user}@example.com"),
            "https://api.openai.com/auth": {"chatgpt_user_id": user}
        });
        let id_token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        serde_json::to_vec(&serde_json::json!({
            "tokens": {
                "id_token": id_token,
                "access_token": token,
                "refresh_token": format!("refresh-{token}"),
                "account_id": account
            },
            "last_refresh": format!("2026-01-01T00:00:{refresh_second:02}Z")
        }))
        .unwrap()
    }

    #[test]
    fn quota_label_reports_windows_credits_and_age() {
        let now = 10_000;
        let usage = UsageRecord {
            observed_at: now - 7_200,
            primary_window: Some(UsageWindow {
                used_percent: Some(100.0),
                resets_at: Some(now - 1),
                window_minutes: Some(300),
            }),
            reached: true,
            has_credits: Some(false),
            unlimited: Some(false),
            balance: Some(Value::String("0".into())),
            ..UsageRecord::default()
        };

        let label = usage.label(now);
        assert!(label.contains("primary 100% used"));
        assert!(label.contains("(passed)"));
        assert!(!label.contains("EXHAUSTED"));
        assert!(label.contains("no credits"));
        assert!(label.contains("seen 2h ago"));
    }

    #[test]
    fn expired_percent_limit_is_not_currently_exhausted() {
        let now = 10_000;
        let usage = UsageRecord {
            primary_window: Some(UsageWindow {
                used_percent: Some(100.0),
                resets_at: Some(now - 1),
                window_minutes: None,
            }),
            reached: true,
            ..UsageRecord::default()
        };

        assert!(!usage.exhausted_now(now));
    }

    #[test]
    fn spend_control_remains_exhausted_after_a_percent_window_resets() {
        let now = 10_000;
        let usage = UsageRecord {
            primary_window: Some(UsageWindow {
                used_percent: Some(100.0),
                resets_at: Some(now - 1),
                window_minutes: None,
            }),
            individual_window: Some(UsageWindow {
                used_percent: Some(100.0),
                resets_at: Some(now + 1),
                window_minutes: None,
            }),
            reached: true,
            spend_control_reached: true,
            ..UsageRecord::default()
        };

        assert!(usage.exhausted_now(now));
    }

    #[test]
    fn spend_control_expires_with_its_individual_limit() {
        let now = 10_000;
        let usage = UsageRecord {
            individual_window: Some(UsageWindow {
                used_percent: Some(75.0),
                resets_at: Some(now - 1),
                window_minutes: None,
            }),
            reached: true,
            spend_control_reached: true,
            ..UsageRecord::default()
        };

        assert!(!usage.exhausted_now(now));
    }

    #[test]
    fn credit_depletion_is_currently_exhausted() {
        let usage = UsageRecord {
            reached: true,
            reached_type: Some("workspace_member_credits_depleted".into()),
            ..UsageRecord::default()
        };

        assert!(usage.exhausted_now(10_000));
    }

    #[test]
    fn preserving_a_claimed_credential_never_removes_a_later_writer() {
        let root = tempfile::tempdir().unwrap();
        let account_store = root.path().join("store");
        let active_auth = root.path().join("shared/auth.json");
        let codex_home = root.path().join("codex");
        fs::create_dir_all(account_store.join("profile-1")).unwrap();
        fs::create_dir_all(active_auth.parent().unwrap()).unwrap();
        fs::create_dir_all(&codex_home).unwrap();
        let old = auth_bytes("old", "account-one", "user-one", 1);
        let new = auth_bytes("new", "account-one", "user-one", 2);
        fs::write(account_store.join("profile-1/auth.json"), &old).unwrap();
        mkfifo(&active_auth, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let config = Config {
            codex_home: codex_home.clone(),
            codex_binary: None,
            active_auth: active_auth.clone(),
            account_store: account_store.clone(),
            active_profile: account_store.join("active-profile"),
            switch_lock: account_store.join("switch.lock"),
            server_start_marker: account_store.join("starting"),
            app_server_socket: account_store.join("app-server.sock"),
            session_auth: codex_home.join("auth.json"),
            usage_ttl_seconds: 120,
            skip_usage_refresh: true,
        };
        let claimed = config.active_reappeared();
        let writer_active = active_auth.clone();
        let writer = std::thread::spawn(move || {
            while !claimed.exists() {
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut pipe = fs::OpenOptions::new().write(true).open(&claimed).unwrap();
            pipe.write_all(&old).unwrap();
            pipe.flush().unwrap();
            fs::write(&writer_active, new).unwrap();
        });

        let result = Store::new(config).preserve_reappeared_active(None);
        writer.join().unwrap();

        assert!(matches!(result, Err(Error::RecoveryDeferred)));
        let live = AuthDocument::read(&active_auth).unwrap();
        assert_eq!(live.raw["tokens"]["access_token"], "new");
    }

    #[test]
    fn rollback_restore_never_replaces_a_late_writer() {
        let root = tempfile::tempdir().unwrap();
        let active_auth = root.path().join("auth.json");
        let hold = root.path().join("auth.json.cxa-hold");
        let held = auth_bytes("held", "account-one", "user-one", 1);
        let newer = auth_bytes("newer", "account-one", "user-one", 2);
        fs::write(&hold, held).unwrap();
        fs::write(&active_auth, newer).unwrap();

        let result = restore_copy_if_absent(&hold, &active_auth);

        assert!(matches!(result, Err(Error::RecoveryDeferred)));
        let live = AuthDocument::read(&active_auth).unwrap();
        assert_eq!(live.raw["tokens"]["access_token"], "newer");
        assert!(hold.exists());
    }
}
