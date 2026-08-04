# Onboarding wizard — manual smoke test

> Extracted verbatim from the "Manual smoke test" section of
> `docs/superpowers/plans/2026-07-26-onboarding-wizard-implementation.md`.
> The `dialoguer` prompt sequences cannot be driven by `cargo test` (they read
> a real terminal). Everything they *call* is already covered by
> unit/integration tests; what needs a human is that the right prompts appear
> in the right order and that the resulting DB rows are correct. Run every
> section below in a **fresh empty directory** so `.router_secret` and the
> SQLite file start absent.

```bash
cargo build --offline
BIN="$PWD/target/debug/1router"
```

## A. `1router setup`, no secret, passthrough provider

- [ ] `cd "$(mktemp -d)"` then `ROUTER_SQLITE_PATH=./t.db "$BIN" setup`
- [ ] Prompt appears: "No admin secret yet. Generate a random one, or enter your own?"
- [ ] Choose **Generate**. A 64-hex-char secret is printed once, plus the
      "will not be printed again" line.
- [ ] `ls -l .router_secret` → exists, `-rw-------` (0600), contents == the
      printed secret, no trailing newline issues (`wc -c` == 64).
- [ ] Prompt: "Add a provider now?" → **yes**.
- [ ] Prompt: "Provider kind" → **passthrough**.
- [ ] Enter name `smoke-openai`; wire format `openai`; base_url
      `https://api.openai.com/v1/chat/completions`; a real API key —
      **confirm the key is masked as you type**; upstream model `gpt-4o-mini`.
- [ ] Prompt: "Pool id" → accept the default (`smoke-openai`).
- [ ] Output confirms `added 'smoke-openai' to pool 'smoke-openai' at priority 1`.
- [ ] Prompt: "Add another provider?" → **no**. The example `curl` is printed
      and the process exits **0** (`echo $?`).
- [ ] `sqlite3 t.db 'select id,name,kind,wire_format,upstream_model from providers; select * from pools; select * from pool_members;'`
      → one passthrough provider, one pool, one member at priority 1.
      **`api_key` is the real key** (it is stored plaintext by design) but was
      never echoed to the terminal.
- [ ] Start the server in the same dir: `ROUTER_SQLITE_PATH=./t.db "$BIN"` —
      it boots **without prompting** (secret comes from the sidecar, providers
      table is non-empty) and logs `1router listening`.
- [ ] A real request works:
      `curl -s localhost:8080/v1/chat/completions -H "Authorization: Bearer $(cat .router_secret)" -H 'content-type: application/json' -d '{"model":"smoke-openai","messages":[{"role":"user","content":"Say OK"}]}'`
      → HTTP 200 with a `choices[0].message.content`.
- [ ] The same request with a wrong bearer → 401.

## B. Re-running `setup` on an already-configured install

- [ ] In the same directory, `ROUTER_SQLITE_PATH=./t.db "$BIN" setup`.
- [ ] It does **not** re-ask for a secret; it prints "reusing ./.router_secret".
- [ ] Prompt reads "This gateway already has providers. Add another one?".
- [ ] Answer **yes**, add a second passthrough provider, and give it the
      **same pool id** as in section A.
- [ ] Output says priority **2** (not 1) — it went behind the incumbent.
- [ ] `sqlite3 t.db 'select * from pool_members order by priority;'` confirms
      1 then 2, and the pool row's `created_at` is unchanged.
- [ ] Answer **no** to "Add another"; exit code 0.

## C. `ROUTER_SHARED_SECRET` wins over the sidecar

- [ ] Still in the same dir: `ROUTER_SQLITE_PATH=./t.db ROUTER_SHARED_SECRET=env-wins "$BIN" setup`
- [ ] It prints "using ROUTER_SHARED_SECRET from the environment" and does not
      touch `.router_secret` (`ls -l` mtime unchanged; contents unchanged).
- [ ] Ctrl-C at the next prompt; exit code is non-zero.

## D. Corrupt sidecar is fatal, not silently replaced

- [ ] `chmod 000 .router_secret` then `ROUTER_SQLITE_PATH=./t.db "$BIN"`
      (as a non-root user).
- [ ] Startup **fails** with a "failed to read secret file" error naming the
      path. Exit code non-zero.
- [ ] `.router_secret` still holds the original secret (nothing regenerated).
- [ ] `chmod 600 .router_secret` restores normal boot.
- [ ] `printf '' > .router_secret` → startup fails with the "is empty" message.
      Restore the real secret afterwards.

## E. First-boot auto-trigger with a TTY

- [ ] `cd "$(mktemp -d)"` (fresh) then `ROUTER_SQLITE_PATH=./t.db "$BIN"` —
      **no `setup` argument**.
- [ ] The wizard runs automatically (empty DB + no seed path + TTY), starting
      with the secret prompt.
- [ ] Complete it with one passthrough provider; after "Add another? → no",
      the process **continues into normal startup** and logs
      `1router listening` (it does not exit).
- [ ] Ctrl-C to stop the server.
- [ ] Repeat `ROUTER_SQLITE_PATH=./t.db "$BIN"` in the same dir: **no wizard**
      (providers table non-empty), straight to listening.

## F. First-boot auto-trigger is suppressed by `ROUTER_SEED_PATH`

- [ ] `cd "$(mktemp -d)"` (fresh). Write a minimal seed file:
      `echo '{"providers":[{"id":"seeded","name":"seeded","wire_format":"openai","kind":"passthrough","base_url":"https://x/v1/chat/completions","api_key":"k","upstream_model":"m","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}],"pools":[],"members":[]}' > seed.json`
