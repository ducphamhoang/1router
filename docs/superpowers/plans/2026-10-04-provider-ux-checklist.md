# Provider create/edit UX — fix checklist

Source: UX review of `frontend/src/pages/Providers.tsx` + `CodexOAuthPanel.tsx`, verified against the backend by an Opus reviewer.
Rule: tick `[x]` right after a task is done AND verified (`cargo test --offline`, `npm test` in `frontend/`). Log notes under "Work state".

Decisions: `base_url` is the FULL endpoint (adapter POSTs it as-is) — never auto-append `/v1`. Bearer-auth admin requests stay CSRF-exempt (see CLAUDE.md). Overlay / focus trap / Esc are deferred.

## P0 — Backend correctness
- [x] 1. Reset runtime state (`runtime.remove(id)`) on PATCH, delete, OAuth complete, successful test (Misconfigured is otherwise permanent)
- [x] 2. Server validation: non-empty/trimmed id (safe charset), name, upstream_model; normalize `""` -> null for base_url/api_key
- [x] 3. Distinguish duplicate id vs duplicate name error message
- [x] 4. (REVERTED after rebase onto v0.4.2: Codex supports both client wire formats, the UI Codex template uses `anthropic`)
- [x] 5. `mask`: never reveal short keys (<= 8 chars)
- [x] 6. Provider selection skips providers that are not `ready` (passthrough w/o base_url; codex w/o token)

## P1 — Create flow & credentials
- [x] 7. `ready` / `credential_status` in provider JSON; UI shows "Needs credentials" instead of default "healthy"
- [x] 8. Credentials optional on create; after creating codex provider switch to edit mode with OAuth panel ("Connect now" / "Later")
- [x] 9. API key UX: masked value shown, `type=password`, Replace / Remove key; `DELETE /admin/providers/:id/oauth` to disconnect codex
- [x] 10. Form adapts to kind (oauth_codex hides endpoint/key/wire format; friendly labels)
- [x] 11. Edit mode: id / kind / wire format read-only with "recreate to change"
- [x] 12. Client validation: required, http(s) scheme, per-field errors (`aria-invalid`, `aria-describedby`); `encodeURIComponent` ids

## P2 — Endpoint URL & verification
- [x] 13. Rename to "Endpoint URL", presets per wire format, placeholders; normalize whitespace + trailing `/` only
- [x] 14. Warn (with one-click fix) if URL doesn't end in `/chat/completions` (openai) or `/messages` (anthropic), or contains `/v1/v1`
- [x] 15. Real probe replacing `test_stub` (classify 2xx / 401-403 / 404-405 / 400 model / connect error); draft-test endpoint; never send stored key to a different URL; 10s timeout, no redirects, truncated body
- [x] 16. Hints/placeholders for Name & Upstream model; auto-slug id from name

## P3 — Form & table polish
- [x] 17. Cancel button, scroll+focus on open, clear stale error, `saving` flag (no double-submit), loading state
- [x] 18. Delete: confirm dialog listing affected pools, error handling, `--danger` styling
- [x] 19. Table: endpoint, key status, OAuth status, state badge, `unavailable_in_secs`, empty/loading/error states
- [x] 20. Success toast; widen form max-width

## P4 — Codex OAuth panel
- [x] 21. Single "Paste redirect URL" field (parse code+state), keep two separate fields as fallback
- [x] 22. try/catch on start, show `authorize_url` as link (popup blocker), note that Start twice invalidates the previous flow
- [x] 23. Refresh providers/state after successful connect

## Final
- [x] 24. Full test run (`cargo test --offline`, frontend tests)
- [x] 25. Opus specialist review of the diff; fix Critical/Important findings

## Work state
(started 2026-10-04, branch `feat/provider-ux`; no commits yet - user hasn't asked)
- Build note: /home is full, so build with `CARGO_TARGET_DIR=<scratchpad>/target cargo test --offline --no-default-features`.
- P0 (1-6) DONE + tests. Extra found: serde maps `null` to outer None for Option<Option<T>>, so `api_key: null` never cleared - fixed via `double_option` deserializer.
- Backend halves DONE: 7 (`ready`/`credential_status` in JSON), 9 (`DELETE /admin/providers/:id/oauth`, null-clear), 15 (`classify_probe`, `/admin/providers/:id/test` real probe, `POST /admin/provider-test` draft). UI halves still open.
- Suite: 149 lib + integration tests green (backend).
- P1-P4 (7-23) DONE in `Providers.tsx` / `CodexOAuthPanel.tsx` / `styles.css`; frontend 31 vitest tests green, `tsc --noEmit` clean.
- Deviations: state polling is still one request per provider (no bulk endpoint; unready providers are skipped). Overlay/focus-trap/Esc deferred as agreed. OAuth panel is rendered OUTSIDE the provider <form> (nested forms are invalid HTML).
- Disk: ran `cargo clean` (user OK'd) because /home was full.
- Opus review fixes DONE: draft probe moved to `/admin/provider-test` (a provider with id `test` stays addressable, tested); onboarding + import now normalize (`id_from_name`, `clean_opt`, `clean_endpoint`, `normalize_new`); probe omits `max_tokens` for openai wire format and 400 message softened; PATCH ignores base_url/api_key for oauth_codex; runtime reset only on a real 2xx probe; UI: `key={editing.id}` on OAuth panel, session token drops stale async results, delete double-click guard.
- Final run: 150 lib + all integration tests green; vitest 31 passed; `tsc --noEmit` clean.
- Leftovers closed: added integration tests (`disconnect_oauth_really_removes_tokens`, `unready_member_is_skipped_and_next_member_serves`). `x-1router-tried` deliberately still lists only providers actually attempted (skipped unready ones are not "tried").
- Real browser e2e (Playwright + system Chrome against the built `1router` binary with embedded UI, local mock upstream): login, create + Test connection (ok / 401 auth failure), key-less create, key masked in DOM, Remove key, Codex create + OAuth panel bad-redirect error + "Needs setup" badge, delete with confirm - all 9 steps pass. Script lives in the session scratchpad (not committed). Only network error seen: the expected 401 on the first /admin/providers call before login.
- CLI: added `--version`/`-V`, `--help`/`-h`/`help`; unknown arguments exit 2 instead of booting a server (`tests/cli.rs`).
- REBASE NOTE (v0.4.3): the work was first built on a stale base (120 commits behind v0.4.2). It was re-applied on top of origin/master: backend merged by hand (kept upstream reasoning-effort checks, `credential_configured`, `OauthCommandCode`, `reset_provider_to_healthy`); the frontend kept upstream's modal form/templates/Validate/Codex panel and ports only the deltas: Needs-setup badge + Credentials column, endpoint-suffix warning with one-click fix, client-side validation, Remove key (`api_key: null`), read-only kind/API format on edit, delete confirmation listing affected pools, save guard + notices, Disconnect account. The `oauth_codex` + `anthropic` rejection (item 4) was dropped because the Codex template uses `anthropic`. `/admin/provider-test` and `/admin/providers/:id/test` stay as backend endpoints (no UI caller; the modal uses upstream's Validate).
- Final: 572 cargo tests, 111 vitest, tsc clean; headless-Chrome e2e 9/9 against the merged release binary.
