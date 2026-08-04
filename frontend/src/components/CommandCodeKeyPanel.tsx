import { FormEvent, useState } from "react";
import { apiJson } from "../lib/apiClient";

export function CommandCodeKeyPanel({ providerId }: { providerId: string }) {
  const [apiKey, setApiKey] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function save(event: FormEvent) {
    event.preventDefault(); setMessage(null); setError(null);
    try {
      await apiJson(`/admin/providers/${encodeURIComponent(providerId)}/commandcode/key`, {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ api_key: apiKey })
      });
      setApiKey(""); setMessage("Command Code API key saved.");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Command Code key save failed.");
    }
  }

  return (
    <section aria-labelledby="commandcode-key-title">
      <h2 id="commandcode-key-title">Command Code</h2>
      <form onSubmit={save}>
        <label>
          API key
          <input type="password" value={apiKey} onChange={(event) => setApiKey(event.target.value)} />
        </label>
        <button type="submit">Save Command Code key</button>
      </form>
      {message ? <p role="status">{message}</p> : null}
      {error ? <p role="alert">{error}</p> : null}
    </section>
  );
}
