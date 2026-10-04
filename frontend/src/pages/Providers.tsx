import { FormEvent, useEffect, useRef, useState } from "react";
import { apiJson } from "../lib/apiClient";
import { CodexOAuthPanel } from "../components/CodexOAuthPanel";
import { CommandCodeKeyPanel } from "../components/CommandCodeKeyPanel";
import { Modal } from "../components/Modal";

type Provider = {
  id: string;
  name: string;
  wire_format: string;
  kind: string;
  base_url?: string | null;
  api_key?: string | null;
  upstream_model: string;
  credential_configured?: boolean;
  // `ready`: the router could actually use this provider right now (an
  // endpoint is set / the OAuth account is connected). `credential_status`
  // is one of set | none | connected | not_connected.
  ready?: boolean;
  credential_status?: string;
  dataset_logging?: boolean;
  default_reasoning_effort?: string | null;
};

// The form keeps `default_reasoning_effort` as a plain string ("" = no
// default) rather than `string | null`, so the <select> below is always a
// controlled component; `saveProvider` normalizes "" back to null.
type ProviderForm = Omit<Provider, "default_reasoning_effort" | "base_url" | "api_key"> & {
  base_url: string;
  api_key: string;
  default_reasoning_effort: string;
};

type PoolRef = { id: string };

// Hand-mirrored from `capability_for` in src/core/reasoning.rs - keep the
// two in sync. Dispatch is by `kind` FIRST, then by wire_format, and only
// then by model name: the shape a request must take is a property of which
// adapter builds it, not of the model's name. In particular a Claude-named
// model behind an OpenAI-compatible mirror (wire_format "openai") is NOT
// eligible for a thinking budget.
//
// Known day-one gaps, deliberate and mirrored from the Rust side: legacy
// `claude-3-*` names and OpenAI's `o1`/`o3`/`o4-mini` are not matched.
export type ReasoningCapability = "unsupported" | "openai_effort" | "anthropic_thinking_budget";

export function capabilityFor(kind: string, wireFormat: string, upstreamModel: string): ReasoningCapability {
  const model = (upstreamModel ?? "").toLowerCase();
  if (kind === "oauth_codex") {
    return "openai_effort";
  }
  if (kind === "oauth_command_code") {
    // Mirrors commandcode::transform::wants_messages_shape.
    return model.includes("claude") ? "anthropic_thinking_budget" : "unsupported";
  }
  if (wireFormat === "openai" && model.startsWith("gpt-5")) {
    return "openai_effort";
  }
  if (
    wireFormat === "anthropic" &&
    (model.startsWith("claude-sonnet-") || model.startsWith("claude-opus-") || model.startsWith("claude-haiku-"))
  ) {
    return "anthropic_thinking_budget";
  }
  return "unsupported";
}

const REASONING_EFFORT_OPTIONS = [
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" }
];

// User-facing labels for backend enum values - the operator never needs to
// know these values are "passthrough"/"openai"/"anthropic" internally, only
// what kind of credential the provider needs and what shape its API speaks.
const KIND_LABELS: Record<string, string> = {
  passthrough: "API key",
  oauth_codex: "OAuth (Codex / ChatGPT account)",
  oauth_command_code: "OAuth (Command Code)"
};

const WIRE_FORMAT_LABELS: Record<string, string> = {
  openai: "OpenAI-compatible",
  anthropic: "Anthropic-compatible"
};

// The base URL is the FULL endpoint the router POSTs to - it is never
// rewritten - so a URL missing the usual path suffix is almost always a typo.
export function endpointWarning(wireFormat: string, rawUrl: string): { message: string; fixed: string } | null {
  const url = rawUrl.trim().replace(/\/+$/, "");
  if (!url) {
    return null;
  }
  if (url.includes("/v1/v1")) {
    return { message: "The URL contains a duplicated /v1.", fixed: url.replace("/v1/v1", "/v1") };
  }
  const suffix = wireFormat === "anthropic" ? "/messages" : "/chat/completions";
  if (!url.endsWith(suffix)) {
    return {
      message: `The router POSTs to this exact URL. It usually ends with ${suffix}.`,
      fixed: `${url}${suffix}`
    };
  }
  return null;
}

function credentialLabel(provider: Provider) {
  switch (provider.credential_status) {
    case "set":
      return "API key set";
    case "none":
      return "No API key";
    case "connected":
      return "Connected";
    case "not_connected":
      return "Not connected";
    default:
      return "—";
  }
}

