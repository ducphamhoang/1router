import { FormEvent, useEffect, useState } from "react";
import { apiJson } from "../lib/apiClient";

// Assembly note: B7's canonical response also carries `masked`/`origin` -
// harmless to ignore for the fields already used below, declared here for
// type accuracy (e.g. to skip the "Reveal" round-trip when already unmasked).
type SharedSecretResponse = {
  shared_secret: string;
  masked: boolean;
  origin: "env" | "sidecar_file";
};

type Pool = { id: string; wire_format: string };

export function Settings() {
  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [sharedSecret, setSharedSecret] = useState("");
  const [sharedSecretRevealed, setSharedSecretRevealed] = useState(false);
  const [sharedSecretEdited, setSharedSecretEdited] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pools, setPools] = useState<Pool[]>([]);

  async function loadSharedSecret(reveal = false) {
    const suffix = reveal ? "?reveal=true" : "";
    const body = await apiJson<SharedSecretResponse>(`/admin/settings/shared-secret${suffix}`);
    setSharedSecret(body.shared_secret);
    setSharedSecretRevealed(!body.masked);
    setSharedSecretEdited(false);
  }

  useEffect(() => {
    void loadSharedSecret(false);
    void apiJson<Pool[]>("/admin/pools").then(setPools).catch(() => setPools([]));
  }, []);

  const baseUrl = `${window.location.origin}/v1`;
  const exampleModel = pools[0]?.id ?? "<pool-id>";
  const anthropicPools = pools.filter((p) => p.wire_format === "anthropic");
  const openaiPools = pools.filter((p) => p.wire_format === "openai");

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setMessage("Copied to clipboard.");
    } catch {
      // Clipboard access can be denied/unavailable (permissions, non-HTTPS,
      // headless test env) - the value is still shown on screen to copy by hand.
    }
  }

  async function changePassword(event: FormEvent) {
    event.preventDefault();
    setMessage(null);
    setError(null);
    try {
      await apiJson("/admin/auth/password", {
        method: "PATCH",
        skipAuthRedirect: true,
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ current_password: currentPassword, new_password: newPassword })
      });
      setCurrentPassword("");
      setNewPassword("");
      setMessage("Password changed.");
    } catch (error) {
      setError(error instanceof Error ? error.message : "Password change failed.");
    }
  }

  async function saveSharedSecret(event: FormEvent) {
    event.preventDefault();
    setMessage(null);
    setError(null);
    if (!sharedSecretRevealed) {
      setError("Reveal the current shared secret before changing it.");
      return;
    }
    if (!sharedSecretEdited) {
      setError("Edit the revealed shared secret before saving it.");
      return;
    }
    try {
      await apiJson("/admin/settings/shared-secret", {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ shared_secret: sharedSecret })
      });
      setMessage("Shared secret saved.");
    } catch (error) {
      setError(error instanceof Error ? error.message : "Shared secret save failed.");
    }
  }

  return (
    <section aria-labelledby="settings-title">
      <h1 id="settings-title">Settings</h1>
      {message ? <p role="status">{message}</p> : null}
      {error ? <p role="alert">{error}</p> : null}

      <form onSubmit={changePassword}>
        <h2>Admin password</h2>
        <label>
          Current password
          <input type="password" value={currentPassword} onChange={(event) => setCurrentPassword(event.target.value)} />
        </label>
        <label>
          New password
          <input type="password" value={newPassword} onChange={(event) => setNewPassword(event.target.value)} />
        </label>
        <button type="submit">Change password</button>
      </form>

      <form onSubmit={saveSharedSecret}>
        <h2>Shared secret</h2>
        <label>
          Shared secret
          <input
            value={sharedSecret}
            disabled={!sharedSecretRevealed}
            onChange={(event) => {
              setSharedSecret(event.target.value);
              setSharedSecretEdited(true);
            }}
          />
        </label>
        <button type="button" onClick={() => loadSharedSecret(true)}>
          Reveal shared secret
        </button>
        <button type="submit" disabled={!sharedSecretRevealed || !sharedSecretEdited}>
          Save shared secret
        </button>
      </form>

      <section aria-labelledby="connect-title">
        <h2 id="connect-title">Connect a client</h2>
        <p>
          Point any OpenAI-compatible client (or a plain <code>curl</code>) at 1router by setting its base URL and
          API key below. Anthropic-format clients (Claude Code, etc.) use the same base host with{" "}
          <code>/v1/messages</code> instead of <code>/v1/chat/completions</code>.
        </p>

        <label>
          Base URL
          <div className="model-override-row">
            <input readOnly value={baseUrl} aria-label="Base URL" />
            <button type="button" className="btn-ghost" onClick={() => void copy(baseUrl)}>
              Copy
            </button>
          </div>
        </label>

        <label>
          API key
          <div className="model-override-row">
            <input
              readOnly
              value={sharedSecretRevealed ? sharedSecret : "•".repeat(12)}
              aria-label="API key for client connections"
            />
            <button
              type="button"
              className="btn-ghost"
              onClick={() => (sharedSecretRevealed ? void copy(sharedSecret) : void loadSharedSecret(true))}
            >
              {sharedSecretRevealed ? "Copy" : "Reveal"}
            </button>
          </div>
        </label>

        <h3>Available models</h3>
        {pools.length === 0 ? (
          <p>No pools yet — add a provider on the Providers page first.</p>
        ) : (
          <>
            {openaiPools.length > 0 ? (
              <>
                <p>OpenAI-compatible (use with `/v1/chat/completions`):</p>
                <ul>
                  {openaiPools.map((p) => (
                    <li key={p.id}>
                      <code>{p.id}</code>
                    </li>
                  ))}
                </ul>
              </>
            ) : null}
            {anthropicPools.length > 0 ? (
              <>
                <p>Anthropic-compatible (use with `/v1/messages`):</p>
                <ul>
                  {anthropicPools.map((p) => (
                    <li key={p.id}>
                      <code>{p.id}</code>
                    </li>
                  ))}
                </ul>
              </>
            ) : null}
          </>
        )}

        <pre>
          {`curl ${baseUrl}/chat/completions \\
  -H "Authorization: Bearer ${sharedSecretRevealed ? sharedSecret : "<your-api-key>"}" \\
  -H "Content-Type: application/json" \\
  -d '{"model":"${exampleModel}","messages":[{"role":"user","content":"hi"}]}'`}
        </pre>
      </section>
    </section>
  );
}
