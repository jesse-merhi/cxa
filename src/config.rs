use std::env;
use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub codex_home: PathBuf,
    pub codex_binary: Option<PathBuf>,
    pub account_store: PathBuf,
    pub switch_lock: PathBuf,
    pub session_auth: PathBuf,
    pub usage_ttl_seconds: u64,
    pub skip_usage_refresh: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let codex_home = env_path("CODEX_HOME")?;
        let account_store = env_path("CXA_ACCOUNT_STORE")?;
        let home = if codex_home.is_none() || account_store.is_none() {
            Some(
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .ok_or_else(|| Error::Message("HOME is not set".into()))?,
            )
        } else {
            None
        };
        let codex_home = codex_home
            .or_else(|| home.as_ref().map(|home| home.join(".codex")))
            .expect("HOME is available when CODEX_HOME is omitted");
        let account_store = account_store
            .or_else(|| home.as_ref().map(|home| home.join(".codex-auth")))
            .expect("HOME is available when CXA_ACCOUNT_STORE is omitted");
        let codex_binary = env_path("CXA_CODEX_BIN")?;
        let session_auth = codex_home.join("auth.json");

        if paths_overlap(&codex_home, &account_store) {
            return Err(Error::Message(format!(
                "CODEX_HOME and CXA_ACCOUNT_STORE must be separate directories: {} and {}",
                codex_home.display(),
                account_store.display()
            )));
        }

        let usage_ttl_seconds = env::var("CXA_USAGE_TTL")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(120);
        let skip_usage_refresh = env::var("CXA_SKIP_USAGE_REFRESH").as_deref() == Ok("1");

        Ok(Self {
            switch_lock: account_store.join("switch.lock"),
            session_auth,
            codex_home,
            codex_binary,
            account_store,
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

    pub fn codex_binary(&self) -> &Path {
        self.codex_binary.as_deref().unwrap_or(Path::new("codex"))
    }

    pub fn require_no_credential_override(&self) -> Result<()> {
        if env::var_os("CODEX_ACCESS_TOKEN").is_some_and(|value| !value.is_empty()) {
            return Err(Error::Message(
                "cxa requires file-backed ChatGPT OAuth credentials. Unset CODEX_ACCESS_TOKEN, then run cxa again."
                    .into(),
            ));
        }
        Ok(())
    }
}

fn env_path(name: &str) -> Result<Option<PathBuf>> {
    let path = env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    if path.as_ref().is_some_and(|path| !path.is_absolute()) {
        return Err(Error::Message(format!("{name} must be an absolute path.")));
    }
    Ok(path)
}

fn paths_overlap(first: &Path, second: &Path) -> bool {
    let first = resolved(first);
    let second = resolved(second);
    path_eq(&first, &second) || is_inside(&first, &second) || is_inside(&second, &first)
}

fn is_inside(path: &Path, directory: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .any(|parent| path_eq(parent, directory))
}

fn path_eq(first: &Path, second: &Path) -> bool {
    if first == second {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        first
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&second.as_os_str().to_string_lossy())
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn resolved(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    let mut missing = Vec::new();
    let mut existing = path;
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        missing.push(name.to_owned());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent;
    }
    if let Ok(mut resolved) = existing.canonicalize() {
        for name in missing.iter().rev() {
            resolved.push(name);
        }
        return resolved;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_stores_are_rejected() {
        assert!(paths_overlap(
            Path::new("/tmp/codex"),
            Path::new("/tmp/codex/accounts")
        ));
    }

    #[test]
    fn separate_sibling_stores_are_allowed() {
        assert!(!paths_overlap(
            Path::new("/tmp/codex"),
            Path::new("/tmp/codex-auth")
        ));
    }
}
