use crate::core::config::Config;

pub fn build_client(cfg: &Config) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(cfg.connect_timeout)
        // reqwest's read_timeout is an inter-read idle timeout that resets on every
        // read, not a headers-only TTFB cap — it also governs gaps between streamed
        // SSE chunks once a response is streaming. Use idle_timeout (the more
        // permissive value, meant for exactly this role) rather than ttfb_timeout,
        // so a valid slow stream isn't killed by a tighter TTFB-oriented value.
        // reqwest has no separate mechanism for a headers-only TTFB deadline, and
        // AppState holds a single shared client, so this is a deliberate v1
        // simplification: ttfb_timeout is reserved for a future distinct enforcement
        // (e.g. a second client used only up to response-headers) if that's ever
        // needed. Do NOT set an overall .timeout() — long valid streamed bodies must
        // not be killed by a deadline.
        .read_timeout(cfg.idle_timeout)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_nodelay(true)
        // Never follow redirects (SEC-10): reqwest strips only
        // Authorization/Cookie on a cross-host hop, so a 3xx from an upstream
        // would hand `x-api-key` / `ChatGPT-Account-ID` to another host, or
        // relay an internal address's body to the caller. A 3xx is passed
        // back as an ordinary (retryable) upstream response instead.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest client")
}

/// Error bodies are relayed to the caller / logged: 1 MiB is plenty (SEC-16).
pub const MAX_ERROR_BODY: usize = 1024 * 1024;
/// Whole-body reads of a successful non-streaming response (aggregation).
pub const MAX_BUFFERED_BODY: usize = 64 * 1024 * 1024;

/// Read at most `cap` bytes of an upstream body as (lossy) text; the rest is
/// dropped. A read error ends the body early, like the
/// `.text().await.unwrap_or_default()` it replaces - an upstream can no
/// longer make the gateway buffer an unbounded error body.
pub async fn read_text_truncated(mut resp: reqwest::Response, cap: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        let room = cap - buf.len();
        if chunk.len() >= room {
            buf.extend_from_slice(&chunk[..room]);
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Read a whole upstream body, failing once it exceeds `cap` bytes.
pub async fn read_body_limited(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
        if buf.len() + chunk.len() > cap {
            return Err(format!("upstream body exceeds {} MiB", cap / (1024 * 1024)));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
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
            dataset_log_dir: std::path::PathBuf::from("dataset-logs"),
            media: Default::default(),
        }
    }

    #[test]
    fn build_client_returns_usable_client() {
        let client = build_client(&cfg());
        // Smoke: the builder did not panic and produced a Client we can clone cheaply.
        let _c2 = client.clone();
    }
}
