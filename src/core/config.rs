use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub sqlite_path: String,
    pub shared_secret: String,
    pub seed_path: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub ttfb_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_body_bytes: usize,
    pub drain_timeout: Duration,
}

fn env_secs(key: &str, default: u64) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default))
}

pub const SECRET_FILE_NAME: &str = ".router_secret";

/// Where a resolved admin secret came from, or that none exists yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    Env(String),
    SidecarFile(String),
    BootstrapNeeded,
}

impl SecretSource {
    pub fn into_secret(self) -> Option<String> {
        match self {
            SecretSource::Env(s) | SecretSource::SidecarFile(s) => Some(s),
            SecretSource::BootstrapNeeded => None,
        }
    }
}

pub fn secret_file_path(sqlite_path: &str) -> PathBuf {
    // `Path::parent()` of a bare filename is Some(""), which joins correctly
    // to a plain relative ".router_secret"; None only happens for a path that
    // terminates in a root/prefix, where CWD is the only sane fallback.
    match std::path::Path::new(sqlite_path).parent() {
        Some(dir) => dir.join(SECRET_FILE_NAME),
        None => PathBuf::from(SECRET_FILE_NAME),
    }
}

/// env -> sidecar file -> BootstrapNeeded.
///
/// A sidecar file that exists but cannot be read is a fail-fast error: we must
/// never silently generate a replacement, because whatever secret it held has
/// already been handed out to real callers.
pub fn resolve_shared_secret(sqlite_path: &str) -> anyhow::Result<SecretSource> {
    if let Ok(s) = std::env::var("ROUTER_SHARED_SECRET") {
        if !s.is_empty() {
            return Ok(SecretSource::Env(s));
        }
    }
    let path = secret_file_path(sqlite_path);
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                anyhow::bail!("secret file {path:?} is empty; delete it to re-bootstrap, or set ROUTER_SHARED_SECRET");
            }
            Ok(SecretSource::SidecarFile(trimmed.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SecretSource::BootstrapNeeded),
        Err(e) => Err(anyhow::anyhow!("failed to read secret file {path:?}: {e}")),
    }
}

