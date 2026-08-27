use std::path::{Component, Path, PathBuf};
use std::{env, fs};

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt;

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub codex_home: PathBuf,
    pub codex_binary: Option<PathBuf>,
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
        let default_codex_home = home.join(".codex");
        let exported_codex_home = env_path("CODEX_HOME")?;
        let persisted_codex_home = service_path("CODEX_HOME", &home)?;
        if let (Some(exported), Some(persisted)) = (&exported_codex_home, &persisted_codex_home) {
            if !same_directory(exported, persisted) {
                return Err(Error::Message(format!(
                    "CODEX_HOME must match the persisted systemd service home: {}",
                    persisted.display()
                )));
            }
        }
        if exported_codex_home.is_none()
            && persisted_codex_home
                .as_ref()
                .is_some_and(|persisted| !same_directory(persisted, &default_codex_home))
        {
            return Err(Error::Message(format!(
                "The systemd service uses a custom CODEX_HOME ({}); export that CODEX_HOME before running cxa.",
                persisted_codex_home.unwrap().display()
            )));
        }
        let native_codex_home = exported_codex_home
            .clone()
            .or(persisted_codex_home)
            .unwrap_or(default_codex_home);
        let cxa_codex_home = env_path("CXA_CODEX_HOME")?;
        let codex_home = match cxa_codex_home {
            Some(_) if exported_codex_home.is_none() => {
                return Err(Error::Message(
                    "CXA_CODEX_HOME requires CODEX_HOME to be set to the same path.".into(),
                ));
            }
            Some(cxa) if !same_directory(&native_codex_home, &cxa) => {
                return Err(Error::Message(
                    "CXA_CODEX_HOME must match CODEX_HOME so cxa and Codex use the same session."
                        .into(),
                ));
            }
            _ => native_codex_home,
        };
        let account_store = env_or_service_path("CXA_ACCOUNT_STORE", &home)?
            .unwrap_or_else(|| home.join(".codex-auth"));
        let codex_binary = env_or_service_path("CXA_CODEX_BIN", &home)?;
        let session_auth = codex_home.join("auth.json");
        if is_profile_path(&account_store, &session_auth) {
            return Err(Error::Message(format!(
                "CODEX_HOME must not place session credentials inside an enrolled profile: {}",
                session_auth.display()
            )));
        }
        let active_profile = account_store.join("active-profile");
        let switch_lock = account_store.join("switch.lock");
        let server_start_marker = account_store.join(".shared-app-server-starting");
        let transaction_state = account_store.join(".auth-transaction.json");
        let configured_active_auth = env_or_service_path("CXA_ACTIVE_AUTH", &home)?
            .unwrap_or_else(|| default_active_auth(&account_store, &session_auth));
        let active_auth = resolve_active_auth(&configured_active_auth)?;
        if fs::symlink_metadata(&active_auth)
            .ok()
            .is_some_and(|metadata| !metadata.file_type().is_file())
        {
            return Err(Error::Message(format!(
                "CXA_ACTIVE_AUTH must name a regular credential file when it exists: {}",
                active_auth.display()
            )));
        }
        if is_profile_path(&account_store, &active_auth) {
            return Err(Error::Message(format!(
                "CXA_ACTIVE_AUTH must not point at an enrolled profile: {}",
                active_auth.display()
            )));
        }
        if is_profile_staging_path(&account_store, &active_auth) {
            return Err(Error::Message(format!(
                "CXA_ACTIVE_AUTH must not overlap cxa profile staging: {}",
                active_auth.display()
            )));
        }
        let app_server_socket = env_or_service_path("CXA_SHARED_APP_SERVER_SOCKET", &home)?
            .unwrap_or_else(|| default_app_server_socket(&account_store, &active_auth));
        if is_profile_path(&account_store, &app_server_socket) {
            return Err(Error::Message(format!(
                "CXA_SHARED_APP_SERVER_SOCKET must not point inside an enrolled profile: {}",
                app_server_socket.display()
            )));
        }
        if is_profile_staging_path(&account_store, &app_server_socket) {
            return Err(Error::Message(format!(
                "CXA_SHARED_APP_SERVER_SOCKET must not overlap cxa profile staging: {}",
                app_server_socket.display()
            )));
        }
        if active_auth_collides_with_session(&active_auth, &session_auth) {
            return Err(Error::Message(format!(
                "CXA_ACTIVE_AUTH and the Codex session path must be different: {}",
                session_auth.display()
            )));
        }
        let active_hold = append_path_suffix(&active_auth, ".cxa-hold");
        let active_pending = append_path_suffix(&active_auth, ".cxa-pending");
        let active_reappeared = append_path_suffix(&active_auth, ".cxa-reappeared");
        let session_hold = append_path_suffix(&session_auth, ".cxa-detached");
        let session_link_pending = append_path_suffix(&session_auth, ".cxa-link");
        ensure_distinct_persistent_paths(&[
            ("account store", &account_store),
            ("active credentials", &active_auth),
            ("selected account", &active_profile),
            ("switch lock", &switch_lock),
            ("server start marker", &server_start_marker),
            ("app-server socket", &app_server_socket),
            ("transaction journal", &transaction_state),
            ("active credential hold", &active_hold),
            ("active credential pending", &active_pending),
            ("late active credential", &active_reappeared),
            ("detached session credential", &session_hold),
            ("pending session link", &session_link_pending),
        ])?;
        for (name, path) in [
            ("account store", account_store.as_path()),
            ("selected account", active_profile.as_path()),
            ("switch lock", switch_lock.as_path()),
            ("server start marker", server_start_marker.as_path()),
            ("app-server socket", app_server_socket.as_path()),
            ("transaction journal", transaction_state.as_path()),
            ("active credential hold", active_hold.as_path()),
            ("active credential pending", active_pending.as_path()),
            ("late active credential", active_reappeared.as_path()),
            ("detached session credential", session_hold.as_path()),
            ("pending session link", session_link_pending.as_path()),
        ] {
            if same_path_entry(&session_auth, path) {
                return Err(Error::Message(format!(
                    "Codex session credentials and {name} must use different paths: {}",
                    session_auth.display()
                )));
            }
        }
        let usage_ttl_seconds = env::var("CXA_USAGE_TTL")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(120);
        let skip_usage_refresh = env::var("CXA_SKIP_USAGE_REFRESH").as_deref() == Ok("1");
        Ok(Self {
            active_profile,
            switch_lock,
            server_start_marker,
            session_auth,
            codex_home,
            codex_binary,
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

    pub fn codex_binary(&self) -> &Path {
        self.codex_binary.as_deref().unwrap_or(Path::new("codex"))
    }

    pub fn profile_auth(&self, slot: u32) -> PathBuf {
        self.profile_dir(slot).join("auth.json")
    }

    pub fn profile_usage(&self, slot: u32) -> PathBuf {
        self.profile_dir(slot).join("usage.json")
    }

    pub fn profile_pending(&self, slot: u32) -> PathBuf {
        self.account_store
            .join(format!(".profile-{slot}.auth.cxa-pending"))
    }

    pub fn profile_backup(&self, slot: u32) -> PathBuf {
        self.profile_dir(slot).join(".auth.cxa-backup")
    }

    pub fn usage_attempt(&self, slot: u32) -> PathBuf {
        self.profile_dir(slot).join(".usage-attempt")
    }

    pub fn transaction_state(&self) -> PathBuf {
        self.account_store.join(".auth-transaction.json")
    }

    pub fn active_hold(&self) -> PathBuf {
        append_path_suffix(&self.active_auth, ".cxa-hold")
    }

    pub fn active_pending(&self) -> PathBuf {
        append_path_suffix(&self.active_auth, ".cxa-pending")
    }

    pub fn active_reappeared(&self) -> PathBuf {
        append_path_suffix(&self.active_auth, ".cxa-reappeared")
    }

    pub fn session_hold(&self) -> PathBuf {
        append_path_suffix(&self.session_auth, ".cxa-detached")
    }

    pub fn session_link_pending(&self) -> PathBuf {
        append_path_suffix(&self.session_auth, ".cxa-link")
    }

    pub fn session_links_to_active(&self) -> bool {
        self.resolved_session_link_target()
            .is_some_and(|target| exact_path_entry(&target, &self.active_auth))
    }

    pub fn session_links_to_profile(&self) -> bool {
        self.resolved_session_link_target()
            .is_some_and(|target| is_exact_profile_auth(&self.account_store, &target))
    }

    fn resolved_session_link_target(&self) -> Option<PathBuf> {
        let target = fs::read_link(&self.session_auth).ok().map(|target| {
            if target.is_absolute() {
                target
            } else {
                self.session_auth
                    .parent()
                    .unwrap_or(Path::new("/"))
                    .join(target)
            }
        })?;
        resolve_active_auth(&target).ok()
    }
}

pub(crate) fn append_path_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    value.into()
}

