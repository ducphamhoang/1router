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

type AuthModeResponse = {
  require_shared_secret: boolean;
  origin: "env" | "db" | "default";
};

type SecurityStatusResponse = {
  require_shared_secret: boolean;
  listen_addr_is_loopback: boolean;
};

type Pool = { id: string; wire_format: string };
type Provider = { id: string; name: string; kind: string; wire_format: string };

// Per-provider result of calling its own GET .../models - kept separate
// from `pools` because it's a real network call to each provider, so it's
// triggered on demand rather than on every Integration page load.
type Discovery = { status: "loading" } | { status: "ok"; models: string[] } | { status: "error"; message: string };

export function Integration() {
  const [sharedSecret, setSharedSecret] = useState("");
  const [sharedSecretRevealed, setSharedSecretRevealed] = useState(false);
  const [sharedSecretEdited, setSharedSecretEdited] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pools, setPools] = useState<Pool[]>([]);
  const [providers, setProviders] = useState<Provider[]>([]);
  const [discovery, setDiscovery] = useState<Record<string, Discovery>>({});
  const [discovering, setDiscovering] = useState(false);
  const [requireSharedSecret, setRequireSharedSecret] = useState(true);
  const [authModeOrigin, setAuthModeOrigin] = useState<AuthModeResponse["origin"]>("default");
  const [listenAddrIsLoopback, setListenAddrIsLoopback] = useState(true);
  const [selectedRequireSharedSecret, setSelectedRequireSharedSecret] = useState<boolean | null>(null);
  const [openConfirmation, setOpenConfirmation] = useState<"base" | "critical" | null>(null);

  async function loadSharedSecret(reveal = false) {
    const suffix = reveal ? "?reveal=true" : "";
    const body = await apiJson<SharedSecretResponse>(`/admin/settings/shared-secret${suffix}`);
    setSharedSecret(body.shared_secret);
    setSharedSecretRevealed(!body.masked);
    setSharedSecretEdited(false);
  }

  useEffect(() => {
    void loadSharedSecret(false);
    void apiJson<AuthModeResponse>("/admin/settings/auth-mode")
      .then((body) => {
        setRequireSharedSecret(body.require_shared_secret);
        setAuthModeOrigin(body.origin);
      })
      .catch(() => undefined);
    void apiJson<SecurityStatusResponse>("/admin/settings/security-status")
      .then((body) => setListenAddrIsLoopback(body.listen_addr_is_loopback))
      .catch(() => undefined);
    void apiJson<Pool[]>("/admin/pools").then(setPools).catch(() => setPools([]));
    void apiJson<Provider[]>("/admin/providers").then(setProviders).catch(() => setProviders([]));
  }, []);

  const displayedRequireSharedSecret = selectedRequireSharedSecret ?? requireSharedSecret;

  async function saveAuthMode(value: boolean) {
    setMessage(null);
    setError(null);
    try {
      const body = await apiJson<AuthModeResponse>("/admin/settings/auth-mode", {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ require_shared_secret: value })
      });
      setRequireSharedSecret(body.require_shared_secret);
      setAuthModeOrigin(body.origin);
      setSelectedRequireSharedSecret(null);
      setOpenConfirmation(null);
      setMessage("Client API access updated.");
    } catch (error) {
      setSelectedRequireSharedSecret(null);
      setOpenConfirmation(null);
      setError(error instanceof Error ? error.message : "Client API access update failed.");
    }
  }

  function selectAuthMode(value: boolean) {
    if (authModeOrigin === "env") {
      setError("ROUTER_REQUIRE_SHARED_SECRET is set; change or unset the environment variable instead.");
      return;
    }
    setSelectedRequireSharedSecret(value);
    if (value) {
      void saveAuthMode(true);
    } else if (listenAddrIsLoopback) {
      setOpenConfirmation("base");
    } else {
      setOpenConfirmation("base");
    }
  }

  function confirmOpenAccess() {
    if (openConfirmation === "base" && !listenAddrIsLoopback) {
      setOpenConfirmation("critical");
      return;
    }
    void saveAuthMode(false);
  }

  const baseUrl = `${window.location.origin}/v1`;
  const exampleModel = pools[0]?.id ?? "<pool-id>";
  const anthropicPools = pools.filter((p) => p.wire_format === "anthropic");
  const openaiPools = pools.filter((p) => p.wire_format === "openai");
  const poolIds = new Set(pools.map((p) => p.id));
  const discoverableProviders = providers.filter((p) => p.kind === "passthrough" || p.kind === "oauth_command_code");

  async function discoverModels() {
    setDiscovering(true);
    await Promise.all(
      discoverableProviders.map(async (provider) => {
        setDiscovery((current) => ({ ...current, [provider.id]: { status: "loading" } }));
        try {
          const result = await apiJson<{ ok: boolean; models?: string[]; reason?: string }>(
            `/admin/providers/${encodeURIComponent(provider.id)}/list-models`
          );
          setDiscovery((current) => ({
            ...current,
            [provider.id]:
              result.ok && result.models
                ? { status: "ok", models: result.models }
                : { status: "error", message: result.reason ?? "No models returned." }
          }));
        } catch (err) {
          setDiscovery((current) => ({
            ...current,
            [provider.id]: {
              status: "error",
              message: err instanceof Error ? err.message : "Fetching models failed."
            }
          }));
        }
      })
    );
    setDiscovering(false);
  }

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setMessage("Copied to clipboard.");
    } catch {
      // Clipboard access can be denied/unavailable (permissions, non-HTTPS,
      // headless test env) - the value is still shown on screen to copy by hand.
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
    <section aria-labelledby="integration-title">
      <h1 id="integration-title">Integration</h1>
      {message ? <p role="status">{message}</p> : null}
      {error ? <p role="alert">{error}</p> : null}

      <section aria-labelledby="client-access-title">
        <h2 id="client-access-title">Client API access</h2>
        <fieldset disabled={authModeOrigin === "env"}>
          <legend>/v1 access mode</legend>
          <label className="radio-row">
            <input
              type="radio"
              name="client-api-access"
              checked={displayedRequireSharedSecret}
              onChange={() => selectAuthMode(true)}
            />
            API key required — clients send Authorization: Bearer &lt;key&gt;
          </label>
          <label className="radio-row">
            <input
              type="radio"
              name="client-api-access"
              checked={!displayedRequireSharedSecret}
              onChange={() => selectAuthMode(false)}
            />
            Open access — /v1/* accepts requests with no API key
          </label>
        </fieldset>
        <p>
          The admin UI still requires this password. Anyone who can reach this gateway can send requests through your providers.
        </p>
        {openConfirmation ? (
          <div role="group" aria-label="Confirm open access">
            <p>
              {openConfirmation === "critical"
                ? "This gateway is not bound to localhost. Anyone who can reach it can use your providers."
                : "Open access lets /v1/* accept requests without an API key."}
            </p>
            <button type="button" onClick={confirmOpenAccess}>
              {openConfirmation === "critical" ? "Yes, enable open access" : listenAddrIsLoopback ? "Enable open access" : "Review non-local open access"}
            </button>
            <button type="button" onClick={() => { setSelectedRequireSharedSecret(null); setOpenConfirmation(null); }}>
              Cancel
            </button>
          </div>
        ) : null}
      </section>

      {requireSharedSecret ? (
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
      ) : null}

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

        {requireSharedSecret ? (
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
        ) : (
          <p>
            Open access is on — <code>/v1/*</code> accepts requests without an API key. Just set the base URL above.
          </p>
        )}

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
          {displayedRequireSharedSecret
            ? `curl ${baseUrl}/chat/completions \\
  -H "Authorization: Bearer ${sharedSecretRevealed ? sharedSecret : "<your-api-key>"}" \\
  -H "Content-Type: application/json" \\
  -d '{"model":"${exampleModel}","messages":[{"role":"user","content":"hi"}]}'`
            : `curl ${baseUrl}/chat/completions \\
  # no API key needed — open access is on \\
  -H "Content-Type: application/json" \\
  -d '{"model":"${exampleModel}","messages":[{"role":"user","content":"hi"}]}'`}
        </pre>

        <h3>Other models available from your providers</h3>
        <p>
          The list above is what clients can actually call right now (<code>model</code> = a pool id). A provider
          often supports more models than the one it's currently pooled under — check here, then add one via a{" "}
          <code>model_override</code> on the Pools page to make it callable.
        </p>
        <button
          type="button"
          onClick={() => void discoverModels()}
          disabled={discovering || discoverableProviders.length === 0}
        >
          {discovering ? "Checking providers…" : "Check providers for available models"}
        </button>
        {discoverableProviders.length === 0 ? (
          <p>No providers with a discoverable model list to check (Codex OAuth providers have no discoverable model list).</p>
        ) : (
          <ul>
            {discoverableProviders.map((provider) => {
              const result = discovery[provider.id];
              if (!result) {
                return null;
              }
              return (
                <li key={provider.id}>
                  <strong>{provider.name}</strong>{" "}
                  {result.status === "loading" ? (
                    "checking…"
                  ) : result.status === "error" ? (
                    <span className="validation-result validation-error">{result.message}</span>
                  ) : (
                    <span>
                      {result.models.map((model, index) => (
                        <span key={model}>
                          {index > 0 ? ", " : ""}
                          <code>{model}</code>
                          {poolIds.has(model) ? " (already a pool)" : ""}
                        </span>
                      ))}
                    </span>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </section>
    </section>
  );
}
