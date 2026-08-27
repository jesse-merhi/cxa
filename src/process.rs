#[cfg(target_os = "macos")]
use std::ffi::OsStr;
use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::os::unix::ffi::OsStrExt;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::os::unix::fs::MetadataExt;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::config::Config;
use crate::fs::atomic_write;

const START_MARKER_TTL: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterStatus {
    Running,
    Stopped,
    Unknown,
}

pub fn codex_writer_status(config: &Config) -> WriterStatus {
    let uid = unsafe { libc::geteuid() };
    #[cfg(target_os = "linux")]
    if std::env::var_os("CXA_TEST_PGREP_BACKEND").is_none() {
        return proc_writer_status(Path::new("/proc"), uid, config.codex_binary.as_deref());
    }
    pgrep_writer_status(config, uid)
}

fn pgrep_writer_status(config: &Config, uid: u32) -> WriterStatus {
    let named = pgrep_status(&uid.to_string(), "codex(-.*)?", true);
    let Some(configured) = config.codex_binary.as_deref() else {
        return named;
    };
    if !configured.is_absolute() {
        return WriterStatus::Unknown;
    }
    #[cfg(target_os = "macos")]
    let configured = macos_configured_writer_status(configured, uid);
    #[cfg(not(target_os = "macos"))]
    let configured = {
        let escaped = regex_escape(&configured.to_string_lossy());
        pgrep_status(
            &uid.to_string(),
            &format!("(^|[[:space:]]){escaped}([[:space:]]|$)"),
            false,
        )
    };
    match (named, configured) {
        (WriterStatus::Running, _) | (_, WriterStatus::Running) => WriterStatus::Running,
        (WriterStatus::Stopped, WriterStatus::Stopped) => WriterStatus::Stopped,
        _ => WriterStatus::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn macos_configured_writer_status(configured: &Path, uid: u32) -> WriterStatus {
    let configured = match fs::metadata(configured) {
        Ok(metadata) => (metadata.dev(), metadata.ino()),
        Err(_) => return WriterStatus::Unknown,
    };
    let output = match Command::new("pgrep")
        .args(["-u", &uid.to_string(), "."])
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) if output.status.code() == Some(1) => return WriterStatus::Stopped,
        Ok(_) | Err(_) => return WriterStatus::Unknown,
    };
    let mut uncertain = false;
    for pid in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|pid| *pid != std::process::id() as i32)
    {
        let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let length = unsafe {
            libc::proc_pidpath(
                pid,
                buffer.as_mut_ptr().cast(),
                buffer.len().try_into().unwrap(),
            )
        };
        if length <= 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                uncertain = true;
            }
            continue;
        }
        let path = Path::new(OsStr::from_bytes(&buffer[..length as usize]));
        match fs::metadata(path) {
            Ok(metadata) if (metadata.dev(), metadata.ino()) == configured => {
                return WriterStatus::Running;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => uncertain = true,
        }
    }
    if uncertain {
        WriterStatus::Unknown
    } else {
        WriterStatus::Stopped
    }
}

#[cfg(any(target_os = "linux", test))]
fn proc_writer_status(proc_root: &Path, uid: u32, configured: Option<&Path>) -> WriterStatus {
    if configured.is_some_and(|path| !path.is_absolute()) {
        return WriterStatus::Unknown;
    }
    let entries = match fs::read_dir(proc_root) {
        Ok(entries) => entries,
        Err(_) => return WriterStatus::Unknown,
    };
    let configured_bytes = configured.map(|path| path.as_os_str().as_bytes());
    let configured_identity = configured.map(|path| {
        fs::metadata(path)
            .map(|metadata| (metadata.dev(), metadata.ino()))
            .map_err(|_| ())
    });
    let mut uncertain = false;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                uncertain = true;
                continue;
            }
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let process = entry.path();
        match fs::metadata(&process) {
            Ok(metadata) if metadata.uid() != uid => continue,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                uncertain = true;
                continue;
            }
        }
        match fs::read_to_string(process.join("comm")) {
            Ok(name) if codex_process_name(name.trim()) => return WriterStatus::Running,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => uncertain = true,
        }
        let Some(configured) = configured_bytes else {
            continue;
        };
        match fs::read(process.join("cmdline")) {
            Ok(arguments)
                if arguments
                    .split(|byte| *byte == 0)
                    .any(|arg| arg == configured) =>
            {
                return WriterStatus::Running;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => uncertain = true,
        }
        match configured_identity.as_ref() {
            Some(Ok(identity)) => match fs::metadata(process.join("exe")) {
                Ok(metadata) if (metadata.dev(), metadata.ino()) == *identity => {
                    return WriterStatus::Running;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => uncertain = true,
            },
            Some(Err(())) => uncertain = true,
            None => {}
        }
    }
    if uncertain {
        WriterStatus::Unknown
    } else {
        WriterStatus::Stopped
    }
}