fn default_active_auth(account_store: &Path, session_auth: &Path) -> PathBuf {
    fs::read_link(session_auth)
        .ok()
        .filter(|target| target.is_absolute() && !is_profile_auth(account_store, target))
        .unwrap_or_else(|| account_store.join("auth.json"))
}

fn resolve_active_auth(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(path).map_err(|error| Error::io(path, error))?;
            let target = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or(Path::new("/")).join(target)
            };
            target
                .canonicalize()
                .or_else(|error| {
                    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
                        return Err(error);
                    };
                    if error.kind() == std::io::ErrorKind::NotFound {
                        parent.canonicalize().map(|parent| parent.join(name))
                    } else {
                        Err(error)
                    }
                })
                .map_err(|error| Error::io(&target, error))
        }
        Ok(_) => Ok(path.to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_owned()),
        Err(error) => Err(Error::io(path, error)),
    }
}

fn is_profile_auth(account_store: &Path, target: &Path) -> bool {
    target
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("auth.json"))
        && is_profile_path(account_store, target)
}

fn is_exact_profile_auth(account_store: &Path, target: &Path) -> bool {
    target.file_name().is_some_and(|name| name == "auth.json")
        && resolve_path_entry(target)
            .ancestors()
            .any(|ancestor| is_exact_profile_directory(account_store, ancestor))
}