function validateForm(form: ProviderForm, creating: boolean): string | null {
  if (!form.name.trim()) {
    return "Name is required.";
  }
  if (creating && !form.id.trim()) {
    return "Provider ID is required.";
  }
  if (!form.upstream_model.trim()) {
    return "Upstream model is required.";
  }
  if (form.kind === "passthrough") {
    const url = form.base_url.trim();
    if (!url) {
      return "Base URL is required.";
    }
    if (!/^https?:\/\//i.test(url)) {
      return "Base URL must start with http:// or https://.";
    }
  }
  return null;
}

const emptyForm: ProviderForm = {
  id: "",
  name: "",
  wire_format: "openai",
  kind: "passthrough",
  base_url: "",
  api_key: "",
  upstream_model: "",
  dataset_logging: false,
  default_reasoning_effort: ""
};

// Picking a template sets `kind` (+ a default wire_format the provider
// itself may not even care about - see below) and prefills a suggested
// id/name; every field, including kind, stays editable afterward, and
// "Custom" (no template applied) stays the default so this never gets in
// the way of an unlisted provider. Mirrors PROVIDER_TEMPLATES in
// src/onboarding.rs - keep the two in sync if either grows.
type ProviderTemplate = {
  label: string;
  kind: string;
  // Only meaningful for passthrough: a passthrough provider hits exactly
  // one upstream wire shape. OAuth-kind providers (Codex, Command Code)
  // bridge Anthropic<->OpenAI themselves and serve both client formats
  // regardless of this value - the field is hidden from the form for those
  // kinds.
  wire_format: string;
  suggestedId: string;
  base_url?: string;
  upstream_model?: string;
  // Only set for templates whose credential is a public, non-secret
  // constant (currently just OpenCode Free's "public" token) - every
  // other template needs a real secret the operator must type, so the API
  // key field is never prefilled for them.
  api_key?: string;
};

const PROVIDER_TEMPLATES: ProviderTemplate[] = [
  {
    label: "OpenAI",
    kind: "passthrough",
    wire_format: "openai",
    suggestedId: "openai",
    base_url: "https://api.openai.com/v1/chat/completions",
    upstream_model: "gpt-5.4"
  },
  {
    label: "Anthropic",
    kind: "passthrough",
    wire_format: "anthropic",
    suggestedId: "anthropic",
    base_url: "https://api.anthropic.com/v1/messages",
    upstream_model: "claude-sonnet-5"
  },
  {
    label: "DeepSeek (OpenAI-compatible)",
    kind: "passthrough",
    wire_format: "openai",
    suggestedId: "deepseek-openai",
    base_url: "https://api.deepseek.com/v1/chat/completions",
    upstream_model: "deepseek-flash"
  },
  {
    label: "DeepSeek (Anthropic-compatible)",
    kind: "passthrough",
    wire_format: "anthropic",
    suggestedId: "deepseek-anthropic",
    base_url: "https://api.deepseek.com/anthropic/v1/messages",
    upstream_model: "deepseek-flash"
  },
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
  {
    label: "OpenCode Free",
    kind: "passthrough",
    wire_format: "openai",
    suggestedId: "opencode-free",
    base_url: "https://opencode.ai/zen/v1/chat/completions",
    upstream_model: "deepseek-v4-flash-free",
    // Verified live: `Authorization: Bearer public` alone gets a real 200
    // from this endpoint - see the design spec's "OpenCode Free" section.
    api_key: "public"
  },
  {
    label: "Gemini (OpenAI-compatible)",
    kind: "passthrough",
    wire_format: "openai",
    suggestedId: "gemini",
    base_url: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
    upstream_model: "gemini-2.5-flash"
  },
  {
    label: "Codex (ChatGPT account)",
    kind: "oauth_codex",
    wire_format: "anthropic",
    suggestedId: "codex",
    // Placeholder until the model is discovered/set after Connect - mirrors
    // PENDING_MODEL in src/onboarding.rs.
    upstream_model: "pending"
  },
  {
    label: "Command Code",
    kind: "oauth_command_code",
    wire_format: "anthropic",
    suggestedId: "command-code",
    upstream_model: "pending"
  }
];

