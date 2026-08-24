use std::process::{Command, Stdio};

use crate::config::Config;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterStatus {
    Running,
    Stopped,
    Unknown,
}

pub fn codex_writer_status() -> WriterStatus {
    let uid = unsafe { libc::geteuid() };
    match Command::new("pgrep")
        .args(["-u", &uid.to_string(), "-x", "codex(-.*)?"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => WriterStatus::Running,
        Ok(status) if status.code() == Some(1) => WriterStatus::Stopped,
        Ok(_) | Err(_) => WriterStatus::Unknown,
    }
}

pub fn writers_running(config: &Config) -> bool {
    codex_writer_status() != WriterStatus::Stopped || config.server_start_marker.exists()
}
