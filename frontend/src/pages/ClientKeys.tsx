import { FormEvent, useEffect, useState } from "react";
import { apiJson } from "../lib/apiClient";
import { Modal } from "../components/Modal";

export type ClientKey = {
  id: string;
  name: string;
  key_prefix: string;
  created_at: string;
  last_used_at: string | null;
  revoked_at: string | null;
};

type CreatedClientKey = ClientKey & { api_key: string };

type CallerStats = {
  callers: { caller_key_id: string | null; caller_name: string | null; total: number; successes: number; failures: number }[];
};

function formatTime(value: string | null) {
  return value ? new Date(value).toLocaleString() : "—";
}

// Per-caller API keys for /v1/*: each key identifies one user/client in the
// request log. The raw key is only ever returned by the create call, so it
// is shown once here and never again.
export function ClientKeys() {
  const [keys, setKeys] = useState<ClientKey[]>([]);
  const [requests, setRequests] = useState<Record<string, number>>({});
  const [name, setName] = useState("");
  const [created, setCreated] = useState<CreatedClientKey | null>(null);
  const [copied, setCopied] = useState(false);
  const [revoking, setRevoking] = useState<ClientKey | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function load() {
    try {
      const [list, stats] = await Promise.all([
        apiJson<ClientKey[]>("/admin/client-keys"),
        apiJson<CallerStats>("/admin/stats/callers")
      ]);
      setKeys(list);
      const byKey: Record<string, number> = {};
      for (const c of stats.callers) {
        if (c.caller_key_id) byKey[c.caller_key_id] = c.total;
      }
      setRequests(byKey);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Loading API keys failed.");
    }
  }

  useEffect(() => {
    void load();
  }, []);

  async function createKey(event: FormEvent) {
    event.preventDefault();
    setError(null);
    try {
      const body = await apiJson<CreatedClientKey>("/admin/client-keys", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name })
      });
      setCreated(body);
      setCopied(false);
      setName("");
      const { api_key: _secret, ...key } = body;
      setKeys((current) => [key, ...current]);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Creating API key failed.");
    }
  }

  async function confirmRevoke() {
    if (!revoking) return;
    const target = revoking;
    setRevoking(null);
    setError(null);
    try {
      const updated = await apiJson<ClientKey>(`/admin/client-keys/${encodeURIComponent(target.id)}`, {
        method: "DELETE"
      });
      setKeys((current) => current.map((k) => (k.id === updated.id ? updated : k)));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Revoking API key failed.");
    }
  }

  async function copyCreated() {
    if (!created) return;
    try {
      await navigator.clipboard.writeText(created.api_key);
      setCopied(true);
    } catch {
      // Clipboard can be unavailable (non-HTTPS origin) - the key is still
      // selectable in the field.
    }
  }

  return (
    <section aria-labelledby="client-keys-title">
      <h1 id="client-keys-title">API Keys</h1>
      <p>
        Issue one key per user or client. Callers send it as <code>Authorization: Bearer &lt;key&gt;</code> or{" "}
        <code>x-api-key</code> on <code>/v1/*</code>, and every request is logged under the key's name. Requests using
        the shared admin secret are logged as <code>admin</code>.
      </p>
      {error ? <p role="alert">{error}</p> : null}

      <form onSubmit={createKey}>
        <label>
          Name
          <input value={name} onChange={(event) => setName(event.target.value)} placeholder="e.g. alice" />
        </label>
        <button type="submit" disabled={!name.trim()}>
          Create key
        </button>
      </form>

      {created ? (
        <div role="status">
          <p>
            Key for <strong>{created.name}</strong> created. Copy it now — it won't be shown again.
          </p>
          <input aria-label="New API key" readOnly value={created.api_key} onFocus={(e) => e.target.select()} />
          <button type="button" onClick={copyCreated}>
            {copied ? "Copied" : "Copy"}
          </button>
          <button type="button" className="btn-ghost" onClick={() => setCreated(null)}>
            Done
          </button>
        </div>
      ) : null}

      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th>Key</th>
            <th>Created</th>
            <th>Last used</th>
            <th>Requests</th>
            <th>Status</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {keys.length === 0 ? (
            <tr>
              <td colSpan={7}>No API keys yet.</td>
            </tr>
          ) : (
            keys.map((key) => (
              <tr key={key.id}>
                <td>{key.name}</td>
                <td>
                  <code>{key.key_prefix}…</code>
                </td>
                <td>{formatTime(key.created_at)}</td>
                <td>{formatTime(key.last_used_at)}</td>
                <td>{requests[key.id] ?? 0}</td>
                <td>{key.revoked_at ? `Revoked ${formatTime(key.revoked_at)}` : "Active"}</td>
                <td>
                  {key.revoked_at ? null : (
                    <button
                      type="button"
                      className="btn-danger-quiet"
                      onClick={() => setRevoking(key)}
                      aria-label={`Revoke ${key.name}`}
                    >
                      Revoke
                    </button>
                  )}
                </td>
              </tr>
            ))
          )}
        </tbody>
      </table>

      {revoking ? (
        <Modal label="Revoke API key" onClose={() => setRevoking(null)}>
          <p>
            Revoke the key for <strong>{revoking.name}</strong> (<code>{revoking.key_prefix}…</code>)? Clients using it
            will get 401 immediately. This can't be undone.
          </p>
          <button type="button" className="btn-danger" onClick={confirmRevoke}>
            Revoke key
          </button>
          <button type="button" className="btn-ghost" onClick={() => setRevoking(null)}>
            Cancel
          </button>
        </Modal>
      ) : null}
    </section>
  );
}
