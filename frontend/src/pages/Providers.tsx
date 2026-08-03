import { FormEvent, useEffect, useState } from "react";
import { apiJson } from "../lib/apiClient";
import { CodexOAuthPanel } from "../components/CodexOAuthPanel";

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

// Pre-fills wire_format/base_url/upstream_model for a new provider; every
// field stays editable afterward, and "Custom" (no preset applied) stays the
// default so this never gets in the way of an unlisted provider.
type ProviderPreset = {
  label: string;
  wire_format: string;
  base_url: string;
  upstream_model: string;
};

const PROVIDER_PRESETS: ProviderPreset[] = [
  {
    label: "OpenAI",
    wire_format: "openai",
    base_url: "https://api.openai.com/v1/chat/completions",
    upstream_model: "gpt-5.4"
  },
  {
    label: "Anthropic",
    wire_format: "anthropic",
    base_url: "https://api.anthropic.com/v1/messages",
    upstream_model: "claude-sonnet-5"
  },
  {
    label: "DeepSeek (OpenAI-compatible)",
    wire_format: "openai",
    base_url: "https://api.deepseek.com/v1/chat/completions",
    upstream_model: "deepseek-flash"
  },
  {
    label: "DeepSeek (Anthropic-compatible)",
    wire_format: "anthropic",
    base_url: "https://api.deepseek.com/anthropic/v1/messages",
    upstream_model: "deepseek-flash"
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
    setModalOpen(true);
  }

  function applyPreset(label: string) {
    setPreset(label);
    if (label === "custom") {
      return;
    }
    const chosen = PROVIDER_PRESETS.find((preset) => preset.label === label);
    if (!chosen) {
      return;
    }
    setForm((current) => ({
      ...current,
      wire_format: chosen.wire_format,
      base_url: chosen.base_url,
      upstream_model: chosen.upstream_model
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
      await apiJson(editing ? `/admin/providers/${encodeURIComponent(editing.id)}` : "/admin/providers", {
        method: editing ? "PATCH" : "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body)
      });

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
          setModalOpen(false);
          await loadProviders();
          return;
        }
      }

      setModalOpen(false);
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
                Preset <span className="optional">optional</span>
                <select value={preset} onChange={(event) => applyPreset(event.target.value)}>
                  <option value="custom">Custom</option>
                  {PROVIDER_PRESETS.map((p) => (
                    <option key={p.label} value={p.label}>
                      {p.label}
                    </option>
                  ))}
                </select>
              </label>
              <label>
                Provider ID
                <input value={form.id} onChange={(event) => setForm({ ...form, id: event.target.value })} />
              </label>
            </>
          ) : null}
          <label>
            Name
            <input value={form.name} onChange={(event) => setForm({ ...form, name: event.target.value })} />
          </label>
          <label>
            Wire format
            <select value={form.wire_format} onChange={(event) => setForm({ ...form, wire_format: event.target.value })}>
              <option value="openai">openai</option>
              <option value="anthropic">anthropic</option>
            </select>
          </label>
          <label>
            Kind
            <select value={form.kind} onChange={(event) => setForm({ ...form, kind: event.target.value })}>
              <option value="passthrough">passthrough</option>
              <option value="oauth_codex">oauth_codex</option>
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
          {error ? <p role="alert">{error}</p> : null}
          <button type="submit" disabled={!editing && (!form.id.trim() || form.id.includes("/"))}>
            Save provider
          </button>
        </form>
      ) : null}
    </section>
  );
}