#[cfg(any(target_os = "linux", test))]
fn codex_process_name(name: &str) -> bool {
    name == "codex" || name.starts_with("codex-")
}

fn pgrep_status(uid: &str, pattern: &str, process_name_only: bool) -> WriterStatus {
    let mut command = Command::new("pgrep");
    command.args(["-u", uid]);
    if process_name_only {
        command.arg("-x");
    } else {
        command.arg("-f");
    }
    match command
        .arg(pattern)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => WriterStatus::Running,
        Ok(status) if status.code() == Some(1) => WriterStatus::Stopped,
        Ok(_) | Err(_) => WriterStatus::Unknown,
    }
}

#[cfg(not(target_os = "macos"))]
fn regex_escape(value: &str) -> String {
    value.chars().fold(String::new(), |mut escaped, character| {
        if "\\.^$|()[]{}*+?".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
        escaped
    })
}

pub fn writers_running(config: &Config) -> bool {
    codex_writer_status(config) != WriterStatus::Stopped || start_marker_active(config)
}

pub fn mark_service_starting(config: &Config) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    atomic_write(
        &config.server_start_marker,
        format!("{now}\n").as_bytes(),
        0o600,
    )
}

fn start_marker_active(config: &Config) -> bool {
    let Ok(value) = fs::read_to_string(&config.server_start_marker) else {
        return false;
    };
    let Some(created) = value.trim().parse::<u64>().ok() else {
        return false;
    };
    let created = UNIX_EPOCH + Duration::from_secs(created);
    SystemTime::now()
        .duration_since(created)
        .is_ok_and(|age| age < START_MARKER_TTL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn proc_scanner_finds_named_and_configured_codex_processes() {
        let root = tempfile::tempdir().unwrap();
        let named = root.path().join("424241");
        fs::create_dir(&named).unwrap();
        fs::write(named.join("comm"), "codex-linux\n").unwrap();
        fs::write(named.join("cmdline"), b"codex-linux\0").unwrap();
        let uid = unsafe { libc::geteuid() };

        assert_eq!(
            proc_writer_status(root.path(), uid, None),
            WriterStatus::Running
        );

        fs::write(named.join("comm"), "bash\n").unwrap();
        fs::write(
            named.join("cmdline"),
            b"/usr/bin/bash\0/opt/custom/codex-wrapper\0",
        )
        .unwrap();
        assert_eq!(
            proc_writer_status(
                root.path(),
                uid,
                Some(Path::new("/opt/custom/codex-wrapper"))
            ),
            WriterStatus::Running
        );
        assert_eq!(
            proc_writer_status(root.path(), uid, Some(Path::new("relative-codex"))),
            WriterStatus::Unknown
        );

        let executable = root.path().join("custom-assistant");
        fs::write(&executable, b"binary").unwrap();
        fs::write(named.join("comm"), "assistant\n").unwrap();
        fs::write(named.join("cmdline"), b"assistant\0app-server\0").unwrap();
        symlink(&executable, named.join("exe")).unwrap();
        assert_eq!(
            proc_writer_status(root.path(), uid, Some(&executable)),
            WriterStatus::Running
        );
    }
}
