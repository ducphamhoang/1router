# OpenCode Preset Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add "OpenCode (OpenAI-compatible)" and "OpenCode (Anthropic-compatible)" as two new preset entries in the existing `PROVIDER_TEMPLATES` dropdown (CLI wizard + admin UI), pointing at OpenCode Go's `https://opencode.ai/zen/go/v1/{chat/completions,messages}` endpoints. Config-only — no new `ProviderKind`, no adapter change, no migration.

**Architecture:** Same shape as the existing DeepSeek dual-template precedent: `PassthroughAdapter` already speaks both wire formats these endpoints use, and `derive_models_url` already generalizes to fetch `/models` off either. The only surface touched is the two mirrored `PROVIDER_TEMPLATES` arrays, the frontend's static model-suggestion lists, and docs.

**Tech Stack:** No new dependency. Rust (`src/onboarding.rs`), TypeScript/React (`frontend/src/pages/Providers.tsx`, `frontend/src/pages/Pools.tsx`), Markdown docs.

## Global Constraints

- Package is `router`, binary is `1router`. Build/test with `cargo build --offline` / `cargo test --offline`. Frontend tests: `npm --prefix frontend test` (or the repo's existing frontend test command — check `package.json` scripts if unsure).
- **No migration, no new `ProviderKind`, no adapter change.** If a task starts reaching for `src/providers/adapter/http.rs` or `src/core/model.rs`, stop — that means the "OpenCode Free" no-auth tier is being pulled in scope, which the design spec (`docs/superpowers/specs/2026-08-05-opencode-preset-provider-design.md`, "Out of scope") explicitly defers.
- Fixed upstream constants, used verbatim:
  - OpenAI-compatible endpoint: `https://opencode.ai/zen/go/v1/chat/completions`
  - Anthropic-compatible endpoint: `https://opencode.ai/zen/go/v1/messages`
  - Default upstream models: `kimi-k2.7-code` (OpenAI-compatible template), `qwen3.7-max` (Anthropic-compatible template)
- The two `PROVIDER_TEMPLATES` arrays (`src/onboarding.rs`, `frontend/src/pages/Providers.tsx`) carry an existing "mirrors ... keep the two in sync" comment on both sides — both must grow together, in the same task, or the CLI wizard and admin UI dropdowns diverge.
- `frontend/src/pages/Pools.tsx`'s `OPENAI_MODEL_SUGGESTIONS` / `ANTHROPIC_MODEL_SUGGESTIONS` are suggestions only (a `<datalist>`, free-text still accepted) — do not build a validated enum, do not dedupe against upstream reality beyond what's listed here.
- Model discovery (`derive_models_url` + `fetch_live_models` in `src/providers/routes.rs`) is **not modified** — the design spec's bet is that it already works unmodified for both new base URLs. Task 3 includes a manual smoke check of this; if it's wrong, that's a bug report against `derive_models_url`/OpenCode's `/models` endpoint, not a reason to add bespoke discovery code to this plan.
- axum/route-syntax concerns don't apply here — no new route is added.

---

### Task 1: `PROVIDER_TEMPLATES` in `src/onboarding.rs`

**Files:**
- Modify: `src/onboarding.rs` (`PROVIDER_TEMPLATES` array ~line 170, its `[ProviderTemplate; 4]` size annotation)
- Test: `cargo test --offline --lib onboarding`

**Interfaces:**
- Consumes: nothing.
- Produces: two new `ProviderTemplate` entries, selectable in the wizard's `Select` at `add_passthrough_provider` (~line 241). Task 3 reads their labels/URLs/models back out in a smoke check.

- [ ] **Step 1: Write the failing test**

`ProviderTemplate`'s fields are all private to the module and the struct
has no derives beyond what's needed to build it, so add a same-module unit
test in `src/onboarding.rs`'s existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn provider_templates_include_both_opencode_entries() {
    let labels: Vec<&str> = PROVIDER_TEMPLATES.iter().map(|p| p.label).collect();
    assert!(labels.contains(&"OpenCode (OpenAI-compatible)"));
    assert!(labels.contains(&"OpenCode (Anthropic-compatible)"));

    let openai_tmpl = PROVIDER_TEMPLATES
        .iter()
        .find(|p| p.label == "OpenCode (OpenAI-compatible)")
        .unwrap();
    assert_eq!(openai_tmpl.wire_format, WireFormat::OpenAi);
    assert_eq!(openai_tmpl.base_url, "https://opencode.ai/zen/go/v1/chat/completions");
    assert_eq!(openai_tmpl.upstream_model, "kimi-k2.7-code");

    let anthropic_tmpl = PROVIDER_TEMPLATES
        .iter()
        .find(|p| p.label == "OpenCode (Anthropic-compatible)")
        .unwrap();
    assert_eq!(anthropic_tmpl.wire_format, WireFormat::Anthropic);
    assert_eq!(anthropic_tmpl.base_url, "https://opencode.ai/zen/go/v1/messages");
    assert_eq!(anthropic_tmpl.upstream_model, "qwen3.7-max");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --offline --lib onboarding::tests::provider_templates_include_both_opencode_entries`
Expected: FAIL — the two labels aren't in the array yet (and/or the array-size mismatch `[ProviderTemplate; 4]` vs. 6 elements fails to compile once Step 3's entries are added without bumping the count, so this order also catches that).

- [ ] **Step 3: Add the two entries**

Bump `const PROVIDER_TEMPLATES: [ProviderTemplate; 4]` to `[ProviderTemplate; 6]` and append:

```rust
ProviderTemplate {
    label: "OpenCode (OpenAI-compatible)",
    wire_format: WireFormat::OpenAi,
    base_url: "https://opencode.ai/zen/go/v1/chat/completions",
    upstream_model: "kimi-k2.7-code",
},
ProviderTemplate {
    label: "OpenCode (Anthropic-compatible)",
    wire_format: WireFormat::Anthropic,
    base_url: "https://opencode.ai/zen/go/v1/messages",
    upstream_model: "qwen3.7-max",
},
```

Place them directly after the two DeepSeek entries — the array order is
the dropdown order the operator sees.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --offline --lib onboarding` — PASS, including the new test and everything pre-existing.

- [ ] **Step 5: Commit**

```bash
git add src/onboarding.rs
git commit -m "feat(onboarding): OpenCode preset templates (OpenAI- and Anthropic-compatible)

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 2: Frontend `PROVIDER_TEMPLATES` + model suggestions

**Files:**
- Modify: `frontend/src/pages/Providers.tsx` (`PROVIDER_TEMPLATES` array ~line 67)
- Modify: `frontend/src/pages/Pools.tsx` (`OPENAI_MODEL_SUGGESTIONS` ~line 45, `ANTHROPIC_MODEL_SUGGESTIONS` ~line 60)
- Modify: `frontend/src/pages/Providers.form.test.tsx` (extend the existing template-prefill test)
- Test: the repo's frontend test command (check `frontend/package.json` scripts — likely `npm --prefix frontend test` or `npm --prefix frontend run test`)

**Interfaces:**
- Consumes: nothing from Task 1 (the two arrays are independently maintained by convention, not by shared code — see the "mirrors ... keep in sync" comments on both).
- Produces: two new `<option>`s in the admin UI's template `<select>`, plus new `<option>` entries under each wire format's `<datalist>` in the Pools page's model-override input.

- [ ] **Step 1: Write the failing test**

Extend `frontend/src/pages/Providers.form.test.tsx` with a new `it(...)`
mirroring `choosing_a_template_prefills_wire_format_base_url_and_model_but_stays_editable`:

```ts
it("choosing_the_opencode_openai_template_prefills_its_fields", async () => {
  render(<Providers />);
  await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

  await userEvent.selectOptions(screen.getByLabelText(/Template/), "OpenCode (OpenAI-compatible)");

  expect(screen.getByLabelText("Kind")).toHaveValue("passthrough");
  expect(screen.getByLabelText("API format")).toHaveValue("openai");
  expect(screen.getByLabelText("Base URL")).toHaveValue("https://opencode.ai/zen/go/v1/chat/completions");
  expect(screen.getByLabelText("Upstream model")).toHaveValue("kimi-k2.7-code");
});

it("choosing_the_opencode_anthropic_template_prefills_its_fields", async () => {
  render(<Providers />);
  await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

  await userEvent.selectOptions(screen.getByLabelText(/Template/), "OpenCode (Anthropic-compatible)");

  expect(screen.getByLabelText("Kind")).toHaveValue("passthrough");
  expect(screen.getByLabelText("API format")).toHaveValue("anthropic");
  expect(screen.getByLabelText("Base URL")).toHaveValue("https://opencode.ai/zen/go/v1/messages");
  expect(screen.getByLabelText("Upstream model")).toHaveValue("qwen3.7-max");
});
```

- [ ] **Step 2: Run to verify it fails**

Run the frontend test command scoped to this file — FAIL: the two option labels don't exist yet.

- [ ] **Step 3: Add the two template entries and model suggestions**

In `Providers.tsx`, append to `PROVIDER_TEMPLATES` (after the DeepSeek pair, before Codex/Command Code):

```ts
{
  label: "OpenCode (OpenAI-compatible)",
  kind: "passthrough",
  wire_format: "openai",
  suggestedId: "opencode-openai",
  base_url: "https://opencode.ai/zen/go/v1/chat/completions",
  upstream_model: "kimi-k2.7-code"
},
{
  label: "OpenCode (Anthropic-compatible)",
  kind: "passthrough",
  wire_format: "anthropic",
  suggestedId: "opencode-anthropic",
  base_url: "https://opencode.ai/zen/go/v1/messages",
  upstream_model: "qwen3.7-max"
},
```

In `Pools.tsx`, add a module-level comment-documented constant (unlike
`DEEPSEEK_MODEL_SUGGESTIONS`, do **not** share these across both lists —
see the design spec's "Model-name suggestions" section for why: each
OpenCode Go model id only answers on the one wire format the reference
implementation routes it to):

```ts
// Unlike DEEPSEEK_MODEL_SUGGESTIONS, these are NOT shared across both
// lists - each OpenCode Go model id only answers on the one wire format
// opencode.ai routes it to (see the design spec's reference-repo table).
// deepseek-v4-pro/deepseek-v4-flash are also OpenCode Go models but are
// omitted here - they're already in DEEPSEEK_MODEL_SUGGESTIONS below, and
// a duplicate <option> value trips React's key-uniqueness warning.
const OPENCODE_OPENAI_MODEL_SUGGESTIONS = [
  "glm-5.2",
  "glm-5.1",
  "kimi-k2.7-code",
  "kimi-k2.6",
  "mimo-v2.5",
  "mimo-v2.5-pro"
];

const OPENCODE_ANTHROPIC_MODEL_SUGGESTIONS = [
  "minimax-m3",
  "minimax-m2.7",
  "minimax-m2.5",
  "qwen3.7-max",
  "qwen3.7-plus",
  "qwen3.6-plus"
];
```

`deepseek-v4-pro`/`deepseek-v4-flash` will duplicate strings already in
`DEEPSEEK_MODEL_SUGGESTIONS` once both spread into `OPENAI_MODEL_SUGGESTIONS`
— that's fine, a `<datalist>` tolerates duplicate `<option>` values, and
deduping across two independently-sourced suggestion lists isn't worth the
code.

Then spread both into the existing arrays:

```ts
const OPENAI_MODEL_SUGGESTIONS = [
  ...,
  ...DEEPSEEK_MODEL_SUGGESTIONS,
  ...OPENCODE_OPENAI_MODEL_SUGGESTIONS
];

const ANTHROPIC_MODEL_SUGGESTIONS = [
  ...,
  ...DEEPSEEK_MODEL_SUGGESTIONS,
  ...OPENCODE_ANTHROPIC_MODEL_SUGGESTIONS
];
```

- [ ] **Step 4: Run to verify it passes**

Run the frontend test command — PASS, including both new tests and the full existing `Providers.form.test.tsx` / `Pools` suite unchanged.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/pages/Providers.tsx frontend/src/pages/Pools.tsx frontend/src/pages/Providers.form.test.tsx
git commit -m "feat(admin): OpenCode preset templates and model suggestions

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 3: Manual smoke check + docs

**Files:**
- Modify: `README.md` (preset dropdown list ~line 237)
- Test: none new — this task is a manual verification plus a doc update

**Interfaces:**
- Consumes: Tasks 1 and 2.
- Produces: no new code surface.

- [ ] **Step 1: Manual smoke check (real network, not `--offline`)**

The design spec's model-discovery claim is unverified from this repo
alone (no network access when the plan was authored). Before or shortly
after landing Tasks 1-2, run the wizard or admin UI against a real
OpenCode Go API key and confirm:

1. Creating a provider from each new template, with a real key, and
   sending one request through it (`curl .../v1/chat/completions` or
   `.../v1/messages` against the pool the wizard/UI auto-creates) gets a
   real completion back — proves the base URLs and auth-header choice in
   the design spec are correct, not just internally consistent.
2. The admin UI's "Fetch models" button (or `1router setup`'s Command
   Code-style discovery, if the wizard route for passthrough providers has
   one) against `https://opencode.ai/zen/go/v1/models` returns a live list
   rather than erroring — proves `derive_models_url` generalizes here as
   assumed.

If either check fails, that is a follow-up bug against
`derive_models_url` / the base URLs above, not a reason to add new code
to this plan — file it separately and keep this plan's scope to what
Tasks 1-2 shipped.

- [ ] **Step 2: Update `README.md`**

The preset dropdown line currently reads:

> A preset dropdown (OpenAI/Anthropic/DeepSeek) pre-fills wire format, base
> URL, and model — every field stays editable after picking one.

Update the parenthetical to name OpenCode too:

> A preset dropdown (OpenAI/Anthropic/DeepSeek/OpenCode) pre-fills wire
> format, base URL, and model — every field stays editable after picking
> one.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: mention OpenCode in the preset dropdown list

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage** — every section of
`docs/superpowers/specs/2026-08-05-opencode-preset-provider-design.md`
maps to a task:

| Spec section | Task |
|---|---|
| Two `PROVIDER_TEMPLATES` entries (Rust) | Task 1 |
| Two `PROVIDER_TEMPLATES` entries (frontend) + model suggestions | Task 2 |
| Model discovery comes for free (unverified claim) | Task 3 Step 1 |
| Out of scope: OpenCode Free | Global Constraints (explicit stop condition), not a task |

**2. Placeholder scan** — no `TBD`; every URL, label, and default model id
is given literally and matches the design spec's table exactly.

**3. Name consistency** — `"OpenCode (OpenAI-compatible)"` /
`"OpenCode (Anthropic-compatible)"` labels, `kimi-k2.7-code` /
`qwen3.7-max` default models, and the two base URLs appear identically in
Tasks 1, 2, and the design spec.

**4. Sequencing** — Tasks 1 and 2 are independent (no shared code, only a
by-convention "keep in sync" comment) and could run in parallel; Task 3
depends on both existing so its smoke check has something to click
through.

### Critical Files for Implementation
- /home/ducph/duc/1router/src/onboarding.rs
- /home/ducph/duc/1router/frontend/src/pages/Providers.tsx
- /home/ducph/duc/1router/frontend/src/pages/Pools.tsx
- /home/ducph/duc/1router/frontend/src/pages/Providers.form.test.tsx
- /home/ducph/duc/1router/README.md
