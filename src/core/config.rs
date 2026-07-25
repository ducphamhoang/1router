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

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        let listen_addr = std::env::var("ROUTER_LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse()?;
        let sqlite_path =
            std::env::var("ROUTER_SQLITE_PATH").unwrap_or_else(|_| "1router.db".to_string());
        let shared_secret = std::env::var("ROUTER_SHARED_SECRET")
            .map_err(|_| anyhow::anyhow!("ROUTER_SHARED_SECRET is required"))?;
        let seed_path = std::env::var("ROUTER_SEED_PATH").ok().map(PathBuf::from);
        let max_body_bytes = std::env::var("ROUTER_MAX_BODY_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10 * 1024 * 1024);

        Ok(Config {
            listen_addr,
            sqlite_path,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_reads_required_and_defaults() {
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
    }

    #[test]
    fn from_env_errors_without_secret() {
        std::env::set_var("ROUTER_SQLITE_PATH", "/tmp/x.db");
        std::env::remove_var("ROUTER_SHARED_SECRET");
        assert!(Config::from_env().is_err());
    }
}
