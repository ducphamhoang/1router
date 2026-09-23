//! Owner-only file permissions for files holding secrets or prompt data:
//! the shared-secret sidecar, the SQLite DB (plaintext provider keys, OAuth
//! tokens) and dataset-log JSONL (full prompts) - SEC-13.
//!
//! Unix: `chmod 600`. Windows: best effort via `icacls` - drop inherited
//! ACEs and grant only the current user full control. Failures are logged,
//! never fatal: a gateway that can't tighten an ACL should still start.

use std::path::Path;

pub fn restrict_to_owner(path: &Path) {
    if let Err(e) = restrict_impl(path) {
        tracing::warn!(path = ?path, error = %e, "could not restrict file permissions to owner");
    }
}

#[cfg(unix)]
fn restrict_impl(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn restrict_impl(path: &Path) -> std::io::Result<()> {
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) if !d.is_empty() && !u.is_empty() => format!("{d}\\{u}"),
        (_, Ok(u)) if !u.is_empty() => u,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "USERNAME not set",
            ))
        }
    };
    let out = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:F"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "icacls exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(not(any(unix, windows)))]
fn restrict_impl(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The DB file plus its WAL/SHM siblings, when they exist on disk
/// (`:memory:` and URI forms are skipped).
pub fn restrict_sqlite_files(sqlite_path: &str) {
    let base = Path::new(sqlite_path);
    if !base.is_file() {
        return;
    }
    restrict_to_owner(base);
    for suffix in ["-wal", "-shm"] {
        let mut p = base.as_os_str().to_owned();
        p.push(suffix);
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            restrict_to_owner(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn restricts_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "x").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        restrict_to_owner(&p);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn restricted_file_stays_readable_and_writable_by_owner() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "x").unwrap();
        restrict_to_owner(&p);
        std::fs::write(&p, "y").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "y");
    }
}
