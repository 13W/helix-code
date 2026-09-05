//! Discovery lock file, see `claude-code-ide-protocol-spec.md` §1.1.
//!
//! The CLI scans `<configDir>/ide/*.lock`, takes the port from the file
//! *name*, and reads the auth token and workspace folders from the JSON body.
//! Stale files left by other IDEs are cleaned up by the CLI itself (PROTO
//! §1.3), so this module never touches files it did not write.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// JSON body of `<port>.lock`. Field order matches the VS Code extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockFile {
    pub pid: u32,
    pub workspace_folders: Vec<String>,
    pub ide_name: String,
    pub transport: String,
    pub running_in_windows: bool,
    pub auth_token: String,
}

impl LockFile {
    pub fn new(
        pid: u32,
        workspace_folders: impl IntoIterator<Item = impl AsRef<Path>>,
        ide_name: impl Into<String>,
        auth_token: impl Into<String>,
    ) -> Self {
        LockFile {
            pid,
            workspace_folders: workspace_folders
                .into_iter()
                .map(|p| p.as_ref().to_string_lossy().into_owned())
                .collect(),
            ide_name: ide_name.into(),
            transport: "ws".to_string(),
            running_in_windows: cfg!(windows),
            auth_token: auth_token.into(),
        }
    }
}

/// Directory that holds lock files.
///
/// Resolution order: explicit override → `$CLAUDE_CONFIG_DIR/ide` →
/// `~/.claude/ide`. Falls back to the current directory only if the home
/// directory cannot be determined at all.
pub fn lock_dir(override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("ide");
    }
    match etcetera::home_dir() {
        Ok(home) => home.join(".claude").join("ide"),
        Err(err) => {
            log::warn!("claude-ide: cannot determine home directory ({err}); using cwd");
            PathBuf::from(".claude").join("ide")
        }
    }
}

pub fn lock_path(dir: &Path, port: u16) -> PathBuf {
    dir.join(format!("{port}.lock"))
}

/// Create the directory (`0700`) and write `<port>.lock` (`0600`, compact
/// JSON, no trailing newline — identical to `JSON.stringify`).
pub fn write(dir: &Path, port: u16, lock: &LockFile) -> io::Result<PathBuf> {
    create_dir_private(dir)?;
    let path = lock_path(dir, port);
    let body = serde_json::to_string(lock).map_err(io::Error::other)?;
    write_private(&path, body.as_bytes())?;
    Ok(path)
}

/// Remove a lock file; a missing file is not an error.
pub fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn create_dir_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_private(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private(path: &Path, body: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies when the file is created; enforce it on overwrite too.
    file.set_permissions(
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )?;
    file.write_all(body)?;
    file.flush()
}

#[cfg(not(unix))]
fn write_private(path: &Path, body: &[u8]) -> io::Result<()> {
    std::fs::write(path, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_like_the_extension() {
        let lock = LockFile::new(42, ["/tmp/ws"], "Helix", "tok");
        let json = serde_json::to_string(&lock).unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"pid":42,"workspaceFolders":["/tmp/ws"],"ideName":"Helix","transport":"ws","runningInWindows":{},"authToken":"tok"}}"#,
                cfg!(windows)
            )
        );
    }

    #[test]
    fn write_and_remove_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ide");
        let lock = LockFile::new(1, ["/a"], "Helix", "t");
        let path = write(&dir, 12345, &lock).unwrap();
        assert_eq!(path, dir.join("12345.lock"));
        let parsed: LockFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed, lock);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        remove(&path).unwrap();
        assert!(!path.exists());
        // second removal is not an error
        remove(&path).unwrap();
    }

    #[test]
    fn override_dir_wins() {
        assert_eq!(lock_dir(Some(Path::new("/x"))), PathBuf::from("/x"));
    }
}
