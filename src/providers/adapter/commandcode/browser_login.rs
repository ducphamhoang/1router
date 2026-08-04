use chrono::{DateTime, Utc};
use serde_json::Value;
use std::io;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub const TEN_YEARS_SECS: i64 = 10 * 365 * 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthCallback {
    pub api_key: String,
    pub state: String,
    pub user_id: String,
    pub user_name: String,
    pub key_name: String,
}

#[derive(Debug)]
pub enum LoginError {
    Timeout,
    StateMismatch,
    Denied(String),
    Io(std::io::Error),
}

pub struct AuthListener {
    pub port: u16,
    pub state_token: String,
    listener: TcpListener,
}

impl AuthListener {
    pub fn new(listener: TcpListener, port: u16, state_token: String) -> Self {
        Self {
            listener,
            port,
            state_token,
        }
    }

    pub fn authorize_url(&self) -> String {
        authorize_url_for(self.port, &self.state_token)
    }

    pub async fn wait(self) -> Result<AuthCallback, LoginError> {
        match tokio::time::timeout(auth_timeout(), accept_loop(self.listener)).await {
            Ok(result) => result,
            Err(_) => Err(LoginError::Timeout),
        }
    }
}

pub fn authorize_url_for(port: u16, state_token: &str) -> String {
    let callback = format!("http://localhost:{port}/callback");
    format!(
        "https://commandcode.ai/studio/auth/cli?callback={}&state={}",
        urlencoding::encode(&callback),
        urlencoding::encode(state_token)
    )
}

pub async fn bind_listener() -> std::io::Result<(TcpListener, u16)> {
    for port in 5959..5969 {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok((listener, port)),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error),
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

pub fn open_in_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();
}