- [ ] `ROUTER_SQLITE_PATH=./t.db ROUTER_SEED_PATH=./seed.json ROUTER_SHARED_SECRET=x "$BIN"`
- [ ] **No wizard prompt at all**, even though stdin is a TTY. Logs
      `first-boot seed applied` then `1router listening`.
- [ ] Same again but **without** `ROUTER_SHARED_SECRET` in a fresh dir: still
      no wizard; a secret is generated, logged once, and written to
      `.router_secret`.

## G. No-TTY paths never block

- [ ] `cd "$(mktemp -d)"` then `ROUTER_SQLITE_PATH=./t.db "$BIN" < /dev/null`
- [ ] No prompt. One `info` log line with the generated secret and the
      "SAVE THIS NOW" wording. `.router_secret` created at 0600. Server reaches
      `1router listening` with an empty provider set.
- [ ] `curl -s localhost:8080/health` → 200 (health is unauthenticated).
- [ ] `curl -s localhost:8080/v1/models -H "Authorization: Bearer $(cat .router_secret)"`
      → 200 with an empty list, proving the logged secret is the live one.
- [ ] Ctrl-C. Then `ROUTER_SQLITE_PATH=./t.db "$BIN" setup < /dev/null`
      → prints the "needs a terminal on stdin" message to **stderr** and exits
      with status **2**. It does **not** hang.

## H. Codex OAuth provider (needs a real ChatGPT account + a browser)

- [ ] `cd "$(mktemp -d)"` then `ROUTER_SQLITE_PATH=./t.db "$BIN" setup`;
      generate a secret; add a provider; choose **Codex OAuth**.
- [ ] Enter name `smoke-codex`. The authorize URL is printed with the
      three-step instructions.
- [ ] **Paste garbage** at the prompt first → it reports "could not find both
      `code` and `state`" and **re-prompts** (the wizard does not abort and does
      not make you redo the authorize step).
- [ ] Open the URL, log in, copy the `localhost:1455/auth/callback?...` URL
      from the address bar (it will fail to load — expected) and paste it.
- [ ] "login stored." then "Probing which model this ChatGPT account
      accepts..." with one `trying "<model>"` line per candidate, in the order
      `gpt-5.4, gpt-5-codex, gpt-5.1-codex, gpt-5, codex-mini-latest`,
      **stopping at the first success**.
- [ ] `-> using upstream_model "<model>"` is printed and
      `sqlite3 t.db 'select upstream_model from providers;'` matches it (not
      `pending`).
- [ ] Assign it to pool `smoke-codex`; finish the wizard.
- [ ] `sqlite3 t.db 'select provider_id, access_token is not null, refresh_token is not null, pkce_verifier is null, oauth_state is null from provider_oauth_state;'`
      → tokens present, PKCE columns cleared.
- [ ] Start the server and send a real chat completion against pool
      `smoke-codex` → HTTP 200 with content. This is the end-to-end proof.
- [ ] Also paste an **already-used** code on a second run to confirm the
      exchange failure re-prompts in place rather than aborting.

## I. Model-probe total failure is not fatal

Hard to force naturally; simulate it by temporarily editing
`CANDIDATE_MODELS` to a single bogus value (`["definitely-not-a-model"]`),
rebuilding, and re-running section H.

- [ ] Every attempt's status + body is printed.
- [ ] The wizard **continues** to the pool prompt (it does not abort).
- [ ] `upstream_model` stays `pending` in the DB, and the printed hint shows
      the exact `PATCH /admin/providers/smoke-codex` curl to fix it.
- [ ] Revert the `CANDIDATE_MODELS` edit and rebuild before committing
      anything. **Do not commit the bogus list.**

## J. Command Code provider (browser callback or paste fallback)

- [ ] In a fresh setup, choose **Command Code (commandcode.ai browser login)**.
- [ ] Select the client wire format; confirm the printed studio URL contains
      the encoded `http://localhost:<port>/callback` and state token.
- [ ] Complete the browser login and confirm the callback stores the key in
      `provider_oauth_state` as both access and refresh tokens while
      `providers.api_key` remains `NULL`.
- [ ] If the callback listener cannot bind or times out, confirm the wizard
      prints the URL and reaches the hidden paste-key prompt without hanging.
- [ ] Confirm model discovery offers the unauthenticated Command Code model
      list, and that the selected model is persisted as `upstream_model`.
- [ ] Confirm both `/v1/chat/completions` and `/v1/messages` work through the
      resulting pool; the admin UI should use its separate paste-key flow.

---

## Final verification checklist

```bash
cargo build --offline --release
cargo test --offline                     # all unit + integration; e2e stay ignored
cargo clippy --offline --all-targets -- -D warnings
```

- [ ] All of the above green.
- [ ] `git diff --stat` touches only: `Cargo.toml`, `Cargo.lock`,
      `src/onboarding.rs`, `src/lib.rs`, `src/core/config.rs`, `src/main.rs`,
      `src/providers/oauth_routes.rs`, `README.md`, `CLAUDE.md`, and the two
      docs files. **Anything else means scope creep — justify or revert it.**
- [ ] `grep -rn "shared_secret\|ROUTER_SHARED_SECRET" src/` shows no new
      logging of the secret value other than the one deliberate no-TTY
      bootstrap `info!` in `main.rs`.
- [ ] The manual smoke checklist above is fully ticked, including section H
      against a real ChatGPT account.
- [ ] `CANDIDATE_MODELS` in `src/onboarding.rs` is identical to
      `candidate_models` in `tests/e2e_real_providers.rs` (the unit test
      `candidate_list_matches_the_e2e_test` pins one side; eyeball the other).
- [ ] Existing behaviour is unchanged for anyone who sets
      `ROUTER_SHARED_SECRET`: `tests/startup.rs`, `tests/codex_oauth.rs` and
      the admin tests all pass untouched.
