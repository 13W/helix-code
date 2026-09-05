//! Cosmetic process information for `:claude-ide-status` (T8 §3.7).
//!
//! The only place with platform `cfg`s: Linux reads `/proc/<pid>/cwd`, every
//! other platform reports "unknown" (`None`) for now.

use std::path::PathBuf;

/// Current working directory of `pid`, when the platform exposes it.
pub fn cwd_of_pid(pid: u32) -> Option<PathBuf> {
    cwd_of_pid_impl(pid)
}

#[cfg(target_os = "linux")]
fn cwd_of_pid_impl(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(target_os = "linux"))]
fn cwd_of_pid_impl(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn own_cwd_when_supported() {
        let cwd = super::cwd_of_pid(std::process::id());
        if cfg!(target_os = "linux") {
            assert_eq!(cwd, std::env::current_dir().ok());
        } else {
            assert!(cwd.is_none());
        }
    }
}
