use std::fs;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::fcntl::{OFlag, open};
use nix::sys::stat::{Mode, fchmod};
use nix::unistd::{Gid, Group, Uid, User, fchown};
use tungstenite::client;
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;

use crate::fs::DeadlineUnixStream;
use crate::{Error, Result};

pub const SOCKET_DIR: &str = "/var/lib/codex-auth";
pub const SOCKET_NAME: &str = "app-server.sock";
pub const OPENCLAW_GROUP: &str = "openclaw";
pub const PROC_ROOT: &str = "/proc";
const WRITABLE_DIRECTORY_MODE: libc::mode_t = 0o700;
const PUBLISHED_DIRECTORY_MODE: libc::mode_t = 0o2511;
const PUBLISHED_SOCKET_MODE: u32 = 0o600;

#[derive(Clone, Copy, Debug)]
struct Identity {
    uid: Uid,
    gid: Gid,
}

fn identity(user_name: &str) -> Result<Identity> {
    let user = User::from_name(user_name)?
        .ok_or_else(|| Error::Message(format!("missing account: {user_name}")))?;
    let group = Group::from_name(OPENCLAW_GROUP)?
        .ok_or_else(|| Error::Message(format!("missing group: {OPENCLAW_GROUP}")))?;
    Ok(Identity {
        uid: user.uid,
        gid: group.gid,
    })
}

fn open_directory(path: &Path, identity: Identity) -> Result<OwnedFd> {
    let descriptor = open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )?;
    let details = fs::metadata(path).map_err(|error| Error::io(path, error))?;
    if !matches!(details.uid(), 0) && details.uid() != identity.uid.as_raw()
        || details.gid() != identity.gid.as_raw()
    {
        return Err(Error::Message(format!(
            "{} must be owned by the login user:{OPENCLAW_GROUP}",
            path.display()
        )));
    }
    Ok(descriptor)
}

fn take_directory(descriptor: &OwnedFd, identity: Identity) -> Result<()> {
    fchown(descriptor, Some(Uid::from_raw(0)), Some(identity.gid))?;
    fchmod(
        descriptor,
        Mode::from_bits_truncate(PUBLISHED_DIRECTORY_MODE),
    )?;
    Ok(())
}

fn restore_directory(descriptor: &OwnedFd, identity: Identity) -> Result<()> {
    fchown(descriptor, Some(identity.uid), Some(identity.gid))?;
    fchmod(
        descriptor,
        Mode::from_bits_truncate(WRITABLE_DIRECTORY_MODE),
    )?;
    Ok(())
}

fn socket_path(directory: &Path) -> PathBuf {
    directory.join(SOCKET_NAME)
}

fn socket_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => Ok(Some(metadata)),
        Ok(_) => Err(Error::Message(format!(
            "{} exists but is not a Unix socket",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io(path, error)),
    }
}

pub fn prepare(user_name: &str) -> Result<()> {
    prepare_at(Path::new(SOCKET_DIR), user_name)
}

