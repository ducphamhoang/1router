use crate::core::config::Config;

pub fn build_client(cfg: &Config) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(cfg.connect_timeout)
        // TTFB: cap time to receive response headers, but do NOT set an overall
        // .timeout() — long valid streamed bodies must not be killed by a deadline.
        .read_timeout(cfg.ttfb_timeout)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_nodelay(true)
        .build()
        .expect("failed to build reqwest client")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use std::time::Duration;

    fn cfg() -> Config {
        Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            sqlite_path: ":memory:".into(),
            shared_secret: "x".into(),
            seed_path: None,
            connect_timeout: Duration::from_secs(3),
            ttfb_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(7),
            max_body_bytes: 1024,
            drain_timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn build_client_returns_usable_client() {
        let client = build_client(&cfg());
        // Smoke: the builder did not panic and produced a Client we can clone cheaply.
        let _c2 = client.clone();
    }
}
