use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tempfile::{Builder, NamedTempFile};

use crate::{Error, Result};

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
        flock(file.as_raw_fd(), libc::LOCK_EX).map_err(|error| Error::io(path, error))?;
        Ok(Self { file })
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        let _ = flock(self.file.as_raw_fd(), libc::LOCK_UN);
    }
}

fn flock(file: RawFd, operation: libc::c_int) -> std::io::Result<()> {
    let status = unsafe { libc::flock(file, operation) };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
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
    fs::create_dir_all(path).map_err(|error| Error::io(path, error))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| Error::io(path, error))?;
    sync_parent(path)
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
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| Error::io(parent, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_is_private_when_created() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("store/profile-1");

        private_dir(&nested).unwrap();

        assert_eq!(
            fs::metadata(nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn atomic_write_replaces_a_symlink_entry() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join("auth.json");
        fs::write(&target, b"old").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        atomic_write(&link, b"new", 0o600).unwrap();

        assert_eq!(fs::read(&link).unwrap(), b"new");
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }
}
