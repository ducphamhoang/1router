# Security audit — 2026-09-23

Whole-repo, read-only code audit of `master` @ `675cf06` (after the users /
per-caller credentials merge), split into four areas: auth/session/admin,
proxy request path, OAuth/secrets/logging, frontend/deps/deployment. Every
finding below was checked against the code; **SEC-01 was also reproduced
live** on a throwaway instance (fresh DB, fake upstream). Nothing was run
against the real gateway beyond read-only checks.

Out of scope here, tracked separately as a known issue: the live deployment
itself (default admin password, `0.0.0.0` bind, open access, plain HTTP) —
see [BACKLOG BL-06](BACKLOG.md#bl-06-live-deployment-is-exposed-default-admin-password-0000-open-access-plain-http).

Attacker positions used below: **/v1 caller** (anyone holding a user key —
or anyone at all while open access is on), **admin**, **upstream** (a
malicious or compromised provider endpoint the admin configured), **local**
(another OS user / process on the host).

## Remediation status

| ID | Status |
|---|---|
| SEC-01 | **Fixed** (`fix/sec-01-02`): 400/413/422 no longer touch runtime state; re-verified live |
| SEC-02 | **Fixed** (`fix/sec-01-02`): cap enforced mid-stream via `http_body_util::Limited`; 200 MB chunked body cut off at ~12 MB, gateway RSS 13 MB |
| BL-01 | **Fixed** alongside SEC-01: `Misconfigured` now re-probes after 5 min instead of being permanent |
| others | Open — see BL-07 for order |

| ID | Sev | Title | Attacker |
|---|---|---|---|
| SEC-01 | High | One bad request permanently disables a pool member | /v1 caller |
| SEC-02 | High | Body size limit checked only after buffering the whole body | /v1 caller |
| SEC-03 | Medium | Runtime-state map grows without bound via direct addressing | /v1 caller |
| SEC-04 | Medium | Shared secret is both a /v1 key and a full admin credential | whoever is given it |
| SEC-05 | Medium | Login rate limit is racy; argon2 blocks async workers | unauthenticated |
| SEC-06 | Medium | Direct addressing uses any model on any provider's key | /v1 caller |
| SEC-07 | Medium | All upstream response headers are relayed to callers | upstream → caller |
| SEC-08 | Medium | No security headers (clickjacking, no CSP) | same-site web page |
| SEC-09 | Medium | Docker: runs as root, `.dockerignore` misses secrets | build / container |
| SEC-10 | Low | Redirects followed; `x-api-key` survives cross-host redirect | upstream |
| SEC-11 | Low | Error bodies / debug headers leak internal URLs and topology | /v1 caller |
| SEC-12 | Low | Secrets and internals in logs and error responses | log reader |
| SEC-13 | Low | Secret file / DB / dataset logs not permission-restricted | local |
| SEC-14 | Low | No shared-secret strength rule, no Bearer failure throttling | network |
| SEC-15 | Low | Crafted `reasoning` field panics the codex transform | /v1 caller |
| SEC-16 | Low | Upstream responses buffered without limit, no total deadline | upstream |
| SEC-17 | Low | Command Code browser-login listener can be aborted by any page | local / web page |
| SEC-18 | Low | Empty Command Code key pulls the host user's own credential | admin |
| SEC-19 | Low | Login-attempts map never shrinks | unauthenticated |
| SEC-20 | Info | Hardening nits (see list) | — |

---

## SEC-01 — High — One bad request permanently disables a pool member

**Where:** `src/proxy/backoff.rs:16` (400 → `NonRetryable`),
`src/proxy/flow.rs:265-268` (`mark_misconfigured()`),
`src/core/runtime.rs` (`Misconfigured` is unavailable forever).

**Scenario (reproduced):** user `mallory` sends one request the upstream
rejects with 400 (`{"model":"shared","messages":"x"}` — or just an
over-long context). The pool member is marked misconfigured; every later
request from every user gets `503 no provider produced a response` until an
admin validates the provider or restarts the daemon. N bad requests take out
an N-member pool. Ordinary users hitting context-length errors trigger it by
accident. Same root cause as BL-01, but this is a client-triggerable path.

**Fix:** a client-caused 4xx (400/404/413/422) is a per-request failure with
no runtime-state change — relay it to the caller and stop failover. Keep
`Misconfigured` for real credential/config failures, and give it a TTL
(BL-01).

## SEC-02 — High — Body size limit checked only after buffering

**Where:** `src/proxy/body.rs:6-17` — `body.collect().await` then
`len > cap`. Handlers take a raw `Body` (`src/proxy/routes.rs`), which
bypasses `DefaultBodyLimit`; there is no `RequestBodyLimitLayer`.

**Scenario:** a chunked POST of arbitrary size to `/v1/chat/completions` is
held in RAM in full before being rejected; a few concurrent ones OOM the
process. No read deadline either, so slow senders tie up connections.

**Fix:** `http_body_util::Limited::new(body, cap)` inside `buffer_body` (or
`tower_http::limit::RequestBodyLimitLayer` on the proxy router), plus a
request read timeout.

## SEC-03 — Medium — Runtime-state map grows without bound

**Where:** `src/proxy/flow.rs:146` —
`state.runtime.entry(runtime_key(&provider.id, effective_model)).or_default()`
runs before any network call; with `<provider_id>/<model>` addressing
(`src/pools/select.rs`) `effective_model` is the caller's raw string.

**Scenario:** each distinct `"<provider_id>/<long random string>"` inserts a
never-evicted entry (key up to the body limit in size). Provider ids are
discoverable via `/v1/models` and `x-1router-tried`.

**Fix:** read with `get` and only insert on an actual outcome; cap model
length/charset; evict non-pool keys.

## SEC-04 — Medium — Shared secret is both a /v1 key and a full admin credential

**Where:** `src/auth/middleware.rs` (`require_bearer` and
`require_admin_session` both accept it), `src/onboarding.rs:1081` ("Use it
as `Authorization: Bearer …` on /v1/* and /admin/*"), `GET /admin/export`
(`src/admin/mod.rs`) returns every provider's plaintext `api_key`.

**Scenario:** an operator hands the shared secret to a teammate or SDK "for
API access" (as onboarding suggests). The holder can `GET /admin/export`
(all upstream keys), `PATCH` a provider's `base_url` to their own host
(next proxied request delivers the stored key), or mint users. The
`***tail` masking in provider responses is cosmetic against this.

**Fix:** now that per-user keys exist, stop recommending the shared secret
for `/v1/*` (onboarding text, README); optionally stop accepting it on
`/v1/*` at all, or split it into separate admin and client secrets.

## SEC-05 — Medium — Login rate limit is racy; argon2 blocks async workers

**Where:** `src/admin/auth/routes.rs:40` (lockout check) → `:49` (awaited DB
read) → `:61` (argon2) → `:66` (`record_failure`);
`src/admin/auth/password.rs` (`verify_password` is synchronous).

**Scenario:** concurrent `POST /admin/auth/login` requests all pass the
lockout check before the first failure is recorded, so the 5-per-5-minutes
limit doesn't bound a burst; throughput is limited only by argon2 CPU.
The same flood, needing no credentials, keeps Tokio workers busy with
argon2 and stalls `/v1/*` traffic.

**Fix:** reserve the attempt atomically before verifying; run argon2 in
`spawn_blocking` behind a small semaphore; add a global limiter; group IPv6
by /64.

## SEC-06 — Medium — Direct addressing uses any model on any provider's key

**Where:** `src/pools/select.rs` (`<provider_id>/<model>` resolution).

**Scenario:** any /v1 caller can send `<provider_id>/<most-expensive-model>`
and bill it to the admin's key, bypassing the pool curation the admin set
up. There is no per-user restriction (out of scope for users v1).

**Fix:** allow only models in `discovered_models` or pool members, or make
direct addressing opt-in per provider / per user.

## SEC-07 — Medium — All upstream response headers are relayed

**Where:** `src/providers/adapter/http.rs:119-127` copies every header except
`transfer-encoding`/`content-length`.

**Scenario:** callers receive `set-cookie` (e.g. Cloudflare), the admin's
`openai-organization` / `openai-project` / `anthropic-organization-id`, and
hop-by-hop headers (`connection`, `keep-alive`).

**Fix:** forward an allowlist (content-type, request id, rate-limit headers,
`retry-after`).

## SEC-08 — Medium — No security headers

**Where:** `src/ui_assets.rs` sets only `Content-Type`; nothing anywhere sets
CSP, `X-Frame-Options`, `nosniff`, `Referrer-Policy` or `Cache-Control`.

**Scenario:** `SameSite=Strict` ignores the port, so another web app on the
same host/IP (different port) can iframe `/ui/*` with the admin's session
and clickjack "reveal secret", auth-mode toggles, deletes or key rotation.
No CSP backstop against any future XSS.

**Fix:** one `SetResponseHeader` layer: `Content-Security-Policy:
default-src 'self'; frame-ancestors 'none'; object-src 'none'; base-uri
'none'`, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`,
`Referrer-Policy: no-referrer`; `Cache-Control: no-store` on `/admin/*`,
`no-cache` on `index.html`.

## SEC-09 — Medium — Docker image runs as root; `.dockerignore` gaps

**Where:** `Dockerfile:36` (`distroless/static-debian12`, no `USER`,
default DB path `/`), `.dockerignore` (only `target`, `.git`, `*.db*`),
`Dockerfile:11` (`COPY frontend/ frontend/` over the in-image `npm ci`).

**Scenario:** `.router_secret`, `.env`, `dataset-logs/` (full prompts),
`*.log` go into the build context (daemon, remote builders, CI cache); a
process compromise is root in the container; the host's `node_modules` can
overwrite the image's.

**Fix:** `:nonroot` base + `USER 65532` + `/data` volume
(`ROUTER_SQLITE_PATH=/data/1router.db`); extend `.dockerignore` with
`.router_secret`, `.env`, `dataset-logs/`, `*.log`, `frontend/node_modules`,
`frontend/dist`, `.claude`, `.superpowers`, `.commandcode`, `graphify-out`.

## SEC-10 — Low — Redirects followed; `x-api-key` survives cross-host

**Where:** `src/core/http_client.rs` sets no redirect policy (reqwest follows
up to 10, stripping only `Authorization`/`Cookie` cross-origin).

**Scenario:** an upstream returns `307` to another host and receives the
Anthropic `x-api-key` / `ChatGPT-Account-ID`; or redirects to an internal
address whose 2xx body is relayed to the caller.

**Fix:** `.redirect(reqwest::redirect::Policy::none())`.

## SEC-11 — Low — Errors and debug headers leak internals

**Where:** `src/proxy/flow.rs` (`last_error_body` from reqwest errors,
`wire_error`, `insert_debug_headers`).

**Scenario:** /v1 callers see full upstream URLs (reqwest's Display includes
them, query string and internal IPs included) and provider ids/topology via
`x-1router-tried` / `x-1router-provider`.

**Fix:** generic messages to non-admin callers, details to the log; debug
headers only for the admin caller.

## SEC-12 — Low — Secrets and internals in logs / responses

- `src/main.rs:94-99`: headless first boot logs the generated admin secret at
  info level (JSON stdout → container logs / aggregators).
- `src/admin/auth/routes.rs:67`: failed login logs the raw `username` (often
  a password typed into the wrong field).
- `src/core/error.rs`: `AppError::Db` returns raw sqlx text to clients,
  including on the public login route.
- `src/telemetry/logging.rs`: `redact()` exists but is never called.

**Fix:** log only the sidecar path; drop/hash the username; generic 500
body; wire `redact()` in or delete it.

## SEC-13 — Low — Secret file / DB / dataset logs not permission-restricted

- `src/core/config.rs:180-196`: `.mode(0o600)` applies only on create, so
  rotating the secret never tightens an existing file; on Windows (this
  host) it's a plain `fs::write` with inherited ACLs while onboarding prints
  "(mode 0600)".
- `src/core/db.rs`, `src/telemetry/dataset_log.rs`: DB (plaintext provider
  keys, OAuth tokens) and dataset JSONL (full prompts) use default umask /
  ACLs.

**Fix:** set permissions after open; restrictive ACL on Windows or drop the
claim; umask 077 / explicit modes for DB, WAL and dataset paths.

## SEC-14 — Low — No secret strength rule, no Bearer throttling

**Where:** `src/admin/settings.rs` (any non-empty secret accepted);
`require_bearer` / `require_admin_session` Bearer paths have no failure
limit; comparisons use `==` (not realistically exploitable remotely).

**Fix:** minimum 32 chars (or reject low entropy); throttle failed Bearer
attempts; `subtle::ConstantTimeEq`.

## SEC-15 — Low — Crafted `reasoning` panics the codex transform

**Where:** `src/providers/adapter/codex/transform.rs:227-228` —
`obj.entry("reasoning").or_insert(..)` then `reasoning["effort"] = …`
panics when the client sent `"reasoning": "x"`.

**Scenario:** OpenAI-wire request to a codex-backed pool → task panics,
connection dropped (process survives; no `panic = "abort"`).

**Fix:** replace a non-object `reasoning` with `{}` before indexing.

## SEC-16 — Low — Unbounded upstream buffering

**Where:** SSE framers (`codex/transform.rs:472-486`,
`codex/claude_bridge.rs:779-794`) append until `"\n\n"`; `.text()`/`.bytes()`
on error/aggregation paths (`flow.rs`, `http.rs`, `codex/adapter.rs`);
dataset tee accumulator. Only an idle `read_timeout`, no total deadline.
Also: `\r\n\r\n`-framed SSE never splits, and per-chunk `from_utf8_lossy`
corrupts multi-byte characters split across chunks.

**Fix:** cap buffers (16–64 MB), handle CRLF framing, decode UTF-8
incrementally, total deadline on non-stream paths.

## SEC-17 — Low — Command Code browser-login listener can be aborted

**Where:** `src/providers/adapter/commandcode/browser_login.rs` (first
well-formed POST ends the flow; `"error"` checked before `state`; body
parsed as JSON regardless of content type; one connection at a time).

**Scenario:** any web page the admin has open sends a `text/plain` POST to
the loopback port and cancels the login (DoS only — `state` is a UUID, so no
key injection).

**Fix:** validate `state` first, ignore non-matching requests, require
`application/json`, per-connection read timeout.

## SEC-18 — Low — Empty Command Code key pulls the host user's credential

**Where:** `src/providers/oauth_routes.rs:144-149` →
`commandcode/api_key.rs::commandcode_key_from_disk` (env vars and
`~/.commandcode|.pi|.omp` auth files).

**Scenario:** a remote admin who isn't the host's OS user gets that user's
Command Code account wired into the router.

**Fix:** only allow this from the local CLI wizard, or behind an explicit
opt-in.

## SEC-19 — Low — Login-attempts map never shrinks

**Where:** `src/admin/auth/rate_limit.rs` — entries are only reset, never
removed.

**Fix:** sweep stale entries in the existing cleanup task.

## SEC-20 — Info — Hardening nits

- Username check leaks timing (argon2 only runs on a matching username);
  one admin named `admin`, so low value.
- Session renewal extends the server-side expiry but never re-sends the
  cookie (browser drops it after 24h anyway) — functional bug, not security.
- `frontend/src/pages/Providers.tsx:632-635`: provider `api_key` input is
  plain text (no `type="password"` / `autoComplete="off"`).
- `.gitignore`: add `cookies*.txt`, `*backup*.json`, `**/.env`, `*.pem`.
- `esbuild` 0.21.5 via vite 5 (GHSA-67mh-4wv8-2f99) — dev server only.
- Command Code browser-login timeout (15s) is likely too short for a real
  login from the admin UI — functional.

## Checked and fine

Route coverage (every route behind the right guard; only `/admin/auth/login`,
`/health`, `/ui/*` public); user keys and open access can't reach
`/admin/*`; cookie+Bearer combinations don't bypass CSRF; no CORS on the main
app; session tokens 256-bit, hashed, rotated at login, invalidated on
logout/password change; cookie `HttpOnly` + `SameSite=Strict` (+ `__Host-` /
`Secure` behind HTTPS); `X-Forwarded-For` never trusted; Codex PKCE (S256,
UUID state, cleared after use, no public callback); Command Code listener
bound to 127.0.0.1 with an origin allowlist; refresh-token locking; import
can't change auth mode or touch OAuth state; dataset-log `provider_id` path
sanitised, no headers captured; provider responses mask `api_key`;
upstream URLs never built from client input; `model` only inserted as a JSON
string; client headers not forwarded upstream; debug headers CR/LF-safe;
rustls with no invalid-cert override; no XSS sinks in the UI, no
browser-storage of secrets, every mutating call carries the CSRF header;
`ui_assets` has no path traversal; no known advisories in locked Rust crates;
no secrets committed to git; `build.rs` runs fixed commands only.
