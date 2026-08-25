use std::path::{Path, PathBuf};
use std::{env, fs};

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
        let session_auth = codex_home.join("auth.json");
        let active_auth = env_path("CXA_ACTIVE_AUTH")
            .unwrap_or_else(|| default_active_auth(&account_store, &session_auth));
        let app_server_socket = env_path("CXA_SHARED_APP_SERVER_SOCKET")
            .unwrap_or_else(|| default_app_server_socket(&account_store, &active_auth));
        let usage_ttl_seconds = env::var("CXA_USAGE_TTL")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(120);
        let skip_usage_refresh = env::var("CXA_SKIP_USAGE_REFRESH").as_deref() == Ok("1");
        Ok(Self {
            active_profile: account_store.join("active-profile"),
            switch_lock: account_store.join("switch.lock"),
            server_start_marker: account_store.join(".shared-app-server-starting"),
            session_auth,
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

fn default_active_auth(account_store: &Path, session_auth: &Path) -> PathBuf {
    fs::read_link(session_auth)
        .ok()
        .filter(|target| target.is_absolute())
        .unwrap_or_else(|| account_store.join("auth.json"))
}

fn default_app_server_socket(account_store: &Path, active_auth: &Path) -> PathBuf {
    active_auth
        .parent()
        .unwrap_or(account_store)
        .join("app-server.sock")
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn shared_files_stay_in_the_account_store_by_default() {
        let store = Path::new("/users/example/.codex-auth");
        let session = Path::new("/users/example/.codex/auth.json");
        let active = default_active_auth(store, session);
        assert_eq!(active, store.join("auth.json"));
        assert_eq!(
            default_app_server_socket(store, &active),
            store.join("app-server.sock")
        );
    }

    #[test]
    fn shared_files_follow_an_existing_absolute_session_link() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join(".codex");
        let store = root.path().join(".codex-auth");
        let service_auth = root.path().join("service/auth.json");
        fs::create_dir_all(&codex_home).unwrap();
        symlink(&service_auth, codex_home.join("auth.json")).unwrap();

        let active = default_active_auth(&store, &codex_home.join("auth.json"));
        assert_eq!(active, service_auth);
        assert_eq!(
            default_app_server_socket(&store, &active),
            root.path().join("service/app-server.sock")
        );
    }
}