export function Providers() {
  const [providers, setProviders] = useState<Provider[]>([]);
  const [states, setStates] = useState<Record<string, string>>({});
  const [editing, setEditing] = useState<Provider | null>(null);
  const [form, setForm] = useState<ProviderForm>(emptyForm);
  const [modalOpen, setModalOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  // Edit modal: send `api_key: null` on save to clear the stored key.
  const [removeKey, setRemoveKey] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<{ provider: Provider; pools: string[] | null } | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [pageError, setPageError] = useState<string | null>(null);
  // Bumped whenever the modal opens/closes so late async results are dropped.
  const session = useRef(0);
  const [preset, setPreset] = useState("custom");
  // Tracks whether the user has typed their own id/name since the modal
  // opened, so applying a template never clobbers something they already
  // typed - only the untouched default gets overwritten.
  const [idTouched, setIdTouched] = useState(false);
  const [nameTouched, setNameTouched] = useState(false);
  // Populated once a Command Code credential is confirmed on file (either
  // already saved, or just established via login/paste in
  // CommandCodeKeyPanel) - lets the Upstream model field below switch from a
  // disabled placeholder to a real picker instead of the operator guessing a
  // model id blind.
  const [commandCodeModels, setCommandCodeModels] = useState<string[]>([]);
  const [commandCodeCredentialConfirmed, setCommandCodeCredentialConfirmed] = useState(false);
  // Passthrough-only: lets an operator confirm the saved api_key/base_url/
  // model actually work before relying on them, the same real-request probe
  // Pools.tsx already offers for a pool member's model_override - this is
  // the equivalent for the provider's own default upstream_model.
  const [validation, setValidation] = useState<
    { state: "checking" | "ok" | "error"; message?: string } | null
  >(null);
  // Passthrough create form only: lets an operator discover the upstream's
  // real model ids from the base_url/api_key they just typed, before the
  // provider is saved - same /models lookup `list-models` already does for
  // an existing provider, just without an id to call it against yet.
  const [previewModels, setPreviewModels] = useState<string[]>([]);
  const [modelFetch, setModelFetch] = useState<
    { state: "checking" | "error"; message?: string } | null
  >(null);

  async function loadProviders() {
    setProviders(await apiJson<Provider[]>("/admin/providers"));
  }

  useEffect(() => {
    void loadProviders();
  }, []);

  useEffect(() => {
    // A provider that isn't ready yet (no endpoint / account not connected)
    // has no meaningful runtime state - the table shows "Needs setup".
    const active = providers.filter((provider) => provider.ready !== false);
    if (active.length === 0) {
      return;
    }

    let cancelled = false;
    async function loadStates() {
      const entries = await Promise.all(
        active.map(async (provider) => {
          try {
            const body = await apiJson<{ status: string }>(`/admin/providers/${encodeURIComponent(provider.id)}/state`);
            return [provider.id, body.status] as const;
          } catch {
            return [provider.id, "unknown"] as const;
          }
        })
      );
      if (!cancelled) {
        setStates(Object.fromEntries(entries));
      }
    }

    void loadStates();
    const timer = window.setInterval(loadStates, 5000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [providers]);

  // Lets picking the same template twice (e.g. two OpenAI keys) suggest
  // "openai-2", "openai-3", ... instead of a ready-made id that just bounces
  // off a 409 Conflict on save.
  function uniqueSuggestedId(base: string): string {
    const ids = new Set(providers.map((p) => p.id));
    if (!ids.has(base)) {
      return base;
    }
    let n = 2;
    while (ids.has(`${base}-${n}`)) {
      n += 1;
    }
    return `${base}-${n}`;
  }

  function resetFormFeedback() {
    session.current += 1;
    setSaving(false);
    setError(null);
    setRemoveKey(false);
  }

  function openNew() {
    resetFormFeedback();
    setNotice(null);
    setEditing(null);
    setForm(emptyForm);
    // "Custom" (no template applied) stays the default here - the operator
    // still has to open this dropdown either way, so unlike the CLI wizard
    // (where pressing Enter accepts whatever is highlighted) there's no
    // "fast path" argument for pre-picking a template; it would only mean
    // pre-filled id/name text sitting in the fields to clear first. "Custom"
    // is still last in the dropdown list, though - see PROVIDER_TEMPLATES.
    setPreset("custom");
    setIdTouched(false);
    setNameTouched(false);
    setCommandCodeModels([]);
    setCommandCodeCredentialConfirmed(false);
    setValidation(null);
    setPreviewModels([]);
    setModelFetch(null);
    setModalOpen(true);
  }

  function applyTemplate(label: string) {
    setPreset(label);
    if (label === "custom") {
      // Switching back to "Custom" must clear whatever the previously
      // chosen template prefilled - otherwise the form silently keeps
      // showing that template's kind/wire_format/base_url/model/api_key
      // while the dropdown itself claims "Custom".
      setForm((current) => ({
        ...emptyForm,
        id: idTouched ? current.id : emptyForm.id,
        name: nameTouched ? current.name : emptyForm.name
      }));
      return;
    }
    const chosen = PROVIDER_TEMPLATES.find((template) => template.label === label);
    if (!chosen) {
      return;
    }
    setForm((current) => ({
      ...current,
      kind: chosen.kind,
      wire_format: chosen.wire_format,
      id: idTouched ? current.id : uniqueSuggestedId(chosen.suggestedId),
      name: nameTouched ? current.name : chosen.label,
      base_url: chosen.base_url ?? "",
      api_key: chosen.kind === "passthrough" ? (chosen.api_key ?? current.api_key) : "",
      upstream_model: chosen.upstream_model ?? current.upstream_model
    }));
  }

  function openEdit(provider: Provider) {
    resetFormFeedback();
    setNotice(null);
    setEditing(provider);
    setForm({
      id: provider.id,
      name: provider.name,
      wire_format: provider.wire_format,
      kind: provider.kind,
      base_url: provider.base_url ?? "",
      api_key: "",
      upstream_model: provider.upstream_model,
      dataset_logging: provider.dataset_logging ?? false,
      default_reasoning_effort: provider.default_reasoning_effort ?? ""
    });
    setCommandCodeModels([]);
    setCommandCodeCredentialConfirmed(Boolean(provider.credential_configured));
    setValidation(null);
    setPreviewModels([]);
    setModelFetch(null);
    setModalOpen(true);
  }

  async function validateProvider() {
    setValidation({ state: "checking" });
    try {
      // Editing an existing provider tests the already-saved key/base_url
      // (the operator may have typed a new one above, but it isn't
      // persisted yet); creating a new one has nothing saved yet, so it
      // sends the in-progress form values straight through instead.
      const result = editing
        ? await apiJson<{ ok: boolean; status?: number; message?: string }>(
            `/admin/providers/${encodeURIComponent(editing.id)}/validate-model`,
            {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({ model: form.upstream_model.trim() || undefined })
            }
          )
        : await apiJson<{ ok: boolean; status?: number; message?: string }>(
            "/admin/providers/validate-model-preview",
            {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({
                wire_format: form.wire_format,
                base_url: form.base_url?.trim() || "",
                api_key: form.api_key?.trim() || undefined,
                model: form.upstream_model.trim()
              })
            }
          );
      setValidation(
        result.ok
          ? { state: "ok" }
          : { state: "error", message: result.message || `Upstream returned HTTP ${result.status}.` }
      );
    } catch (err) {
      setValidation({ state: "error", message: err instanceof Error ? err.message : "Validation request failed." });
    }
  }

  async function fetchCodexModels(providerId: string) {
    setModelFetch({ state: "checking" });
    try {
      const result = await apiJson<{ ok: boolean; models?: string[]; reason?: string }>(
        `/admin/providers/${encodeURIComponent(providerId)}/list-models`
      );
      if (result.ok && result.models?.length) {
        setPreviewModels(result.models);
        setModelFetch(null);
        setForm((current) =>
          current.upstream_model.trim() && current.upstream_model !== "pending"
            ? current
            : { ...current, upstream_model: result.models![0] }
        );
      } else {
        setPreviewModels([]);
        setModelFetch({ state: "error", message: result.reason || "No models returned." });
      }
    } catch (err) {
      setPreviewModels([]);
      setModelFetch({ state: "error", message: err instanceof Error ? err.message : "Fetch failed." });
    }
  }

  async function fetchPreviewModels() {
    setModelFetch({ state: "checking" });
    try {
      const result = await apiJson<{ ok: boolean; models?: string[]; reason?: string }>(
        "/admin/providers/list-models-preview",
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            wire_format: form.wire_format,
            base_url: form.base_url?.trim() || "",
            api_key: form.api_key?.trim() || undefined
          })
        }
      );
      if (result.ok && result.models?.length) {
        setPreviewModels(result.models);
        setModelFetch(null);
        setForm((current) =>
          current.upstream_model.trim() ? current : { ...current, upstream_model: result.models![0] }
        );
      } else {
        setPreviewModels([]);
        setModelFetch({ state: "error", message: result.reason || "No models returned." });
      }
    } catch (err) {
      setPreviewModels([]);
      setModelFetch({ state: "error", message: err instanceof Error ? err.message : "Fetch failed." });
    }
  }

  async function saveProvider(event: FormEvent) {
    event.preventDefault();
    if (saving) {
      return;
    }
    setError(null);
    const invalid = validateForm(form, !editing);
    if (invalid) {
      setError(invalid);
      return;
    }
    setSaving(true);
    const mine = session.current;
    try {
      // A stale effort left in form state after the operator changed
      // kind/wire_format/model to something that can't carry one would be
      // rejected by the backend's write-time validation - clear it here
      // instead, matching the select that just disappeared from the form.
      const effort =
        capabilityFor(form.kind, form.wire_format, form.upstream_model) === "unsupported"
          ? null
          : form.default_reasoning_effort || null;
      const body = editing
        ? {
            name: form.name,
            base_url: form.base_url,
            upstream_model: form.upstream_model,
            dataset_logging: form.dataset_logging,
            default_reasoning_effort: effort,
            ...(removeKey ? { api_key: null } : form.api_key.trim() ? { api_key: form.api_key } : {})
          }
        : { ...form, default_reasoning_effort: effort };
      const saved = await apiJson<Provider>(
        editing ? `/admin/providers/${encodeURIComponent(editing.id)}` : "/admin/providers",
        {
          method: editing ? "PATCH" : "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body)
        }
      );

      // An OAuth-kind provider isn't usable yet - it still needs Connect (Codex)
      // or a pasted key (Command Code). Rather than closing the modal and
      // making the operator find it again via Edit, flip straight into edit
      // mode so that panel appears immediately.
      if (mine !== session.current) {
        await loadProviders();
        return;
      }
      if (!editing && saved.kind !== "passthrough") {
        setEditing(saved);
        setForm({
          ...saved,
          base_url: saved.base_url ?? "",
          api_key: "",
          default_reasoning_effort: saved.default_reasoning_effort ?? ""
        });
        setNotice(`Created ${saved.name}. Connect the account below, or close this dialog and do it later.`);
      } else {
        setModalOpen(false);
        setNotice(
          editing
            ? `Saved ${form.name.trim()}.`
            : form.api_key.trim()
              ? `Created ${form.name.trim()}.`
              : `Created ${form.name.trim()}. It has no API key yet - add one later if the endpoint requires it.`
        );
      }
      await loadProviders();
    } catch (error) {
      if (mine === session.current) {
        setError(error instanceof Error ? error.message : "Provider save failed.");
      }
    } finally {
      if (mine === session.current) {
        setSaving(false);
      }
    }
  }

  async function askDelete(provider: Provider) {
    setPageError(null);
    setDeleteTarget({ provider, pools: null });
    try {
      const pools = await apiJson<PoolRef[]>("/admin/pools");
      const names: string[] = [];
      await Promise.all(
        pools.map(async (pool) => {
          const members = await apiJson<{ provider_id: string }[]>(
            `/admin/pools/${encodeURIComponent(pool.id)}/members`
          );
          if (members.some((member) => member.provider_id === provider.id)) {
            names.push(pool.id);
          }
        })
      );
      setDeleteTarget((current) => (current?.provider.id === provider.id ? { provider, pools: names.sort() } : current));
    } catch {
      // Leave `pools` null: the confirmation still works, it just can't list pools.
    }
  }

  async function confirmDelete() {
    if (!deleteTarget || deleting) {
      return;
    }
    const { provider } = deleteTarget;
    setDeleting(true);
    try {
      await apiJson(`/admin/providers/${encodeURIComponent(provider.id)}`, { method: "DELETE" });
      setProviders((current) => current.filter((item) => item.id !== provider.id));
      setNotice(`Deleted ${provider.name}.`);
    } catch (err) {
      setPageError(err instanceof Error ? err.message : "Deleting provider failed.");
    } finally {
      setDeleting(false);
      setDeleteTarget(null);
    }
  }

  return (
    <section aria-labelledby="providers-title">
      <h1 id="providers-title">Providers</h1>
      <button type="button" onClick={openNew}>
        New provider
      </button>
      {notice ? <p role="status">{notice}</p> : null}
      {pageError ? <p role="alert">{pageError}</p> : null}
      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th>Wire format</th>
            <th>Kind</th>
            <th>Model</th>
            <th>Credentials</th>
            <th>State</th>
            <th>Actions</th>
          </tr>
        </thead>
        <tbody>
          {providers.map((provider) => (
            <tr key={provider.id}>
              <td>{provider.name}</td>
              <td>{WIRE_FORMAT_LABELS[provider.wire_format] ?? provider.wire_format}</td>
              <td>{KIND_LABELS[provider.kind] ?? provider.kind}</td>
              <td>{provider.upstream_model}</td>
              <td>{credentialLabel(provider)}</td>
              <td>
                {provider.ready === false ? (
                  <span className="badge badge-warn">Needs setup</span>
                ) : (
                  (states[provider.id] ?? "checking")
                )}
              </td>
              <td>
                <button type="button" onClick={() => openEdit(provider)} aria-label={`Edit ${provider.name}`}>
                  Edit
                </button>
                <button type="button" onClick={() => void askDelete(provider)} aria-label={`Delete ${provider.name}`}>
                  Delete
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {modalOpen ? (
        <Modal label={editing ? `Edit ${editing.name}` : "New provider"} onClose={() => setModalOpen(false)}>
          <form aria-label="Provider form" onSubmit={saveProvider}>
            {!editing ? (
              <>
                <label>
                  Template <span className="optional">optional</span>
                  <select value={preset} onChange={(event) => applyTemplate(event.target.value)}>
                    {PROVIDER_TEMPLATES.map((t) => (
                      <option key={t.label} value={t.label}>
                        {t.label}
                      </option>
                    ))}
                    <option value="custom">Custom</option>
                  </select>
                </label>
                <label>
                  Provider ID
                  <input
                    value={form.id}
                    onChange={(event) => {
                      setIdTouched(true);
                      setForm({ ...form, id: event.target.value });
                    }}
                  />
                </label>
              </>
            ) : null}
            <label>
              Name
              <input
                value={form.name}
                onChange={(event) => {
                  setNameTouched(true);
                  setForm({ ...form, name: event.target.value });
                }}
              />
            </label>
            <label>
              Kind
              <select
                value={form.kind}
                disabled={Boolean(editing)}
                onChange={(event) => setForm({ ...form, kind: event.target.value })}
              >
                <option value="passthrough">{KIND_LABELS.passthrough}</option>
                <option value="oauth_codex">{KIND_LABELS.oauth_codex}</option>
                <option value="oauth_command_code">{KIND_LABELS.oauth_command_code}</option>
              </select>
              {editing ? <span className="hint">Kind and API format can't be changed - recreate the provider to change them.</span> : null}
            </label>
            <label className="checkbox-row">
              <input
                type="checkbox"
                checked={Boolean(form.dataset_logging)}
                onChange={(event) => setForm({ ...form, dataset_logging: event.target.checked })}
              />
              Log requests/responses for this provider (dataset logging)
            </label>
            {(() => {
              // Recomputed live as kind/wire_format/upstream_model change.
              // Hidden entirely (not merely disabled) when this
              // provider/model can't carry a reasoning parameter - there's
              // nothing to choose, and offering one would only produce a
              // rejected save.
              const capability = capabilityFor(form.kind, form.wire_format, form.upstream_model);
              if (capability === "unsupported") {
                return null;
              }
              return (
                <label>
                  Reasoning effort <span className="optional">optional</span>
                  <select
                    aria-label="Reasoning effort"
                    value={form.default_reasoning_effort}
                    onChange={(event) => setForm({ ...form, default_reasoning_effort: event.target.value })}
                  >
                    <option value="">Provider default (send nothing)</option>
                    {REASONING_EFFORT_OPTIONS.map((option) => (
                      <option key={option.value} value={option.value}>
                        {option.label}
                      </option>
                    ))}
                  </select>
                  <span className="hint">
                    {capability === "anthropic_thinking_budget"
                      ? "Sent as an Anthropic thinking budget. A client that sets its own thinking/reasoning field always wins."
                      : "Sent as OpenAI reasoning_effort. A client that sets its own reasoning field always wins."}
                  </span>
                </label>
              );
            })()}
            {form.kind === "passthrough" ? (
              <>
                <label>
                  API format
                  <select
                    value={form.wire_format}
                    disabled={Boolean(editing)}
                    onChange={(event) => setForm({ ...form, wire_format: event.target.value })}
                  >
                    <option value="openai">{WIRE_FORMAT_LABELS.openai}</option>
                    <option value="anthropic">{WIRE_FORMAT_LABELS.anthropic}</option>
                  </select>
                </label>
                <label>
                  Base URL
                  <input value={form.base_url} onChange={(event) => setForm({ ...form, base_url: event.target.value })} />
                </label>
                {(() => {
                  const warning = endpointWarning(form.wire_format, form.base_url);
                  return warning ? (
                    <p className="hint" role="note">
                      {warning.message}{" "}
                      <button
                        type="button"
                        className="btn-ghost"
                        onClick={() => setForm({ ...form, base_url: warning.fixed })}
                      >
                        Use {warning.fixed}
                      </button>
                    </p>
                  ) : null;
                })()}
                <label>
                  API key
                  <div className="model-override-row">
                    <input
                      type="password"
                      autoComplete="off"
                      value={form.api_key}
                      disabled={removeKey}
                      placeholder={
                        editing
                          ? editing.api_key
                            ? `Leave blank to keep ${editing.api_key}`
                            : "No key stored"
                          : "Optional - can be added later"
                      }
                      onChange={(event) => setForm({ ...form, api_key: event.target.value })}
                    />
                    {editing && editing.credential_status === "set" ? (
                      <button
                        type="button"
                        className="btn-ghost"
                        aria-pressed={removeKey}
                        onClick={() => setRemoveKey((current) => !current)}
                      >
                        {removeKey ? "Keep key" : "Remove key"}
                      </button>
                    ) : null}
                    {!editing ? (
                      <button
                        type="button"
                        className="btn-ghost"
                        onClick={() => void fetchPreviewModels()}
                        disabled={modelFetch?.state === "checking" || !form.base_url?.trim()}
                        title="Fetch the model list from this base URL and API key"
                      >
                        Fetch models
                      </button>
                    ) : null}
                  </div>
                  {!editing && modelFetch ? (
                    <span
                      className={
                        modelFetch.state === "error" ? "validation-result validation-error" : "validation-result"
                      }
                      role={modelFetch.state === "error" ? "alert" : "status"}
                    >
                      {modelFetch.state === "checking" ? "Fetching models…" : `✗ ${modelFetch.message}`}
                    </span>
                  ) : null}
                </label>
                {(() => {
                  // Creating: nothing is saved yet, so Validate needs the
                  // in-progress base_url/api_key/model right here to have
                  // anything to send - unlike editing, where a blank field
                  // just means "keep what's already on file" and the model
                  // alone is enough (validateProvider falls back to the
                  // saved values for those).
                  const canValidate = editing
                    ? true
                    : Boolean(form.base_url?.trim() && form.upstream_model.trim());
                  if (!canValidate) {
                    // Nothing to send yet - don't just disable the button,
                    // don't offer it at all until there's a base_url + model.
                    return null;
                  }
                  return (
                    <>
                      <p className="hint">
                        {editing
                          ? "Tests the already-saved API key and base URL - click Save first if you just changed either."
                          : "Sends a real test request with the API key, base URL, and model above - nothing is saved yet."}
                      </p>
                      <div className="model-override-row">
                        <button
                          type="button"
                          className="btn-ghost"
                          onClick={() => void validateProvider()}
                          disabled={validation?.state === "checking"}
                        >
                          Validate
                        </button>
                      </div>
                      {validation ? (
                        <span
                          className={
                            validation.state === "ok"
                              ? "validation-result validation-ok"
                              : validation.state === "error"
                                ? "validation-result validation-error"
                                : "validation-result"
                          }
                          role={validation.state === "error" ? "alert" : "status"}
                        >
                          {validation.state === "checking"
                            ? "Sending a test request…"
                            : validation.state === "ok"
                              ? "✓ Model responded successfully."
                              : `✗ ${validation.message}`}
                        </span>
                      ) : null}
                    </>
                  );
                })()}
              </>
            ) : (
              <p className="hint">
                This kind connects via {form.kind === "oauth_codex" ? "OAuth" : "an API key"}, not a base URL - save to
                continue setup below.
              </p>
            )}
            {editing && form.kind === "oauth_command_code" ? (
              <CommandCodeKeyPanel
                providerId={editing.id}
                hasCredential={commandCodeCredentialConfirmed}
                onCredentialSaved={(models) => {
                  setCommandCodeCredentialConfirmed(true);
                  setCommandCodeModels(models);
                  setForm((current) =>
                    models.length && (!current.upstream_model || current.upstream_model === "pending")
                      ? { ...current, upstream_model: models[0] }
                      : current
                  );
                }}
              />
            ) : null}
            <label>
              Upstream model
              {form.kind === "oauth_command_code" && commandCodeModels.length > 0 ? (
                <select
                  value={form.upstream_model}
                  onChange={(event) => setForm({ ...form, upstream_model: event.target.value })}
                >
                  {commandCodeModels.map((model) => (
                    <option key={model} value={model}>
                      {model}
                    </option>
                  ))}
                </select>
              ) : previewModels.length > 0 &&
                ((!editing && form.kind === "passthrough") || (editing && form.kind === "oauth_codex")) ? (
                <select
                  value={form.upstream_model}
                  onChange={(event) => setForm({ ...form, upstream_model: event.target.value })}
                >
                  {(previewModels.includes(form.upstream_model) || !form.upstream_model
                    ? previewModels
                    : [form.upstream_model, ...previewModels]
                  ).map((model) => (
                    <option key={model} value={model}>
                      {model}
                    </option>
                  ))}
                </select>
              ) : (
                <input
                  value={form.upstream_model}
                  onChange={(event) => setForm({ ...form, upstream_model: event.target.value })}
                  disabled={form.kind === "oauth_command_code" && !commandCodeCredentialConfirmed}
                />
              )}
            </label>
            {form.kind === "oauth_command_code" && !commandCodeCredentialConfirmed ? (
              <p className="hint">Log in or paste an API key above to fetch the model list.</p>
            ) : null}
            {editing && form.kind === "oauth_codex" ? <CodexOAuthPanel providerId={editing.id} connected={Boolean(editing.credential_configured)} /> : null}
            {editing && form.kind === "oauth_codex" && editing.credential_configured ? (
              <>
                <div className="model-override-row">
                  <button
                    type="button"
                    className="btn-ghost"
                    onClick={() => void fetchCodexModels(editing.id)}
                    disabled={modelFetch?.state === "checking"}
                    title="Ask ChatGPT which Codex models this account can use"
                  >
                    Fetch models
                  </button>
                </div>
                {modelFetch ? (
                  <span
                    className={modelFetch.state === "error" ? "validation-result validation-error" : "validation-result"}
                    role={modelFetch.state === "error" ? "alert" : "status"}
                  >
                    {modelFetch.state === "checking" ? "Fetching models…" : `✗ ${modelFetch.message}`}
                  </span>
                ) : null}
              </>
            ) : null}
            {editing && form.kind === "oauth_codex" && editing.credential_configured ? (
              <div className="model-override-row">
                <button
                  type="button"
                  className="btn-ghost"
                  onClick={async () => {
                    try {
                      await apiJson(`/admin/providers/${encodeURIComponent(editing.id)}/oauth`, { method: "DELETE" });
                      setNotice("Account disconnected.");
                      setModalOpen(false);
                      await loadProviders();
                    } catch (err) {
                      setError(err instanceof Error ? err.message : "Disconnect failed.");
                    }
                  }}
                >
                  Disconnect account
                </button>
              </div>
            ) : null}
            {error ? <p role="alert">{error}</p> : null}
            <button type="submit" disabled={saving || (!editing && (!form.id.trim() || form.id.includes("/")))}>
              {saving ? "Saving…" : "Save provider"}
            </button>
          </form>
        </Modal>
      ) : null}
      {deleteTarget ? (
        <Modal label={`Delete ${deleteTarget.provider.name}`} onClose={() => setDeleteTarget(null)}>
          <h2>Delete {deleteTarget.provider.name}?</h2>
          {deleteTarget.pools && deleteTarget.pools.length > 0 ? (
            <p>It will also be removed from these pools: {deleteTarget.pools.join(", ")}.</p>
          ) : (
            <p>This cannot be undone.</p>
          )}
          <div className="model-override-row">
            <button type="button" className="btn-ghost" onClick={() => setDeleteTarget(null)}>
              Cancel
            </button>
            <button type="button" className="btn-danger" onClick={() => void confirmDelete()} disabled={deleting}>
              Confirm delete
            </button>
          </div>
        </Modal>
      ) : null}
    </section>
  );
}