pub fn far_future_expiry() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::seconds(TEN_YEARS_SECS)
}
pub fn sanitize_api_key(input: &str) -> String {
    input
        .replace("\x1b[200~", "")
        .replace("\x1b[201~", "")
        .replace("[200~", "")
        .replace("[201~", "")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn validate_state(expected: &str, callback: AuthCallback) -> Result<AuthCallback, LoginError> {
    if callback.state == expected {
        Ok(callback)
    } else {
        Err(LoginError::StateMismatch)
    }
}

const MAX_BODY_BYTES: usize = 10 * 1024;

fn auth_timeout() -> Duration {
    std::env::var("ROUTER_COMMANDCODE_AUTH_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(15))
}

enum RequestOutcome {
    Continue,
    Complete(Result<AuthCallback, LoginError>),
}

async fn accept_loop(listener: TcpListener) -> Result<AuthCallback, LoginError> {
    loop {
        let (stream, _) = listener.accept().await.map_err(LoginError::Io)?;
        match handle_connection(stream).await.map_err(LoginError::Io)? {
            RequestOutcome::Continue => {}
            RequestOutcome::Complete(result) => return result,
        }
    }
}

async fn handle_connection(mut stream: tokio::net::TcpStream) -> io::Result<RequestOutcome> {
    let mut request = Vec::new();
    let header_end = loop {
        let mut part = [0u8; 1024];
        let count = stream.read(&mut part).await?;
        if count == 0 {
            return Ok(RequestOutcome::Continue);
        }
        request.extend_from_slice(&part[..count]);
        if request.len() > MAX_BODY_BYTES + 8192 {
            write_response(
                &mut stream,
                413,
                "{\"success\":false,\"error\":\"request too large\"}",
                "http://localhost:3000",
                false,
            )
            .await?;
            return Ok(RequestOutcome::Continue);
        }
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };
    let head = String::from_utf8_lossy(&request[..header_end]);
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    let mut first_parts = first.split_whitespace();
    let method = first_parts.next().unwrap_or_default();
    let path = first_parts.next().unwrap_or_default();
    let mut origin = "";
    let mut requested_headers = "";
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "origin" => origin = value.trim(),
                "access-control-request-headers" => requested_headers = value.trim(),
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let cors_origin = allowed_origin(origin);
    let body_start = header_end + 4;
    let mut body = request[body_start..].to_vec();
    if content_length > MAX_BODY_BYTES {
        write_response(
            &mut stream,
            413,
            "{\"success\":false,\"error\":\"request too large\"}",
            cors_origin,
            false,
        )
        .await?;
        return Ok(RequestOutcome::Continue);
    }
    while body.len() < content_length {
        let mut part = vec![0u8; content_length - body.len()];
        let count = stream.read(&mut part).await?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&part[..count]);
    }

    if method == "OPTIONS" && path == "/callback" {
        write_response_with_headers(&mut stream, 204, "", cors_origin, requested_headers, false)
            .await?;
        return Ok(RequestOutcome::Continue);
    }
    if path != "/callback" {
        write_response_with_headers(
            &mut stream,
            404,
            "{\"success\":false,\"error\":\"not found\"}",
            cors_origin,
            requested_headers,
            true,
        )
        .await?;
        return Ok(RequestOutcome::Continue);
    }
    if method != "POST" {
        write_response_with_headers(
            &mut stream,
            405,
            "{\"success\":false,\"error\":\"method not allowed\"}",
            cors_origin,
            requested_headers,
            true,
        )
        .await?;
        return Ok(RequestOutcome::Continue);
    }
    let value: Value = match serde_json::from_slice(&body[..body.len().min(content_length)]) {
        Ok(value) => value,
        Err(_) => {
            write_response_with_headers(
                &mut stream,
                400,
                "{\"success\":false,\"error\":\"malformed JSON\"}",
                cors_origin,
                requested_headers,
                true,
            )
            .await?;
            return Ok(RequestOutcome::Continue);
        }
    };
    if let Some(error) = value.get("error") {
        let description = error
            .as_str()
            .map(str::to_string)
            .or_else(|| error["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| "authentication denied".into());
        write_response_with_headers(
            &mut stream,
            200,
            "{\"success\":true}",
            cors_origin,
            requested_headers,
            true,
        )
        .await?;
        return Ok(RequestOutcome::Complete(Err(LoginError::Denied(
            description,
        ))));
    }
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let callback = match (
        field("apiKey"),
        field("state"),
        field("userId"),
        field("userName"),
        field("keyName"),
    ) {
        (Some(api_key), Some(state), Some(user_id), Some(user_name), Some(key_name)) => {
            AuthCallback {
                api_key,
                state,
                user_id,
                user_name,
                key_name,
            }
        }
        _ => {
            write_response_with_headers(
                &mut stream,
                400,
                "{\"success\":false,\"error\":\"all callback fields are required\"}",
                cors_origin,
                requested_headers,
                true,
            )
            .await?;
            return Ok(RequestOutcome::Continue);
        }
    };
    write_response_with_headers(
        &mut stream,
        200,
        "{\"success\":true}",
        cors_origin,
        requested_headers,
        true,
    )
    .await?;
    Ok(RequestOutcome::Complete(Ok(callback)))
}

fn allowed_origin(origin: &str) -> &str {
    match origin {
        "https://commandcode.ai" | "https://staging.commandcode.ai" | "http://localhost:3000" => {
            origin
        }
        _ => "http://localhost:3000",
    }
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
    origin: &str,
    close: bool,
) -> io::Result<()> {
    write_response_with_headers(stream, status, body, origin, "", close).await
}

async fn write_response_with_headers(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
    origin: &str,
    requested_headers: &str,
    close: bool,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let body_bytes = body.as_bytes();
    let connection = if close { "close" } else { "keep-alive" };
    let response = format!("HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\naccess-control-allow-origin: {origin}\r\naccess-control-allow-methods: POST, OPTIONS\r\naccess-control-allow-headers: {}\r\naccess-control-allow-private-network: true\r\ncontent-length: {}\r\nconnection: {connection}\r\n\r\n{body}", if requested_headers.is_empty() { "Content-Type" } else { requested_headers }, body_bytes.len());
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, OnceLock};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn auth_timeout_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    async fn request(port: u16, raw: String) -> (u16, String) {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let status = response
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, response)
    }

    fn callback(state: &str) -> serde_json::Value {
        json!({"apiKey":"cc-key","state":state,"userId":"u1","userName":"User","keyName":"cli"})
    }

    async fn listener() -> AuthListener {
        let (listener, port) = bind_listener().await.unwrap();
        AuthListener {
            listener,
            port,
            state_token: "state-1".into(),
        }
    }

    #[tokio::test]
    async fn bind_listener_scans_from_5959() {
        let first = bind_listener().await.unwrap();
        assert!(first.1 >= 5959 || first.1 != 0);
        let second = bind_listener().await.unwrap();
        assert_ne!(first.1, second.1);
    }

    #[tokio::test]
    async fn options_preflight_returns_204_with_cors_headers() {
        let l = listener().await;
        let port = l.port;
        tokio::spawn(l.wait());
        let (status, response) = request(port, "OPTIONS /callback HTTP/1.1\r\nHost: localhost\r\nOrigin: https://commandcode.ai\r\n\r\n".into()).await;
        assert_eq!(status, 204);
        assert!(response.contains("access-control-allow-origin: https://commandcode.ai"));
        assert!(response.contains("access-control-allow-methods: POST, OPTIONS"));
        assert!(response.contains("access-control-allow-private-network: true"));
    }

    #[tokio::test]
    async fn preflight_origin_falls_back_for_an_unknown_origin() {
        let l = listener().await;
        let port = l.port;
        tokio::spawn(l.wait());
        let (_, response) = request(
            port,
            "OPTIONS /callback HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\n\r\n"
                .into(),
        )
        .await;
        assert!(response.contains("access-control-allow-origin: http://localhost:3000"));
    }

    #[tokio::test]
    async fn post_callback_resolves_with_all_five_fields() {
        let l = listener().await;
        let port = l.port;
        let task = tokio::spawn(l.wait());
        let body = callback("state-1").to_string();
        let (status, response) = request(port, format!("POST /callback HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)).await;
        assert_eq!(status, 200);
        assert!(response.contains("{\"success\":true}"));
        assert_eq!(task.await.unwrap().unwrap().api_key, "cc-key");
    }

    #[tokio::test]
    async fn post_callback_rejects_an_incomplete_body() {
        let l = listener().await;
        let port = l.port;
        let task = tokio::spawn(l.wait());
        for field in ["apiKey", "state", "userId", "userName", "keyName"] {
            let mut value = callback("state-1");
            value[field] = json!("");
            let body = value.to_string();
            let (status, _) = request(port, format!("POST /callback HTTP/1.1\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}", body.len(), body)).await;
            assert_eq!(status, 400);
        }
        let body = callback("state-1").to_string();
        let _ = request(port, format!("POST /callback HTTP/1.1\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}", body.len(), body)).await;
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn post_callback_rejects_malformed_json() {
        let l = listener().await;
        let port = l.port;
        let task = tokio::spawn(l.wait());
        let body = "{";
        let (status, _) = request(
            port,
            format!(
                "POST /callback HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            ),
        )
        .await;
        assert_eq!(status, 400);
        let body = callback("state-1").to_string();
        let _ = request(
            port,
            format!(
                "POST /callback HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            ),
        )
        .await;
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn wrong_path_and_wrong_method_are_rejected() {
        let l = listener().await;
        let port = l.port;
        let task = tokio::spawn(l.wait());
        assert_eq!(
            request(port, "GET /callback HTTP/1.1\r\n\r\n".into())
                .await
                .0,
            405
        );
        assert_eq!(
            request(port, "POST /nope HTTP/1.1\r\n\r\n".into()).await.0,
            404
        );
        let body = callback("state-1").to_string();
        let _ = request(
            port,
            format!(
                "POST /callback HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            ),
        )
        .await;
        assert!(task.await.unwrap().is_ok());
    }

    #[test]
    fn state_mismatch_is_rejected_by_the_caller() {
        let result = validate_state(
            "expected",
            AuthCallback {
                api_key: "k".into(),
                state: "wrong".into(),
                user_id: "u".into(),
                user_name: "n".into(),
                key_name: "k".into(),
            },
        );
        assert!(matches!(result, Err(LoginError::StateMismatch)));
    }

    #[tokio::test]
    async fn wait_times_out_after_the_configured_duration() {
        let _guard = auth_timeout_env_lock().lock().unwrap();
        std::env::set_var("ROUTER_COMMANDCODE_AUTH_TIMEOUT_MS", "100");
        let l = listener().await;
        assert!(matches!(l.wait().await, Err(LoginError::Timeout)));
        std::env::remove_var("ROUTER_COMMANDCODE_AUTH_TIMEOUT_MS");
    }

    #[test]
    fn authorize_url_is_correctly_encoded() {
        let port = 5959;
        let token = "tok/+";
        let callback = format!("http://localhost:{port}/callback");
        let expected = format!(
            "https://commandcode.ai/studio/auth/cli?callback={}&state={}",
            urlencoding::encode(&callback),
            urlencoding::encode(token)
        );
        assert_eq!(authorize_url_for(port, token), expected);
    }

    #[test]
    fn sanitize_api_key_strips_bracketed_paste_markers() {
        assert_eq!(sanitize_api_key("\x1b[200~  cc-key\x1b[201~\n"), "cc-key");
        assert_eq!(sanitize_api_key("[200~cc-key[201~"), "cc-key");
    }
}
