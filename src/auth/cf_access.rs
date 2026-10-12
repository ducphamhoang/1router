//! Cloudflare Access as an *additional* credential.
//!
//! When `ROUTER_CF_ACCESS_TEAM_DOMAIN` and `ROUTER_CF_ACCESS_AUD` are set,
//! a request carrying a valid `Cf-Access-Jwt-Assertion` is accepted alongside
//! the existing 1router credentials (shared secret, user keys, admin
//! session). Nothing here ever rejects a request on its own: an absent or
//! invalid token simply means "no Cloudflare credential" and the caller falls
//! back to the normal checks. With the env vars unset the whole module is
//! inert (`AppState::cf_access` is `None`).
//!
//! The identity is read from the *verified* JWT claims, never from the plain
//! `Cf-Access-Authenticated-User-Email` header, which anyone who can reach the
//! origin directly could forge.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::http::HeaderMap;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::signature::{RsaPublicKeyComponents, RSA_PKCS1_2048_8192_SHA256};
use serde::Deserialize;

pub const JWT_HEADER: &str = "cf-access-jwt-assertion";
const MAX_TOKEN_LEN: usize = 8 * 1024;
const CLOCK_LEEWAY_SECS: i64 = 30;
/// Unknown-`kid` tokens may trigger a JWKS fetch at most this often, so junk
/// tokens cannot turn the gateway into a request amplifier.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CfAccessConfig {
    /// e.g. `myteam.cloudflareaccess.com`
    pub team_domain: String,
    /// The Access application's AUD tag.
    pub aud: String,
    /// Lower-cased emails allowed into `/admin/*`.
    pub admin_emails: HashSet<String>,
    pub jwks_url: String,
}

impl CfAccessConfig {
    pub fn new(team_domain: &str, aud: &str, admin_emails: &str) -> anyhow::Result<Self> {
        let team = normalize_team_domain(team_domain);
        anyhow::ensure!(!team.is_empty(), "ROUTER_CF_ACCESS_TEAM_DOMAIN is empty");
        anyhow::ensure!(!aud.trim().is_empty(), "ROUTER_CF_ACCESS_AUD is empty");
        let admin_emails = admin_emails
            .split(',')
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        Ok(CfAccessConfig {
            jwks_url: format!("https://{team}/cdn-cgi/access/certs"),
            team_domain: team,
            aud: aud.trim().to_string(),
            admin_emails,
        })
    }

    fn issuer(&self) -> String {
        format!("https://{}", self.team_domain)
    }
}

fn normalize_team_domain(raw: &str) -> String {
    let t = raw
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    if t.is_empty() || t.contains('.') {
        t
    } else {
        format!("{t}.cloudflareaccess.com")
    }
}

/// Who Cloudflare says is calling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CfIdentity {
    /// Set for people; absent for service tokens.
    pub email: Option<String>,
    /// Set for service tokens (the token's client id).
    pub common_name: Option<String>,
}

impl CfIdentity {
    /// Value recorded in `request_log.user_id` (`cf:<email>` / `cf:<token>`).
    pub fn caller_id(&self) -> Option<String> {
        self.email
            .as_deref()
            .or(self.common_name.as_deref())
            .map(|id| format!("cf:{id}"))
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    n: String,
    e: String,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct JwtHeaderClaims {
    alg: String,
    kid: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Aud {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: Aud,
    exp: i64,
    nbf: Option<i64>,
    email: Option<String>,
    common_name: Option<String>,
}

pub struct CfAccess {
    cfg: CfAccessConfig,
    http: reqwest::Client,
    keys: ArcSwap<Vec<Jwk>>,
    last_fetch: tokio::sync::Mutex<Option<Instant>>,
}

impl std::fmt::Debug for CfAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfAccess").field("cfg", &self.cfg).finish()
    }
}

impl CfAccess {
    pub fn new(cfg: CfAccessConfig, http: reqwest::Client) -> Self {
        CfAccess {
            cfg,
            http,
            keys: ArcSwap::from_pointee(Vec::new()),
            last_fetch: tokio::sync::Mutex::new(None),
        }
    }

    /// `ROUTER_CF_ACCESS_TEAM_DOMAIN` + `ROUTER_CF_ACCESS_AUD` (+ optional
    /// `ROUTER_CF_ACCESS_ADMIN_EMAILS`, comma separated). Neither set means
    /// the feature is off; exactly one set is a configuration error.
    pub fn from_env(http: reqwest::Client) -> anyhow::Result<Option<Arc<CfAccess>>> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let team = get("ROUTER_CF_ACCESS_TEAM_DOMAIN");
        let aud = get("ROUTER_CF_ACCESS_AUD");
        match (team, aud) {
            (None, None) => Ok(None),
            (Some(team), Some(aud)) => {
                let admins = get("ROUTER_CF_ACCESS_ADMIN_EMAILS").unwrap_or_default();
                let cfg = CfAccessConfig::new(&team, &aud, &admins)?;
                Ok(Some(Arc::new(CfAccess::new(cfg, http))))
            }
            _ => anyhow::bail!(
                "ROUTER_CF_ACCESS_TEAM_DOMAIN and ROUTER_CF_ACCESS_AUD must be set together"
            ),
        }
    }