fn prepare_at(directory: &Path, user_name: &str) -> Result<()> {
    let identity = identity(user_name)?;
    let descriptor = open_directory(directory, identity)?;
    let path = socket_path(directory);
    take_directory(&descriptor, identity)?;
    let result = (|| {
        if socket_metadata(&path)?.is_some() {
            match UnixStream::connect(&path) {
                Ok(_) => {
                    return Err(Error::Message(
                        "another app server is already listening".into(),
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(&path).map_err(|error| Error::io(&path, error))?;
                }
                Err(error) => return Err(Error::io(&path, error)),
            }
        }
        Ok(())
    })();
    let restored = restore_directory(&descriptor, identity);
    result.and(restored)
}

pub fn recover_owner(user_name: &str) -> Result<()> {
    let identity = identity(user_name)?;
    let directory = Path::new(SOCKET_DIR);
    let descriptor = open_directory(directory, identity)?;
    restore_directory(&descriptor, identity)
}

pub fn process_cgroups(proc_root: &Path, process_id: u32) -> Result<Vec<String>> {
    let path = proc_root.join(process_id.to_string()).join("cgroup");
    let contents = fs::read_to_string(&path).map_err(|error| Error::io(&path, error))?;
    let mut groups: Vec<String> = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    groups.sort();
    Ok(groups)
}

pub fn listener_belongs_to_service(proc_root: &Path, peer_pid: u32, expected_pid: u32) -> bool {
    if peer_pid == expected_pid {
        return true;
    }
    match (
        process_cgroups(proc_root, peer_pid),
        process_cgroups(proc_root, expected_pid),
    ) {
        (Ok(peer), Ok(expected)) => !peer.is_empty() && peer == expected,
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn peer_credentials(stream: &UnixStream) -> Result<(u32, u32)> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut _ as *mut libc::c_void,
            &mut length,
        )
    };
    if status != 0 {
        return Err(Error::io(
            "app-server socket",
            std::io::Error::last_os_error(),
        ));
    }
    Ok((credentials.pid as u32, credentials.uid))
}

#[cfg(not(target_os = "linux"))]
fn peer_credentials(_stream: &UnixStream) -> Result<(u32, u32)> {
    Err(Error::Message(
        "codex-shared-socket publish is supported only on Linux".into(),
    ))
}

fn verify_websocket(
    stream: UnixStream,
    deadline: Instant,
) -> Result<tungstenite::WebSocket<DeadlineUnixStream>> {
    let request = "ws://localhost/ready"
        .into_client_request()
        .map_err(|error| Error::Protocol(error.to_string()))?;
    let stream = DeadlineUnixStream::new(stream, deadline);
    let (websocket, _) = client(request, stream).map_err(|error| match error {
        HandshakeError::Failure(error) => Error::WebSocket(error),
        HandshakeError::Interrupted(_) => Error::Timeout,
    })?;
    Ok(websocket)
}

fn wait_for_websocket<T>(
    deadline: Instant,
    handshake_timeout: Duration,
    mut candidate: impl FnMut() -> Result<Option<(T, UnixStream)>>,
) -> Result<(T, tungstenite::WebSocket<DeadlineUnixStream>)> {
    loop {
        if Instant::now() >= deadline {
            return Err(Error::Message(
                "fresh app-server listener did not become ready".into(),
            ));
        }
        let Some((state, stream)) = candidate()? else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let attempt_deadline = deadline.min(Instant::now() + handshake_timeout);
        match verify_websocket(stream, attempt_deadline) {
            Ok(websocket) => return Ok((state, websocket)),
            Err(error) if transient_handshake_error(&error) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

fn transient_handshake_error(error: &Error) -> bool {
    matches!(error, Error::Timeout)
        || matches!(
            error,
            Error::WebSocket(tungstenite::Error::Io(source))
            if matches!(
                source.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            )
        )
}

pub fn publish(user_name: &str, expected_pid: u32) -> Result<()> {
    let identity = identity(user_name)?;
    let directory = Path::new(SOCKET_DIR);
    let descriptor = open_directory(directory, identity)?;
    let path = socket_path(directory);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (before, mut websocket) = wait_for_websocket(deadline, Duration::from_secs(1), || {
        let Some(metadata) = socket_metadata(&path)? else {
            return Ok(None);
        };
        let Ok(stream) = UnixStream::connect(&path) else {
            return Ok(None);
        };
        let (peer_pid, peer_uid) = peer_credentials(&stream)?;
        if peer_uid != identity.uid.as_raw()
            || !listener_belongs_to_service(Path::new(PROC_ROOT), peer_pid, expected_pid)
        {
            return Err(Error::Message(
                "socket belongs to an unexpected process".into(),
            ));
        }
        Ok(Some((metadata, stream)))
    })?;
    take_directory(&descriptor, identity)?;
    let result = (|| {
        let after = socket_metadata(&path)?
            .ok_or_else(|| Error::Message("app-server socket vanished while publishing".into()))?;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            return Err(Error::Message(
                "app-server socket changed while publishing permissions".into(),
            ));
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(PUBLISHED_SOCKET_MODE))
            .map_err(|error| Error::io(&path, error))
    })();
    let _ = websocket.close(None);
    if result.is_err() {
        return result.and(restore_directory(&descriptor, identity));
    }
    result
}

pub fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error::Message(
            "codex-shared-socket must run as root".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use tempfile::tempdir;

    #[test]
    fn readiness_handshake_accepts_codex_without_a_subprotocol() {
        let (client, server) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || tungstenite::accept(server).unwrap());

        let websocket = verify_websocket(client, Instant::now() + Duration::from_secs(1));

        assert!(websocket.is_ok(), "{websocket:?}");
        drop(websocket);
        drop(server.join().unwrap());
    }

    #[test]
    fn readiness_handshake_retries_transient_timeouts_until_the_deadline() {
        let mut attempts = 0;
        let result = wait_for_websocket(
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(20),
            || {
                attempts += 1;
                let (client, server) = UnixStream::pair().unwrap();
                let delay = if attempts == 1 {
                    Duration::from_millis(75)
                } else {
                    Duration::ZERO
                };
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    let _ = tungstenite::accept(server);
                });
                Ok(Some(((), client)))
            },
        );

        assert!(result.is_ok(), "{result:?}");
        assert!(attempts >= 2);
    }

    #[test]
    fn accepts_native_child_in_the_service_cgroup() {
        let root = tempdir().unwrap();
        for pid in [100, 101] {
            let process = root.path().join(pid.to_string());
            fs::create_dir(&process).unwrap();
            fs::write(
                process.join("cgroup"),
                "0::/system.slice/codex-shared-app-server@test.service\n",
            )
            .unwrap();
        }
        assert!(listener_belongs_to_service(root.path(), 101, 100));
    }

    #[test]
    fn rejects_same_user_listener_outside_the_service_cgroup() {
        let root = tempdir().unwrap();
        for (pid, cgroup) in [(100, "codex.service"), (200, "other.service")] {
            let process = root.path().join(pid.to_string());
            fs::create_dir(&process).unwrap();
            fs::write(process.join("cgroup"), format!("0::/user.slice/{cgroup}\n")).unwrap();
        }
        assert!(!listener_belongs_to_service(root.path(), 200, 100));
    }

    #[test]
    fn published_state_is_traversable_but_not_mutable() {
        assert_eq!(PUBLISHED_DIRECTORY_MODE & 0o222, 0);
        assert_eq!(PUBLISHED_DIRECTORY_MODE & 0o111, 0o111);
        assert_eq!(PUBLISHED_SOCKET_MODE, 0o600);
        assert_eq!(WRITABLE_DIRECTORY_MODE, 0o700);
    }
}
