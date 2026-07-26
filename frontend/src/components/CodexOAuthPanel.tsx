import { FormEvent, useState } from "react";
import { apiJson } from "../lib/apiClient";

type StartResponse = {
  authorize_url: string;
};

export function CodexOAuthPanel({ providerId }: { providerId: string }) {
  const [code, setCode] = useState("");
  const [state, setState] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function startOAuth() {
    setError(null);
    const body = await apiJson<StartResponse>(`/admin/providers/${providerId}/oauth/start`, { method: "POST" });
    window.open(body.authorize_url, "_blank", "noopener,noreferrer");
  }

  async function completeOAuth(event: FormEvent) {
    event.preventDefault();
    setError(null);
    setMessage(null);
    try {
      await apiJson(`/admin/providers/${providerId}/oauth/complete`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code, state })
      });
      setMessage("Codex OAuth connected.");
    } catch (error) {
      setError(error instanceof Error ? error.message : "Codex OAuth failed.");
    }
  }

  return (
    <section aria-labelledby="codex-oauth-title">
      <h2 id="codex-oauth-title">Codex OAuth</h2>
      <p>
        After you approve access, your browser will show a page that fails to load at localhost:1455 — that's expected.
        Copy the code and state values out of that page's address bar query string and paste them below.
      </p>
      <button type="button" onClick={startOAuth}>
        Start Codex OAuth
      </button>
      <form onSubmit={completeOAuth}>
        <label>
          Code
          <input value={code} onChange={(event) => setCode(event.target.value)} />
        </label>
        <label>
          State
          <input value={state} onChange={(event) => setState(event.target.value)} />
        </label>
        <button type="submit">Complete Codex OAuth</button>
      </form>
      {message ? <p role="status">{message}</p> : null}
      {error ? <p role="alert">{error}</p> : null}
    </section>
  );
}