/// 32 CSPRNG bytes, lowercase hex.
pub fn generate_secret() -> String {
    use rand::RngCore;
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write the sidecar file with owner-only permissions.
pub fn persist_secret(sqlite_path: &str, secret: &str) -> anyhow::Result<()> {
    let path = secret_file_path(sqlite_path);
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    // Create with owner-only permissions from the start via OpenOptions'
    // mode(), rather than write() then chmod() after - the latter leaves a
    // window where the file briefly exists at the process umask's default
    // (often group/world-readable) before being tightened, during which a
    // local user watching the directory could read the secret.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("failed to create secret file {path:?}: {e}"))?;
        f.write_all(secret.as_bytes())
            .map_err(|e| anyhow::anyhow!("failed to write secret file {path:?}: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, secret)
            .map_err(|e| anyhow::anyhow!("failed to write secret file {path:?}: {e}"))?;
    }
    Ok(())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        let sqlite_path = sqlite_path_from_env();
        let secret = resolve_shared_secret(&sqlite_path)?
            .into_secret()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no admin secret: set ROUTER_SHARED_SECRET, or run `1router setup` \
                     to create {:?}",
                    secret_file_path(&sqlite_path)
                )
            })?;
        Config::from_env_with_secret(secret)
    }

    pub fn from_env_with_secret(shared_secret: String) -> anyhow::Result<Config> {
        let listen_addr = std::env::var("ROUTER_LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse()?;
        let seed_path = std::env::var("ROUTER_SEED_PATH").ok().map(PathBuf::from);
        let max_body_bytes = std::env::var("ROUTER_MAX_BODY_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10 * 1024 * 1024);

        Ok(Config {
            listen_addr,
            sqlite_path: sqlite_path_from_env(),
            shared_secret,
            seed_path,
            connect_timeout: env_secs("ROUTER_CONNECT_TIMEOUT", 10),
            ttfb_timeout: env_secs("ROUTER_TTFB_TIMEOUT", 60),
            idle_timeout: env_secs("ROUTER_IDLE_TIMEOUT", 120),
            max_body_bytes,
            drain_timeout: env_secs("ROUTER_DRAIN_TIMEOUT", 30),
        })
    }
}

/// Read once, in one place: `main.rs` needs the DB path *before* it can
/// resolve the secret (the sidecar lives next to the DB file).
pub fn sqlite_path_from_env() -> String {
    std::env::var("ROUTER_SQLITE_PATH").unwrap_or_else(|_| "1router.db".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // std::env is process-global; cargo runs #[test] fns on multiple threads by
    // default, so tests that set/remove env vars must serialize on this lock or
    // they race each other's ROUTER_* variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn from_env_reads_required_and_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("ROUTER_LISTEN_ADDR", "127.0.0.1:9999");
        std::env::set_var("ROUTER_SQLITE_PATH", "/tmp/x.db");
        std::env::set_var("ROUTER_SHARED_SECRET", "s3cret");
        std::env::remove_var("ROUTER_SEED_PATH");

        let c = Config::from_env().unwrap();
        assert_eq!(c.listen_addr.to_string(), "127.0.0.1:9999");
        assert_eq!(c.sqlite_path, "/tmp/x.db");
        assert_eq!(c.shared_secret, "s3cret");
        assert!(c.seed_path.is_none());
        assert_eq!(c.connect_timeout, std::time::Duration::from_secs(10));
        assert_eq!(c.max_body_bytes, 10 * 1024 * 1024);

        std::env::remove_var("ROUTER_SHARED_SECRET");
    }

    #[test]
    fn from_env_errors_without_secret_or_sidecar() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Must be a fresh tempdir, NOT /tmp/x.db: the sidecar for /tmp/x.db is
        // /tmp/.router_secret, which a previous real run on this machine may
        // have created - it would make this test pass/fail depending on
        // unrelated local state.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        std::env::set_var("ROUTER_SQLITE_PATH", db.to_str().unwrap());
        std::env::remove_var("ROUTER_SHARED_SECRET");
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn resolve_prefers_env_over_sidecar_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("r.db");
        let db = db.to_str().unwrap();
        persist_secret(db, "from-file").unwrap();
        std::env::set_var("ROUTER_SHARED_SECRET", "from-env");

        assert_eq!(
            resolve_shared_secret(db).unwrap(),
            SecretSource::Env("from-env".into())
        );
        std::env::remove_var("ROUTER_SHARED_SECRET");
    }

    #[test]
    fn resolve_reads_sidecar_file_when_env_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("r.db");
        let db = db.to_str().unwrap();
        // trailing newline must be trimmed (people edit this file by hand)
        std::fs::write(secret_file_path(db), "  from-file\n").unwrap();
        std::env::remove_var("ROUTER_SHARED_SECRET");

        assert_eq!(
            resolve_shared_secret(db).unwrap(),
            SecretSource::SidecarFile("from-file".into())
        );
    }

    #[test]
    fn resolve_signals_bootstrap_needed_when_neither_exists() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("r.db");
        std::env::remove_var("ROUTER_SHARED_SECRET");

        assert_eq!(
            resolve_shared_secret(db.to_str().unwrap()).unwrap(),
            SecretSource::BootstrapNeeded
        );
    }

    #[test]
    fn secret_file_sits_next_to_the_db_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sub").join("r.db");
        let db = db.to_str().unwrap();
        assert_eq!(secret_file_path(db), std::path::Path::new(db).parent().unwrap().join(".router_secret"));

        persist_secret(db, "abc").unwrap();
        assert_eq!(std::fs::read_to_string(secret_file_path(db)).unwrap(), "abc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(secret_file_path(db)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "sidecar must be owner-read/write only");
        }
    }

    #[test]
    fn secret_file_path_handles_bare_relative_filename() {
        // The default sqlite_path is "1router.db" - no parent component at all.
        assert_eq!(
            secret_file_path("1router.db"),
            std::path::Path::new(".router_secret")
        );
    }

    #[test]
    fn generated_secret_is_64_hex_chars_and_unique() {
        let a = generate_secret();
        let b = generate_secret();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
