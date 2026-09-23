import { FormEvent, useEffect, useState } from "react";
import { apiJson } from "../lib/apiClient";
import { Modal } from "../components/Modal";

export type User = {
  id: string;
  name: string;
  key_prefix: string;
  created_at: string;
  last_used_at: string | null;
  revoked_at: string | null;
};

type UserWithKey = User & { api_key: string };

type UserStats = {
  users: { user_id: string | null; total: number; successes: number; failures: number }[];
};

type PendingAction = { kind: "revoke" | "rotate"; user: User };

function formatTime(value: string | null) {
  return value ? new Date(value).toLocaleString() : "—";
}

// Users: named bearer credentials for /v1/*. Each request made with a user's
// key is attributed to that user's id in the request log. The raw key is only
// ever returned by create/rotate, so it is shown once here and never again.
export function Users() {
  const [users, setUsers] = useState<User[]>([]);
  const [requests, setRequests] = useState<Record<string, number>>({});
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [issued, setIssued] = useState<{ user: User; apiKey: string; rotated: boolean } | null>(null);
  const [copied, setCopied] = useState(false);
  const [pending, setPending] = useState<PendingAction | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function load() {
    try {
      const [list, stats] = await Promise.all([
        apiJson<User[]>("/admin/users"),
        apiJson<UserStats>("/admin/stats/users")
      ]);
      setUsers(list);
      const byUser: Record<string, number> = {};
      for (const row of stats.users) {
        if (row.user_id) byUser[row.user_id] = row.total;
      }
      setRequests(byUser);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Loading users failed.");
    }
  }

  useEffect(() => {
    void load();
  }, []);

  function upsert(user: User) {
    setUsers((current) =>
      current.some((u) => u.id === user.id) ? current.map((u) => (u.id === user.id ? user : u)) : [user, ...current]
    );
  }

  function showIssued(body: UserWithKey, rotated: boolean) {
    const { api_key, ...user } = body;
    upsert(user);
    setIssued({ user, apiKey: api_key, rotated });
    setCopied(false);
  }

  async function createUser(event: FormEvent) {
    event.preventDefault();
    setError(null);
    try {
      const body = await apiJson<UserWithKey>("/admin/users", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id: id.trim(), name: name.trim() || undefined })
      });
      showIssued(body, false);
      setId("");
      setName("");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Creating user failed.");
    }
  }

  async function confirmPending() {
    if (!pending) return;
    const { kind, user } = pending;
    setPending(null);
    setError(null);
    try {
      const path = `/admin/users/${encodeURIComponent(user.id)}/${kind}`;
      if (kind === "rotate") {
        showIssued(await apiJson<UserWithKey>(path, { method: "POST" }), true);
      } else {
        upsert(await apiJson<User>(path, { method: "POST" }));
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : `${kind === "rotate" ? "Rotating" : "Revoking"} key failed.`);
    }
  }

  async function copyIssued() {
    if (!issued) return;
    try {
      await navigator.clipboard.writeText(issued.apiKey);
      setCopied(true);
    } catch {
      // Clipboard can be unavailable (non-HTTPS origin) - the key is still
      // selectable in the field.
    }
  }

  return (
    <section aria-labelledby="users-title">
      <h1 id="users-title">Users</h1>
      <p>
        Issue one key per user or client. Callers send it as <code>Authorization: Bearer &lt;key&gt;</code> or{" "}
        <code>x-api-key</code> on <code>/v1/*</code>, and every request is logged under the user's id. Requests using
        the shared admin secret are logged as <code>admin</code>.
      </p>
      {error ? <p role="alert">{error}</p> : null}

      <form onSubmit={createUser}>
        <label>
          User id
          <input value={id} onChange={(event) => setId(event.target.value)} placeholder="e.g. alice" />
        </label>
        <label>
          Display name (optional)
          <input value={name} onChange={(event) => setName(event.target.value)} />
        </label>
        <button type="submit" disabled={!id.trim()}>
          Create user
        </button>
      </form>

      {issued ? (
        <div role="status">
          <p>
            {issued.rotated ? "New key" : "Key"} for <strong>{issued.user.id}</strong>
            {issued.rotated ? " issued — the old key no longer works." : " created."} Copy it now — it won't be shown
            again.
          </p>
          <input aria-label="New API key" readOnly value={issued.apiKey} onFocus={(e) => e.target.select()} />
          <button type="button" onClick={copyIssued}>
            {copied ? "Copied" : "Copy"}
          </button>
          <button type="button" className="btn-ghost" onClick={() => setIssued(null)}>
            Done
          </button>
        </div>
      ) : null}

      <table>
        <thead>
          <tr>
            <th>User id</th>
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
          {users.length === 0 ? (
            <tr>
              <td colSpan={8}>No users yet.</td>
            </tr>
          ) : (
            users.map((user) => (
              <tr key={user.id}>
                <td>{user.id}</td>
                <td>{user.name}</td>
                <td>
                  <code>{user.key_prefix}…</code>
                </td>
                <td>{formatTime(user.created_at)}</td>
                <td>{formatTime(user.last_used_at)}</td>
                <td>{requests[user.id] ?? 0}</td>
                <td>{user.revoked_at ? `Revoked ${formatTime(user.revoked_at)}` : "Active"}</td>
                <td>
                  {user.revoked_at ? null : (
                    <>
                      <button
                        type="button"
                        onClick={() => setPending({ kind: "rotate", user })}
                        aria-label={`Rotate key for ${user.id}`}
                      >
                        Rotate
                      </button>
                      <button
                        type="button"
                        className="btn-danger-quiet"
                        onClick={() => setPending({ kind: "revoke", user })}
                        aria-label={`Revoke ${user.id}`}
                      >
                        Revoke
                      </button>
                    </>
                  )}
                </td>
              </tr>
            ))
          )}
        </tbody>
      </table>

      {pending ? (
        <Modal label={pending.kind === "rotate" ? "Rotate key" : "Revoke user"} onClose={() => setPending(null)}>
          {pending.kind === "rotate" ? (
            <p>
              Issue a new key for <strong>{pending.user.id}</strong>? The current key (
              <code>{pending.user.key_prefix}…</code>) stops working immediately.
            </p>
          ) : (
            <p>
              Revoke <strong>{pending.user.id}</strong>? Clients using its key will get 401 immediately. This can't be
              undone.
            </p>
          )}
          <button
            type="button"
            className={pending.kind === "rotate" ? "btn-primary" : "btn-danger"}
            onClick={confirmPending}
          >
            {pending.kind === "rotate" ? "Rotate key" : "Revoke user"}
          </button>
          <button type="button" className="btn-ghost" onClick={() => setPending(null)}>
            Cancel
          </button>
        </Modal>
      ) : null}
    </section>
  );
}
