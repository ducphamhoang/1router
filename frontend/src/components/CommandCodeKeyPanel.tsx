import { useEffect, useRef, useState } from "react";
import { apiJson } from "../lib/apiClient";

type StartResponse = {
  authorize_url: string;
};

type StatusResponse = {
  status: "not_started" | "pending" | "success" | "error";
  error?: string;
};

type ListModelsResponse = {
  ok: boolean;
  models?: string[];
  reason?: string;
};

type ValidateModelResponse = {
  ok: boolean;
  message?: string;
};

const POLL_INTERVAL_MS = 1000;
const POLL_TIMEOUT_MS = 20000;

// Command Code's model list mixes plans-gated flagship models (Sonnet,
// Opus, ...) with cheap/free ones - probing with an arbitrary model (e.g.
// whatever list-models happened to return first) can 403 with
// MODEL_NOT_IN_PLAN even though the key itself is fine. Bias the probe (and
// the default prefill in the parent form) toward the cheap/free end instead
// of picking blind.
const CHEAP_MODEL_HINTS = ["deepseek", "haiku", "mini", "flash", "lite", "small", "free"];
// How many cheap-first candidates to actually try validating against before
// giving up and surfacing the last error - keeps a bad/expensive-only model
// list from turning "Validate key" into an unbounded sequence of real API
// calls.
const MAX_VALIDATION_ATTEMPTS = 3;

function orderModelsCheapFirst(models: string[]): string[] {
  const rank = (model: string) => {
    const lower = model.toLowerCase();
    const index = CHEAP_MODEL_HINTS.findIndex((hint) => lower.includes(hint));
    return index === -1 ? CHEAP_MODEL_HINTS.length : index;
  };
  return [...models].sort((a, b) => rank(a) - rank(b));
}

export function CommandCodeKeyPanel({
  providerId,
  hasCredential,
  onCredentialSaved
}: {
  providerId: string;
  hasCredential: boolean;
  // Called with the freshly discovered model list (may be empty if
  // discovery failed) whenever a credential becomes available - both right
  // after login/validate and, once, on mount if one was already on file - so
  // the parent form can enable and prefill its Upstream model field.
  onCredentialSaved: (models: string[]) => void;
}) {
  const [apiKey, setApiKey] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loggingIn, setLoggingIn] = useState(false);
  const [validating, setValidating] = useState(false);
  const pollTimer = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    return () => {
      if (pollTimer.current) clearInterval(pollTimer.current);
    };
  }, []);

  useEffect(() => {
    if (hasCredential) {
      void discoverModels();
    }
    // Only re-run this for a provider actually having a credential already
    // on mount - a credential established during this session already
    // triggers discovery from loginWithBrowser/validateKey below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function discoverModels(): Promise<string[]> {
    try {
      const result = await apiJson<ListModelsResponse>(
        `/admin/providers/${encodeURIComponent(providerId)}/list-models`
      );
      const models = orderModelsCheapFirst(result.ok && result.models ? result.models : []);
      onCredentialSaved(models);
      return models;
    } catch {
      onCredentialSaved([]);
      return [];
    }
  }

  function stopPolling() {
    if (pollTimer.current) {
      clearInterval(pollTimer.current);
      pollTimer.current = null;
    }
    setLoggingIn(false);
  }

  async function loginWithBrowser() {
    setError(null);
    setMessage(null);
    try {
      const body = await apiJson<StartResponse>(
        `/admin/providers/${encodeURIComponent(providerId)}/commandcode/browser-login/start`,
        { method: "POST" }
      );
      window.open(body.authorize_url, "_blank", "noopener,noreferrer");
      setLoggingIn(true);
      setMessage("Waiting for you to finish logging in at commandcode.ai...");

      const deadline = Date.now() + POLL_TIMEOUT_MS;
      pollTimer.current = setInterval(async () => {
        try {
          const status = await apiJson<StatusResponse>(
            `/admin/providers/${encodeURIComponent(providerId)}/commandcode/browser-login/status`
          );
          if (status.status === "success") {
            stopPolling();
            setMessage("Command Code connected.");
            await discoverModels();
          } else if (status.status === "error") {
            stopPolling();
            setError(status.error ?? "Command Code login failed.");
          } else if (Date.now() > deadline) {
            stopPolling();
            setError("Command Code login timed out. Paste your API key below instead.");
          }
        } catch (err) {
          stopPolling();
          setError(err instanceof Error ? err.message : "Command Code login failed.");
        }
      }, POLL_INTERVAL_MS);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not start Command Code login.");
    }
  }

  // Stores the pasted key, discovers Command Code's model list, then sends
  // one real minimal chat request through this provider's own adapter
  // (the same check `/admin/providers/:id/validate-model` does for manually
  // typed models elsewhere) to actually confirm the key authenticates -
  // list-models alone can't tell us that, since it's a public, unauthenticated
  // endpoint that returns the same list regardless of whether the key works.
  async function validateKey() {
    const key = apiKey.trim();
    setError(null);
    setMessage(null);
    if (!key) {
      setError("Enter an API key first.");
      return;
    }
    setValidating(true);
    try {
      await apiJson(`/admin/providers/${encodeURIComponent(providerId)}/commandcode/key`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ api_key: key })
      });

      const models = await discoverModels();
      if (models.length === 0) {
        throw new Error("Key saved, but no Command Code models were found to validate against.");
      }

      let lastFailure = "Command Code rejected this API key.";
      let validatedModel: string | null = null;
      for (const model of models.slice(0, MAX_VALIDATION_ATTEMPTS)) {
        const validated = await apiJson<ValidateModelResponse>(
          `/admin/providers/${encodeURIComponent(providerId)}/validate-model`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ model })
          }
        );
        if (validated.ok) {
          validatedModel = model;
          break;
        }
        lastFailure = validated.message ?? lastFailure;
      }
      if (!validatedModel) {
        onCredentialSaved([]);
        throw new Error(lastFailure);
      }

      setApiKey("");
      setMessage(`Command Code key validated (tested against ${validatedModel}).`);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Command Code key validation failed.");
    } finally {
      setValidating(false);
    }
  }

  return (
    <section aria-labelledby="commandcode-key-title">
      <h2 id="commandcode-key-title">Command Code</h2>
      {hasCredential ? <p>A Command Code API key is already saved. Logging in or validating a new key below replaces it.</p> : null}
      <p>
        Log in with your browser to fetch an API key automatically. This only works when you're
        viewing this admin UI on the same machine running 1router - it opens a local callback
        listener that commandcode.ai posts the key back to.
      </p>
      <button type="button" onClick={loginWithBrowser} disabled={loggingIn}>
        {loggingIn ? "Waiting for login..." : "Login with browser"}
      </button>
      <p>Or paste an existing API key:</p>
      <label>
        API key
        <input type="password" value={apiKey} onChange={(event) => setApiKey(event.target.value)} />
      </label>
      <button type="button" onClick={validateKey} disabled={validating || !apiKey.trim()}>
        {validating ? "Validating..." : "Validate key"}
      </button>
      {message ? <p role="status">{message}</p> : null}
      {error ? <p role="alert">{error}</p> : null}
    </section>
  );
}
