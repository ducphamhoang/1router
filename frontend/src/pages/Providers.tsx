import { FormEvent, useEffect, useState } from "react";
import { apiJson } from "../lib/apiClient";
import { CodexOAuthPanel } from "../components/CodexOAuthPanel";
import { CommandCodeKeyPanel } from "../components/CommandCodeKeyPanel";

type Provider = {
  id: string;
  name: string;
  wire_format: string;
  kind: string;
  base_url?: string;
  api_key?: string;
  upstream_model: string;
};

type ProviderForm = Provider;

const emptyForm: ProviderForm = {
  id: "",
  name: "",
  wire_format: "openai",
  kind: "passthrough",
  base_url: "",
  api_key: "",
  upstream_model: ""
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
  // one upstream wire shape, so this also picks which single wire_format
  // pool "Make it directly callable" auto-creates. OAuth-kind providers
  // (Codex, Command Code) bridge Anthropic<->OpenAI themselves and serve
  // both client formats regardless of this value - the field is hidden
  // from the form for those kinds; this default just seeds the one pool
  // auto-created on save. Add the provider to a second pool of the other
  // wire_format from the Pools page if you need both routes callable.
  wire_format: string;
  suggestedId: string;
  base_url?: string;
  upstream_model?: string;
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
  const [preset, setPreset] = useState("custom");
  const [exposeAsPool, setExposeAsPool] = useState(true);
  // Tracks whether the user has typed their own id/name since the modal
  // opened, so applying a template never clobbers something they already
  // typed - only the untouched default gets overwritten.
  const [idTouched, setIdTouched] = useState(false);
  const [nameTouched, setNameTouched] = useState(false);

  async function loadProviders() {
    setProviders(await apiJson<Provider[]>("/admin/providers"));
  }

  useEffect(() => {
    void loadProviders();
  }, []);

  useEffect(() => {
    if (providers.length === 0) {
      return;
    }

    let cancelled = false;
    async function loadStates() {
      const entries = await Promise.all(
        providers.map(async (provider) => {
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

  function openNew() {
    setEditing(null);
    setForm(emptyForm);
    setPreset("custom");
    setExposeAsPool(true);
    setIdTouched(false);
    setNameTouched(false);
    setModalOpen(true);
  }

  function applyTemplate(label: string) {
    setPreset(label);
    if (label === "custom") {
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
      id: idTouched ? current.id : chosen.suggestedId,
      name: nameTouched ? current.name : chosen.label,
      base_url: chosen.base_url ?? "",
      api_key: chosen.kind === "passthrough" ? current.api_key : "",
      upstream_model: chosen.upstream_model ?? current.upstream_model
    }));
  }

  function openEdit(provider: Provider) {
    setEditing(provider);
    setForm({
      id: provider.id,
      name: provider.name,
      wire_format: provider.wire_format,
      kind: provider.kind,
      base_url: provider.base_url ?? "",
      api_key: "",
      upstream_model: provider.upstream_model
    });
    setModalOpen(true);
  }

  async function saveProvider(event: FormEvent) {
    event.preventDefault();
    setError(null);
    try {
      const body = editing
        ? {
            name: form.name,
            base_url: form.base_url,
            upstream_model: form.upstream_model,
            ...(form.api_key?.trim() ? { api_key: form.api_key } : {})
          }
        : form;
      const saved = await apiJson<Provider>(
        editing ? `/admin/providers/${encodeURIComponent(editing.id)}` : "/admin/providers",
        {
          method: editing ? "PATCH" : "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body)
        }
      );

      if (!editing && exposeAsPool) {
        // Best-effort: the provider is already saved above, so a pool-name
        // clash here (e.g. that id is already a pool of a different
        // wire_format) must surface as a warning, not undo the provider.
        try {
          await apiJson("/admin/pools", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ id: form.id, wire_format: form.wire_format })
          });
          await apiJson(`/admin/pools/${encodeURIComponent(form.id)}/members`, {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ provider_id: form.id, priority: 1 })
          });
        } catch (err) {
          setError(
            `Provider '${form.id}' was created, but could not expose it as a pool: ${
              err instanceof Error ? err.message : "unknown error"
            }. Add it to a pool from the Pools page instead.`
          );
          if (saved.kind !== "passthrough") {
            setEditing(saved);
            setForm({ ...saved, api_key: "" });
          } else {
            setModalOpen(false);
          }
          await loadProviders();
          return;
        }
      }

      // An OAuth-kind provider isn't usable yet - it still needs Connect (Codex)
      // or a pasted key (Command Code). Rather than closing the modal and
      // making the operator find it again via Edit, flip straight into edit
      // mode so that panel appears immediately.
      if (!editing && saved.kind !== "passthrough") {
        setEditing(saved);
        setForm({ ...saved, api_key: "" });
      } else {
        setModalOpen(false);
      }
      await loadProviders();
    } catch (error) {
      setError(error instanceof Error ? error.message : "Provider save failed.");
    }
  }

  async function deleteProvider(provider: Provider) {
    try {
      await apiJson(`/admin/providers/${encodeURIComponent(provider.id)}`, { method: "DELETE" });
      setProviders((current) => current.filter((item) => item.id !== provider.id));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Deleting provider failed.");
    }
  }

  return (
    <section aria-labelledby="providers-title">
      <h1 id="providers-title">Providers</h1>
      <button type="button" onClick={openNew}>
        New provider
      </button>
      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th>Wire format</th>
            <th>Kind</th>
            <th>Model</th>
            <th>State</th>
            <th>Actions</th>
          </tr>
        </thead>
        <tbody>
          {providers.map((provider) => (
            <tr key={provider.id}>
              <td>{provider.name}</td>
              <td>{provider.wire_format}</td>
              <td>{provider.kind}</td>
              <td>{provider.upstream_model}</td>
              <td>{states[provider.id] ?? "checking"}</td>
              <td>
                <button type="button" onClick={() => openEdit(provider)} aria-label={`Edit ${provider.name}`}>
                  Edit
                </button>
                <button type="button" onClick={() => deleteProvider(provider)} aria-label={`Delete ${provider.name}`}>
                  Delete
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {modalOpen ? (
        <form aria-label="Provider form" onSubmit={saveProvider}>
          {!editing ? (
            <>
              <label>
                Template <span className="optional">optional</span>
                <select value={preset} onChange={(event) => applyTemplate(event.target.value)}>
                  <option value="custom">Custom</option>
                  {PROVIDER_TEMPLATES.map((t) => (
                    <option key={t.label} value={t.label}>
                      {t.label}
                    </option>
                  ))}
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
            <select value={form.kind} onChange={(event) => setForm({ ...form, kind: event.target.value })}>
              <option value="passthrough">passthrough</option>
              <option value="oauth_codex">oauth_codex</option>
              <option value="oauth_command_code">oauth_command_code</option>
            </select>
          </label>
          {form.kind === "passthrough" ? (
            <>
              <label>
                Wire format
                <select
                  value={form.wire_format}
                  onChange={(event) => setForm({ ...form, wire_format: event.target.value })}
                >
                  <option value="openai">openai</option>
                  <option value="anthropic">anthropic</option>
                </select>
              </label>
              <label>
                Base URL
                <input value={form.base_url} onChange={(event) => setForm({ ...form, base_url: event.target.value })} />
              </label>
              <label>
                API key
                <input value={form.api_key} onChange={(event) => setForm({ ...form, api_key: event.target.value })} />
              </label>
            </>
          ) : (
            <p className="hint">
              This kind connects via {form.kind === "oauth_codex" ? "OAuth" : "an API key"}, not a base URL - save to
              continue setup below.
            </p>
          )}
          <label>
            Upstream model
            <input value={form.upstream_model} onChange={(event) => setForm({ ...form, upstream_model: event.target.value })} />
          </label>
          {!editing ? (
            <label className="checkbox-row">
              <input
                type="checkbox"
                checked={exposeAsPool}
                onChange={(event) => setExposeAsPool(event.target.checked)}
              />
              Make it directly callable (creates a matching pool, e.g. call <code>{form.id || "<id>"}</code> as the
              model)
            </label>
          ) : null}
          {editing && form.kind === "oauth_codex" ? <CodexOAuthPanel providerId={editing.id} /> : null}
          {editing && form.kind === "oauth_command_code" ? <CommandCodeKeyPanel providerId={editing.id} /> : null}
          {error ? <p role="alert">{error}</p> : null}
          <button type="submit" disabled={!editing && (!form.id.trim() || form.id.includes("/"))}>
            Save provider
          </button>
        </form>
      ) : null}
    </section>
  );
}