fn is_profile_path(account_store: &Path, target: &Path) -> bool {
    let resolved = resolve_path_entry(target);
    resolved
        .ancestors()
        .any(|ancestor| is_profile_directory(account_store, ancestor))
}

fn is_profile_staging_path(account_store: &Path, target: &Path) -> bool {
    let resolved = resolve_path_entry(target);
    resolved
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.strip_prefix(".profile-")
                .and_then(|name| name.strip_suffix(".auth.cxa-pending"))
                .is_some_and(|slot| slot.parse::<u32>().is_ok())
        })
        && resolved
            .parent()
            .is_some_and(|parent| safety_same_directory(parent, account_store))
}

fn is_profile_directory(account_store: &Path, target: &Path) -> bool {
    target
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.to_ascii_lowercase()
                .strip_prefix("profile-")
                .is_some_and(|slot| slot.parse::<u32>().is_ok())
        })
        && target
            .parent()
            .is_some_and(|parent| safety_same_directory(parent, account_store))
}

fn is_exact_profile_directory(account_store: &Path, target: &Path) -> bool {
    target
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("profile-"))
        .is_some_and(|slot| slot.parse::<u32>().is_ok())
        && target
            .parent()
            .is_some_and(|parent| same_directory(parent, account_store))
}

fn ensure_distinct_persistent_paths(paths: &[(&str, &Path)]) -> Result<()> {
    for (index, (first_name, first)) in paths.iter().enumerate() {
        for (second_name, second) in &paths[index + 1..] {
            if same_path_entry(first, second) {
                return Err(Error::Message(format!(
                    "{first_name} and {second_name} must use different paths: {}",
                    first.display()
                )));
            }
        }
    }
    Ok(())
}

fn same_directory(first: &Path, second: &Path) -> bool {
    resolve_path_aliases(first) == resolve_path_aliases(second)
}

fn safety_same_directory(first: &Path, second: &Path) -> bool {
    let first = resolve_path_aliases(first);
    let second = resolve_path_aliases(second);
    safety_path_eq(&first, &second)
}