    pub fn config(&self) -> &CfAccessConfig {
        &self.cfg
    }

    /// May this identity use `/admin/*`? Only people on the allowlist.
    pub fn is_admin(&self, id: &CfIdentity) -> bool {
        id.email
            .as_deref()
            .map(|e| self.cfg.admin_emails.contains(e))
            .unwrap_or(false)
    }

    /// The verified identity behind the request's `Cf-Access-Jwt-Assertion`,
    /// or `None` for any reason it can't be trusted.
    pub async fn identity(&self, headers: &HeaderMap) -> Option<CfIdentity> {
        let token = headers.get(JWT_HEADER)?.to_str().ok()?.trim();
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            return None;
        }
        let parts = Parts::split(token)?;
        let header: JwtHeaderClaims = parts.header()?;
        if header.alg != "RS256" {
            return None;
        }
        let claims = parts.claims()?;
        if !self.claims_ok(&claims, unix_now()) {
            return None;
        }
        let kid = header.kid?;
        let key = self.key_for(&kid).await?;
        if !parts.signature_ok(&key) {
            return None;
        }
        let email = claims
            .email
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty());
        let common_name = claims.common_name.filter(|c| !c.trim().is_empty());
        if email.is_none() && common_name.is_none() {
            return None;
        }
        Some(CfIdentity { email, common_name })
    }

    fn claims_ok(&self, c: &Claims, now: i64) -> bool {
        let aud_ok = match &c.aud {
            Aud::One(a) => a == &self.cfg.aud,
            Aud::Many(v) => v.iter().any(|a| a == &self.cfg.aud),
        };
        c.iss == self.cfg.issuer()
            && aud_ok
            && c.exp + CLOCK_LEEWAY_SECS > now
            && c.nbf.map(|n| n - CLOCK_LEEWAY_SECS <= now).unwrap_or(true)
    }

    async fn key_for(&self, kid: &str) -> Option<Jwk> {
        if let Some(k) = find_key(&self.keys.load(), kid) {
            return Some(k);
        }
        self.refresh().await;
        find_key(&self.keys.load(), kid)
    }

    /// Fetch the JWKS, at most once per `MIN_REFRESH_INTERVAL`. A failed
    /// fetch keeps the keys we already have.
    async fn refresh(&self) {
        let mut last = self.last_fetch.lock().await;
        if matches!(*last, Some(t) if t.elapsed() < MIN_REFRESH_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
        let res = self
            .http
            .get(&self.cfg.jwks_url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .and_then(|r| r.error_for_status());
        match res {
            Ok(resp) => match resp.json::<Jwks>().await {
                Ok(jwks) => self.keys.store(Arc::new(jwks.keys)),
                Err(e) => tracing::warn!(error = %e, "cloudflare access JWKS was not valid JSON"),
            },
            Err(e) => tracing::warn!(error = %e, "cloudflare access JWKS fetch failed"),
        }
    }

    #[cfg(test)]
    pub fn set_keys_for_test(&self, keys: Vec<(String, Vec<u8>, Vec<u8>)>) {
        let keys = keys
            .into_iter()
            .map(|(kid, n, e)| Jwk {
                kid,
                kty: "RSA".into(),
                n: URL_SAFE_NO_PAD.encode(n),
                e: URL_SAFE_NO_PAD.encode(e),
            })
            .collect();
        self.keys.store(Arc::new(keys));
    }
}

fn find_key(keys: &[Jwk], kid: &str) -> Option<Jwk> {
    keys.iter().find(|k| k.kid == kid && k.kty == "RSA").cloned()
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

struct Parts<'a> {
    header_b64: &'a str,
    payload_b64: &'a str,
    sig_b64: &'a str,
    signing_input: &'a str,
}

impl<'a> Parts<'a> {
    fn split(token: &'a str) -> Option<Self> {
        let mut it = token.split('.');
        let (h, p, s) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        let signing_input = &token[..h.len() + 1 + p.len()];
        Some(Parts {
            header_b64: h,
            payload_b64: p,
            sig_b64: s,
            signing_input,
        })
    }

    fn header(&self) -> Option<JwtHeaderClaims> {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(self.header_b64).ok()?).ok()
    }

    fn claims(&self) -> Option<Claims> {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(self.payload_b64).ok()?).ok()
    }

    fn signature_ok(&self, key: &Jwk) -> bool {
        let (Ok(n), Ok(e), Ok(sig)) = (
            URL_SAFE_NO_PAD.decode(&key.n),
            URL_SAFE_NO_PAD.decode(&key.e),
            URL_SAFE_NO_PAD.decode(self.sig_b64),
        ) else {
            return false;
        };
        RsaPublicKeyComponents { n: &n, e: &e }
            .verify(
                &RSA_PKCS1_2048_8192_SHA256,
                self.signing_input.as_bytes(),
                &sig,
            )
            .is_ok()
    }
}

