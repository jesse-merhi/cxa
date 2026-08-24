use std::env;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub codex_home: PathBuf,
    pub active_auth: PathBuf,
    pub account_store: PathBuf,
    pub active_profile: PathBuf,
    pub switch_lock: PathBuf,
    pub server_start_marker: PathBuf,
    pub app_server_socket: PathBuf,
    pub session_auth: PathBuf,
    pub usage_ttl_seconds: u64,
    pub skip_usage_refresh: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::Message("HOME is not set".into()))?;
        let codex_home = env_path("CXA_CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
        let account_store =
            env_path("CXA_ACCOUNT_STORE").unwrap_or_else(|| home.join(".codex-auth"));
        let active_auth =
            env_path("CXA_ACTIVE_AUTH").unwrap_or_else(|| default_active_auth(&account_store));
        let app_server_socket = env_path("CXA_SHARED_APP_SERVER_SOCKET")
            .unwrap_or_else(|| default_app_server_socket(&account_store));
        let usage_ttl_seconds = env::var("CXA_USAGE_TTL")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(120);
        let skip_usage_refresh = env::var("CXA_SKIP_USAGE_REFRESH").as_deref() == Ok("1");
        Ok(Self {
            active_profile: account_store.join("active-profile"),
            switch_lock: account_store.join("switch.lock"),
            server_start_marker: account_store.join(".shared-app-server-starting"),
            session_auth: codex_home.join("auth.json"),
            codex_home,
            active_auth,
            account_store,
            app_server_socket,
            usage_ttl_seconds,
            skip_usage_refresh,
        })
    }

    pub fn profile_dir(&self, slot: u32) -> PathBuf {
        self.account_store.join(format!("profile-{slot}"))
    }

    pub fn profile_auth(&self, slot: u32) -> PathBuf {
        self.profile_dir(slot).join("auth.json")
    }

    pub fn profile_usage(&self, slot: u32) -> PathBuf {
        self.profile_dir(slot).join("usage.json")
    }

    pub fn usage_attempt(&self, slot: u32) -> PathBuf {
        self.profile_dir(slot).join(".usage-attempt")
    }

    pub fn transaction_state(&self) -> PathBuf {
        self.account_store.join(".auth-transaction.json")
    }

    pub fn active_hold(&self) -> PathBuf {
        self.active_auth.with_extension("json.cxa-hold")
    }

    pub fn active_pending(&self) -> PathBuf {
        self.active_auth.with_extension("json.cxa-pending")
    }
}

#[cfg(target_os = "linux")]
fn default_active_auth(_: &Path) -> PathBuf {
    PathBuf::from("/var/lib/codex-auth/auth.json")
}

#[cfg(not(target_os = "linux"))]
fn default_active_auth(account_store: &Path) -> PathBuf {
    account_store.join("auth.json")
}

#[cfg(target_os = "linux")]
fn default_app_server_socket(_: &Path) -> PathBuf {
    PathBuf::from("/var/lib/codex-auth/app-server.sock")
}

#[cfg(not(target_os = "linux"))]
fn default_app_server_socket(account_store: &Path) -> PathBuf {
    account_store.join("app-server.sock")
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn non_linux_shared_files_stay_in_the_account_store() {
        let store = Path::new("/users/example/.codex-auth");
        assert_eq!(default_active_auth(store), store.join("auth.json"));
        assert_eq!(
            default_app_server_socket(store),
            store.join("app-server.sock")
        );
    }
}