fn default_app_server_socket(account_store: &Path, active_auth: &Path) -> PathBuf {
    active_auth
        .parent()
        .unwrap_or(account_store)
        .join("app-server.sock")
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

fn env_or_service_path(name: &str, home: &Path) -> Result<Option<PathBuf>> {
    if env::var_os(name).is_some_and(|value| !value.is_empty()) {
        return env_path(name);
    }
    service_path(name, home)
}

fn service_path(name: &str, home: &Path) -> Result<Option<PathBuf>> {
    let service_file = home.join(".config/cxa/service.env");
    let contents = match fs::read_to_string(&service_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io(&service_file, error)),
    };
    let mut path = None;
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == name {
            path = Some(PathBuf::from(
                serde_json::from_str::<String>(value)
                    .map_err(|error| Error::json(&service_file, error))?,
            ));
            break;
        }
    }
    if path.as_ref().is_some_and(|path| !path.is_absolute()) {
        return Err(Error::Message(format!("{name} must be an absolute path.")));
    }
    Ok(path)
}

fn active_auth_collides_with_session(active: &Path, session: &Path) -> bool {
    if same_path_entry(active, session) {
        return true;
    }
    if fs::symlink_metadata(session)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        if fs::read_link(session)
            .ok()
            .map(|target| {
                if target.is_absolute() {
                    target
                } else {
                    session.parent().unwrap_or(Path::new("/")).join(target)
                }
            })
            .is_some_and(|target| exact_path_entry(&target, active))
        {
            return false;
        }
        if matches!(
            (active.canonicalize(), session.canonicalize()),
            (Ok(active), Ok(session)) if active == session
        ) {
            return false;
        }
    }
    if matches!(
        (active.canonicalize(), session.canonicalize()),
        (Ok(first), Ok(second)) if first == second
    ) {
        return true;
    }
    if matches!(
        (fs::metadata(active), fs::metadata(session)),
        (Ok(first), Ok(second)) if first.dev() == second.dev() && first.ino() == second.ino()
    ) {
        return true;
    }
    false
}

pub(crate) fn same_path_entry(first: &Path, second: &Path) -> bool {
    let first = resolve_path_entry(first);
    let second = resolve_path_entry(second);
    exact_resolved_path_entry(&first, &second) || safety_path_eq(&first, &second)
}

fn exact_path_entry(first: &Path, second: &Path) -> bool {
    exact_resolved_path_entry(&resolve_path_entry(first), &resolve_path_entry(second))
}

fn exact_resolved_path_entry(first: &Path, second: &Path) -> bool {
    first == second
        || matches!(
            (fs::symlink_metadata(first), fs::symlink_metadata(second)),
            (Ok(first), Ok(second)) if first.dev() == second.dev() && first.ino() == second.ino()
        )
}

