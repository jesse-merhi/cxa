use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::fcntl::{AT_FDCWD, AtFlags};
use nix::unistd::linkat;
use tempfile::{Builder, NamedTempFile};

use crate::{Error, Result};

pub const OWNED_TEMP_MARKER: &str = ".cxa-owned-temp";
pub const STAGING_WRITER_LOCK: &str = ".cxa-writer.lock";

#[derive(Debug)]
pub struct DeadlineUnixStream {
    inner: UnixStream,
    deadline: Option<Instant>,
}

impl DeadlineUnixStream {
    pub fn new(inner: UnixStream, deadline: Instant) -> Self {
        Self {
            inner,
            deadline: Some(deadline),
        }
    }

    pub fn without_deadline(inner: UnixStream) -> Self {
        Self {
            inner,
            deadline: None,
        }
    }

    pub fn clear_deadline(&mut self) {
        self.deadline = None;
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    fn remaining(&self) -> std::io::Result<Option<Duration>> {
        let Some(deadline) = self.deadline else {
            return Ok(None);
        };
        deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .map(Some)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "WebSocket handshake deadline elapsed",
                )
            })
    }
}

impl Read for DeadlineUnixStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if let Some(remaining) = self.remaining()? {
            self.inner.set_read_timeout(Some(remaining))?;
        }
        self.inner.read(buffer)
    }
}

impl Write for DeadlineUnixStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if let Some(remaining) = self.remaining()? {
            self.inner.set_write_timeout(Some(remaining))?;
        }
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(remaining) = self.remaining()? {
            self.inner.set_write_timeout(Some(remaining))?;
        }
        self.inner.flush()
    }
}

impl AsFd for DeadlineUnixStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.as_fd()
    }
}

pub struct ExclusiveLock {
    file: File,
}

impl ExclusiveLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            private_dir(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| Error::io(path, error))?;
        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if status != 0 {
            return Err(Error::io(path, std::io::Error::last_os_error()));
        }
        Ok(Self { file })
    }

    pub fn acquire_inheritable(path: &Path) -> Result<Self> {
        let lock = Self::acquire(path)?;
        let flags = unsafe { libc::fcntl(lock.file.as_raw_fd(), libc::F_GETFD) };
        if flags == -1 {
            return Err(Error::io(path, std::io::Error::last_os_error()));
        }
        let status = unsafe {
            libc::fcntl(
                lock.file.as_raw_fd(),
                libc::F_SETFD,
                flags & !libc::FD_CLOEXEC,
            )
        };
        if status == -1 {
            return Err(Error::io(path, std::io::Error::last_os_error()));
        }
        Ok(lock)
    }

    pub fn is_held(path: &Path) -> Result<bool> {
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(Error::io(path, error)),
        };
        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if status == 0 {
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(true);
        }
        Err(Error::io(path, error))
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn private_dir(path: &Path) -> Result<()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(Error::Message(format!(
                "{} exists and is not a directory",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::io(path, error)),
    }
    let mut created = Vec::new();
    let mut candidate = Some(path);
    while let Some(directory) = candidate {
        match fs::metadata(directory) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                created.push(directory.to_owned());
                candidate = directory.parent();
            }
            Err(error) => return Err(Error::io(directory, error)),
        }
    }
    fs::create_dir_all(path).map_err(|error| Error::io(path, error))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| Error::io(path, error))?;
    for directory in created.iter().rev() {
        sync_parent(directory)?;
    }
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Message(format!("{} has no parent directory", path.display())))?;
    private_dir(parent)?;
    let mut temporary = unique_temporary(parent, mode)?;
    temporary
        .write_all(bytes)
        .map_err(|error| Error::io(temporary.path(), error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| Error::io(temporary.path(), error))?;
    temporary
        .persist(path)
        .map_err(|error| Error::io(path, error.error))?;
    sync_parent(path)
}

pub fn atomic_copy(source: &Path, target: &Path, mode: u32) -> Result<()> {
    let bytes = fs::read(source).map_err(|error| Error::io(source, error))?;
    atomic_write(target, &bytes, mode)
}

pub fn atomic_copy_if_absent(source: &Path, target: &Path, mode: u32) -> Result<bool> {
    let bytes = fs::read(source).map_err(|error| Error::io(source, error))?;
    let parent = target
        .parent()
        .ok_or_else(|| Error::Message(format!("{} has no parent directory", target.display())))?;
    private_dir(parent)?;
    let mut temporary = unique_temporary(parent, mode)?;
    (|| {
        temporary
            .write_all(&bytes)
            .map_err(|error| Error::io(temporary.path(), error))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| Error::io(temporary.path(), error))?;
        match fs::hard_link(temporary.path(), target) {
            Ok(()) => {
                sync_parent(target)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(Error::io(target, error)),
        }
    })()
}

pub fn hard_link_entry_if_absent(source: &Path, target: &Path) -> Result<bool> {
    match linkat(AT_FDCWD, source, AT_FDCWD, target, AtFlags::empty()) {
        Ok(()) => {
            sync_parent(target)?;
            Ok(true)
        }
        Err(nix::errno::Errno::EEXIST) => Ok(false),
        Err(error) => Err(Error::io(target, error.into())),
    }
}

fn unique_temporary(parent: &Path, mode: u32) -> Result<NamedTempFile> {
    Builder::new()
        .prefix(".cxa-atomic-")
        .permissions(fs::Permissions::from_mode(mode))
        .tempfile_in(parent)
        .map_err(|error| Error::io(parent, error))
}

pub fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io(path, error)),
    }
}

pub fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .map(|parent| {
            if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            }
        })
        .ok_or_else(|| Error::Message(format!("{} has no parent directory", path.display())))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| Error::io(parent, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_does_not_repermission_an_existing_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();

        private_dir(root.path()).unwrap();

        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn private_dir_creates_nested_private_directories() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("store/profile-1");

        private_dir(&nested).unwrap();

        assert!(nested.is_dir());
        assert_eq!(
            fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn atomic_copy_if_absent_preserves_an_existing_target() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::write(&source, b"candidate").unwrap();
        fs::write(&target, b"writer").unwrap();

        assert!(!atomic_copy_if_absent(&source, &target, 0o600).unwrap());
        assert_eq!(fs::read(&target).unwrap(), b"writer");
    }

    #[test]
    fn atomic_writes_ignore_temporary_files_left_by_a_reused_pid() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::write(&source, b"copied").unwrap();
        for suffix in ["new", "candidate"] {
            fs::write(
                root.path()
                    .join(format!("target.{suffix}.{}", std::process::id())),
                b"stale",
            )
            .unwrap();
        }

        atomic_write(&target, b"fresh", 0o600).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"fresh");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&target).unwrap();
        assert!(atomic_copy_if_absent(&source, &target, 0o600).unwrap());
        assert_eq!(fs::read(&target).unwrap(), b"copied");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn hard_link_entry_if_absent_does_not_follow_or_replace() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source-link");
        let target = root.path().join("target-link");
        let occupied = root.path().join("occupied");
        std::os::unix::fs::symlink("credential.json", &source).unwrap();

        assert!(hard_link_entry_if_absent(&source, &target).unwrap());
        assert_eq!(
            fs::read_link(&target).unwrap(),
            Path::new("credential.json")
        );
        fs::write(&occupied, b"writer").unwrap();
        assert!(!hard_link_entry_if_absent(&source, &occupied).unwrap());
        assert_eq!(fs::read(&occupied).unwrap(), b"writer");
    }
}
