//! Credential discovery for Command Code, mirroring pi's `api-key.ts`:
//! 1. `COMMANDCODE_API_KEY` env var (pi-compatible; also honors
//!    `ROUTER_COMMANDCODE_API_KEY` as an explicit 1router-prefixed override)
//! 2. `~/.commandcode/auth.json` -> `{apiKey}`, `{commandcode}`, or
//!    `{command-code}`, each possibly `{type:"api",key}` or
//!    `{type:"oauth",access}` shaped
//! 3. `~/.pi/agent/auth.json`, `~/.omp/agent/auth.json` (same shapes)

use serde_json::Value;

fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }
}

/// Pull the raw key string out of one auth.json entry, handling both the
/// plain-string shape (`"apiKey": "user_..."`) and the nested
/// `{type, key|access}` shape.
fn extract_entry(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(key) => Some(key.clone()),
        Value::Object(map) => {
            let key = map
                .get("key")
                .and_then(Value::as_str)
                .or_else(|| map.get("access").and_then(Value::as_str))?;
            Some(key.to_string())
        }
        _ => None,
    }
}

fn key_from_auth_json(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    extract_entry(value.get("apiKey"))
        .or_else(|| extract_entry(value.get("commandcode")))
        .or_else(|| extract_entry(value.get("command-code")))
}

pub fn commandcode_key_from_disk() -> Option<String> {
    if let Ok(key) = std::env::var("ROUTER_COMMANDCODE_API_KEY") {
        let key = key.trim();
        if !key.is_empty() {
            return Some(key.to_string());
        }
    }
    if let Ok(key) = std::env::var("COMMANDCODE_API_KEY") {
        let key = key.trim();
        if !key.is_empty() {
            return Some(key.to_string());
        }
    }
    let home = home_dir()?;
    let candidates = [
        home.join(".commandcode").join("auth.json"),
        home.join(".pi").join("agent").join("auth.json"),
        home.join(".omp").join("agent").join("auth.json"),
    ];
    candidates.iter().find_map(|path| key_from_auth_json(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_auth(dir: &tempfile::TempDir, content: &str) -> std::path::PathBuf {
        let path = dir.path().join("auth.json");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn reads_plain_api_key_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth(
            &dir,
            r#"{"apiKey":"user_plain","userId":"u1","userName":"n","keyName":"k"}"#,
        );
        assert_eq!(key_from_auth_json(&path).as_deref(), Some("user_plain"));
    }

    #[test]
    fn reads_nested_api_type_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth(&dir, r#"{"apiKey":{"type":"api","key":"user_nested"}}"#);
        assert_eq!(key_from_auth_json(&path).as_deref(), Some("user_nested"));
    }

    #[test]
    fn reads_nested_oauth_access_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth(&dir, r#"{"apiKey":{"type":"oauth","access":"user_oauth"}}"#);
        assert_eq!(key_from_auth_json(&path).as_deref(), Some("user_oauth"));
    }

    #[test]
    fn falls_back_to_commandcode_and_command_dash_code_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth(&dir, r#"{"commandcode":"user_cc"}"#);
        assert_eq!(key_from_auth_json(&path).as_deref(), Some("user_cc"));
        let path = write_auth(&dir, r#"{"command-code":{"type":"api","key":"user_ccdash"}}"#);
        assert_eq!(key_from_auth_json(&path).as_deref(), Some("user_ccdash"));
    }

    #[test]
    fn prefers_api_key_over_commandcode_when_both_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth(
            &dir,
            r#"{"apiKey":"user_first","commandcode":"user_second"}"#,
        );
        assert_eq!(key_from_auth_json(&path).as_deref(), Some("user_first"));
    }

    #[test]
    fn returns_none_for_missing_or_garbage_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert_eq!(key_from_auth_json(&missing), None);
        let path = write_auth(&dir, "not json");
        assert_eq!(key_from_auth_json(&path), None);
    }
}