fn safety_path_eq(first: &Path, second: &Path) -> bool {
    let mut first = first.components();
    let mut second = second.components();
    loop {
        match (first.next(), second.next()) {
            (Some(first), Some(second))
                if first
                    .as_os_str()
                    .as_bytes()
                    .eq_ignore_ascii_case(second.as_os_str().as_bytes()) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn resolve_path_entry(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => normalize(&resolve_path_aliases(parent).join(name)),
        _ => resolve_path_aliases(path),
    }
}

fn resolve_path_aliases(path: &Path) -> PathBuf {
    resolve_path_aliases_with_limit(&normalize(path), 40)
}

fn resolve_path_aliases_with_limit(path: &Path, remaining_symlinks: usize) -> PathBuf {
    if remaining_symlinks == 0 {
        return normalize(path);
    }
    for ancestor in path.ancestors() {
        let suffix = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
        if let Ok(resolved) = ancestor.canonicalize() {
            return normalize(&resolved.join(suffix));
        }
        if fs::symlink_metadata(ancestor)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            if let Ok(target) = fs::read_link(ancestor) {
                let target = if target.is_absolute() {
                    target
                } else {
                    ancestor.parent().unwrap_or(Path::new("/")).join(target)
                };
                return resolve_path_aliases_with_limit(
                    &normalize(&target.join(suffix)),
                    remaining_symlinks - 1,
                );
            }
        }
    }
    normalize(path)
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_store::Store;
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

    #[test]
    fn configured_paths_compare_lexically_before_they_exist() {
        assert!(active_auth_collides_with_session(
            Path::new("/tmp/example/../auth.json"),
            Path::new("/tmp/auth.json")
        ));
        assert!(!active_auth_collides_with_session(
            Path::new("/missing-one/auth.json"),
            Path::new("/missing-two/auth.json")
        ));
    }

    #[test]
    fn configured_paths_reserve_case_only_aliases_before_they_exist() {
        let root = tempfile::tempdir().unwrap();
        assert!(same_path_entry(
            &root.path().join("Store/active-profile"),
            &root.path().join("store/ACTIVE-PROFILE")
        ));
    }

    #[test]
    fn runtime_paths_require_exact_case() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");

        assert!(!exact_path_entry(
            &root.path().join("active-auth.json"),
            &root.path().join("ACTIVE-AUTH.JSON")
        ));
        assert!(!is_exact_profile_auth(
            &store,
            &store.join("Profile-1/auth.json")
        ));
        assert!(!is_exact_profile_auth(
            &store,
            &store.join("profile-1/AUTH.JSON")
        ));
    }

    #[test]
    fn configured_paths_resolve_dangling_directory_aliases() {
        let root = tempfile::tempdir().unwrap();
        let future_store = root.path().join("future-store");
        let alias = root.path().join("store-alias");
        symlink(&future_store, &alias).unwrap();

        assert!(same_path_entry(
            &alias.join("active-profile"),
            &future_store.join("active-profile")
        ));
        assert!(is_profile_staging_path(
            &future_store,
            &alias.join(".profile-2.auth.cxa-pending")
        ));
    }

    #[test]
    fn configured_paths_resolve_final_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session-auth.json");
        let alias = root.path().join("active-auth.json");
        let hard_alias = root.path().join("hard-active-auth.json");
        let normal_active = root.path().join("normal-active-auth.json");
        let normal_session = root.path().join("normal-session-auth.json");
        fs::write(&session, "{}").unwrap();
        fs::write(&normal_active, "{}").unwrap();
        symlink(&session, &alias).unwrap();
        fs::hard_link(&session, &hard_alias).unwrap();
        symlink(&normal_active, &normal_session).unwrap();

        assert!(active_auth_collides_with_session(&alias, &session));
        assert!(active_auth_collides_with_session(&hard_alias, &session));
        assert!(!active_auth_collides_with_session(
            &normal_active,
            &normal_session
        ));
    }

    #[test]
    fn profile_session_links_migrate_to_the_shared_active_path() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join(".codex");
        let store = root.path().join(".codex-auth");
        let profile_auth = store.join("profile-1/auth.json");
        fs::create_dir_all(&codex_home).unwrap();
        symlink(&profile_auth, codex_home.join("auth.json")).unwrap();

        assert_eq!(
            default_active_auth(&store, &codex_home.join("auth.json")),
            store.join("auth.json")
        );
    }

    #[test]
    fn profile_auth_paths_are_detected_through_directory_aliases() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let profile = store.join("profile-1");
        let alias = root.path().join("store-alias");
        fs::create_dir_all(&profile).unwrap();
        symlink(&store, &alias).unwrap();

        assert!(is_profile_auth(&store, &alias.join("profile-1/auth.json")));
    }

    #[test]
    fn pending_profiles_do_not_reserve_account_slots() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let config = Config {
            codex_home: root.path().join("codex"),
            codex_binary: None,
            active_auth: store.join("auth.json"),
            account_store: store.clone(),
            active_profile: store.join("active-profile"),
            switch_lock: store.join("switch.lock"),
            server_start_marker: store.join("starting"),
            app_server_socket: store.join("app-server.sock"),
            session_auth: root.path().join("codex/auth.json"),
            usage_ttl_seconds: 120,
            skip_usage_refresh: false,
        };

        fs::create_dir_all(&store).unwrap();
        fs::write(config.profile_pending(7), b"pending credentials").unwrap();
        fs::create_dir(config.profile_dir(2)).unwrap();

        assert_eq!(Store::new(config).next_slot().unwrap(), 3);
    }
}