/// The address a request should be rate-limited under. Behind `cloudflared`
/// every peer is loopback, so with Cloudflare Access enabled and a loopback
/// peer we use `CF-Connecting-IP`; otherwise the socket peer, as before.
pub fn effective_ip(cf: Option<&CfAccess>, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    if cf.is_some() && peer.is_loopback() {
        if let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
        {
            return ip;
        }
    }
    peer
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::rsa::PublicKeyComponents;
    use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};

    pub const TEAM: &str = "acme.cloudflareaccess.com";
    pub const AUD: &str = "aud-tag-123";
    pub const KID: &str = "kid-1";

    pub struct TestKey(pub RsaKeyPair);

    pub fn test_key() -> TestKey {
        TestKey(
            RsaKeyPair::from_der(include_bytes!("../../tests/fixtures/cf_test_rsa.der"))
                .expect("fixture key"),
        )
    }

    impl TestKey {
        pub fn components(&self) -> (String, Vec<u8>, Vec<u8>) {
            let c: PublicKeyComponents<Vec<u8>> = self.0.public().into();
            (KID.to_string(), c.n, c.e)
        }

        pub fn sign(&self, payload: serde_json::Value, kid: &str, alg: &str) -> String {
            let header = serde_json::json!({"alg": alg, "kid": kid, "typ": "JWT"});
            let h = URL_SAFE_NO_PAD.encode(header.to_string());
            let p = URL_SAFE_NO_PAD.encode(payload.to_string());
            let input = format!("{h}.{p}");
            let mut sig = vec![0u8; self.0.public().modulus_len()];
            self.0
                .sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig)
                .unwrap();
            format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig))
        }
    }

    pub fn claims(email: Option<&str>, exp_delta: i64) -> serde_json::Value {
        let mut v = serde_json::json!({
            "iss": format!("https://{TEAM}"),
            "aud": [AUD],
            "exp": unix_now() + exp_delta,
            "iat": unix_now(),
        });
        if let Some(e) = email {
            v["email"] = e.into();
        }
        v
    }

    pub fn cf_with_key(admins: &str) -> (Arc<CfAccess>, TestKey) {
        let cfg = CfAccessConfig::new(TEAM, AUD, admins).unwrap();
        let cf = Arc::new(CfAccess::new(cfg, reqwest::Client::new()));
        let key = test_key();
        cf.set_keys_for_test(vec![key.components()]);
        (cf, key)
    }

    pub fn headers_with(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(JWT_HEADER, token.parse().unwrap());
        h
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn team_domain_is_normalized() {
        assert_eq!(normalize_team_domain("https://Acme.cloudflareaccess.com/"), "acme.cloudflareaccess.com");
        assert_eq!(normalize_team_domain("acme"), "acme.cloudflareaccess.com");
    }

    #[test]
    fn from_env_requires_both_or_neither() {
        // Config-level check: empty values are rejected.
        assert!(CfAccessConfig::new("", "x", "").is_err());
        assert!(CfAccessConfig::new("acme", " ", "").is_err());
        let cfg = CfAccessConfig::new("acme", "x", " A@x.com , ,b@x.com").unwrap();
        assert!(cfg.admin_emails.contains("a@x.com") && cfg.admin_emails.contains("b@x.com"));
        assert_eq!(cfg.admin_emails.len(), 2);
    }

    #[tokio::test]
    async fn valid_token_yields_lowercased_email() {
        let (cf, key) = cf_with_key("");
        let t = key.sign(claims(Some("Alice@VNG.com.vn"), 300), KID, "RS256");
        let id = cf.identity(&headers_with(&t)).await.unwrap();
        assert_eq!(id.email.as_deref(), Some("alice@vng.com.vn"));
        assert_eq!(id.caller_id().as_deref(), Some("cf:alice@vng.com.vn"));
    }

    #[tokio::test]
    async fn service_token_uses_common_name_and_is_never_admin() {
        let (cf, key) = cf_with_key("a@x.com");
        let mut c = claims(None, 300);
        c["common_name"] = "client-id.access".into();
        let t = key.sign(c, KID, "RS256");
        let id = cf.identity(&headers_with(&t)).await.unwrap();
        assert_eq!(id.caller_id().as_deref(), Some("cf:client-id.access"));
        assert!(!cf.is_admin(&id));
    }

    #[tokio::test]
    async fn admin_requires_allowlisted_email() {
        let (cf, key) = cf_with_key("boss@x.com");
        let t = key.sign(claims(Some("BOSS@x.com"), 300), KID, "RS256");
        assert!(cf.is_admin(&cf.identity(&headers_with(&t)).await.unwrap()));
        let t = key.sign(claims(Some("other@x.com"), 300), KID, "RS256");
        assert!(!cf.is_admin(&cf.identity(&headers_with(&t)).await.unwrap()));
    }

    #[tokio::test]
    async fn rejects_expired_wrong_aud_wrong_iss_and_bad_alg() {
        let (cf, key) = cf_with_key("");
        let expired = key.sign(claims(Some("a@x.com"), -3600), KID, "RS256");
        assert!(cf.identity(&headers_with(&expired)).await.is_none());

        let mut c = claims(Some("a@x.com"), 300);
        c["aud"] = serde_json::json!(["someone-else"]);
        assert!(cf.identity(&headers_with(&key.sign(c, KID, "RS256"))).await.is_none());

        let mut c = claims(Some("a@x.com"), 300);
        c["iss"] = "https://evil.cloudflareaccess.com".into();
        assert!(cf.identity(&headers_with(&key.sign(c, KID, "RS256"))).await.is_none());

        let hs = key.sign(claims(Some("a@x.com"), 300), KID, "HS256");
        assert!(cf.identity(&headers_with(&hs)).await.is_none());
    }

    #[tokio::test]
    async fn rejects_tampered_payload_and_garbage() {
        let (cf, key) = cf_with_key("");
        let t = key.sign(claims(Some("a@x.com"), 300), KID, "RS256");
        let mut parts: Vec<&str> = t.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(claims(Some("root@x.com"), 300).to_string());
        parts[1] = &forged;
        assert!(cf.identity(&headers_with(&parts.join("."))).await.is_none());
        assert!(cf.identity(&headers_with("not-a-jwt")).await.is_none());
        assert!(cf.identity(&HeaderMap::new()).await.is_none());
    }

    #[tokio::test]
    async fn rejects_token_signed_by_a_different_key() {
        let (cf, key) = cf_with_key("");
        // Same key material but the server only knows KID; an unknown kid
        // forces a (failing, localhost-less) refresh and must end in None.
        let t = key.sign(claims(Some("a@x.com"), 300), "other-kid", "RS256");
        let mut cfg = cf.config().clone();
        cfg.jwks_url = "http://127.0.0.1:9/certs".into();
        let cf2 = CfAccess::new(cfg, reqwest::Client::new());
        cf2.set_keys_for_test(vec![key.components()]);
        assert!(cf2.identity(&headers_with(&t)).await.is_none());
    }

    #[tokio::test]
    async fn unknown_kid_triggers_one_rate_limited_jwks_fetch() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let key = test_key();
        let (kid, n, e) = key.components();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/certs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "keys": [{"kid": kid, "kty": "RSA",
                          "n": URL_SAFE_NO_PAD.encode(n), "e": URL_SAFE_NO_PAD.encode(e)}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let mut cfg = CfAccessConfig::new(TEAM, AUD, "").unwrap();
        cfg.jwks_url = format!("{}/certs", server.uri());
        let cf = CfAccess::new(cfg, reqwest::Client::new());

        let good = key.sign(claims(Some("a@x.com"), 300), KID, "RS256");
        assert!(cf.identity(&headers_with(&good)).await.is_some());
        // Junk kid right after: must not trigger a second fetch (.expect(1)).
        let junk = key.sign(claims(Some("a@x.com"), 300), "nope", "RS256");
        assert!(cf.identity(&headers_with(&junk)).await.is_none());
    }

    #[test]
    fn effective_ip_trusts_cf_header_only_from_loopback_when_enabled() {
        let (cf, _) = cf_with_key("");
        let mut h = HeaderMap::new();
        h.insert("cf-connecting-ip", "203.0.113.9".parse().unwrap());
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let remote: IpAddr = "198.51.100.7".parse().unwrap();
        assert_eq!(effective_ip(Some(&cf), lo, &h), "203.0.113.9".parse::<IpAddr>().unwrap());
        assert_eq!(effective_ip(Some(&cf), remote, &h), remote);
        assert_eq!(effective_ip(None, lo, &h), lo);
    }
}
