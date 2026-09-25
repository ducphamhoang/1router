import { FormEvent, useEffect, useRef, useState } from "react";
import { apiJson } from "../lib/apiClient";

type StartResponse = {
  authorize_url: string;
};

type DeviceStartResponse = {
  user_code: string;
  verification_url: string;
  interval: number;
};

type DeviceStatus =
  | { status: "not_started" | "pending" | "success" }
  | { status: "error"; error: string };

/**
 * Pull `code` and `state` out of the pasted localhost:1455 redirect URL, or
 * a bare `code=...&state=...` query (mirrors `onboarding::parse_code_and_state`).
 */
export function parseRedirect(
  input: string,
): { code: string; state: string } | null {
  const trimmed = input.trim();
  const query = trimmed.includes("?")
    ? trimmed.slice(trimmed.indexOf("?") + 1)
    : trimmed;
  const params = new URLSearchParams(query.split("#")[0]);
  const code = params.get("code");
  const state = params.get("state");
  return code && state ? { code, state } : null;
}

export function CodexOAuthPanel({
  providerId,
  connected = false,
}: {
  providerId: string;
  connected?: boolean;
}) {
  const [isConnected, setIsConnected] = useState(connected);
  // Once a login is on file the sign-in options are tucked behind "Log in
  // again" so they don't read as "you still need to do this".
  const [showLogin, setShowLogin] = useState(!connected);
  const [redirect, setRedirect] = useState("");
  const [device, setDevice] = useState<DeviceStartResponse | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const pollTimer = useRef<number | null>(null);

  function stopPolling() {
    if (pollTimer.current !== null) {
      window.clearInterval(pollTimer.current);
      pollTimer.current = null;
    }
  }

  useEffect(() => stopPolling, []);

  function markConnected() {
    setIsConnected(true);
    setShowLogin(false);
    setRedirect("");
    setMessage("Codex OAuth connected.");
  }

  function openLogin() {
    setMessage(null);
    setError(null);
    setShowLogin(true);
  }

  function cancelLogin() {
    stopPolling();
    setDevice(null);
    setError(null);
    setShowLogin(false);
  }

  async function startOAuth() {
    setError(null);
    const body = await apiJson<StartResponse>(
      `/admin/providers/${providerId}/oauth/start`,
      { method: "POST" },
    );
    window.open(body.authorize_url, "_blank", "noopener,noreferrer");
  }

  async function startDeviceLogin() {
    stopPolling();
    setError(null);
    setMessage(null);
    try {
      const body = await apiJson<DeviceStartResponse>(
        `/admin/providers/${providerId}/oauth/device/start`,
        {
          method: "POST",
        },
      );
      setDevice(body);
      const every = Math.max(body.interval, 2) * 1000;
      pollTimer.current = window.setInterval(() => void pollDevice(), every);
    } catch (error) {
      setError(
        error instanceof Error
          ? error.message
          : "Headless login failed to start.",
      );
    }
  }

  async function pollDevice() {
    try {
      const body = await apiJson<DeviceStatus>(
        `/admin/providers/${providerId}/oauth/device/status`,
      );
      if (body.status === "pending") {
        return;
      }
      stopPolling();
      setDevice(null);
      if (body.status === "success") {
        markConnected();
      } else if (body.status === "error") {
        setError(body.error);
      } else {
        setError("Headless login is no longer running; start it again.");
      }
    } catch {
      // Transient (network blip, server restart) - keep polling.
    }
  }

  async function completeOAuth(event: FormEvent) {
    // stopPropagation matters here, not just style: this form is nested
    // inside the provider form (invalid HTML, but React's DOM API builds it
    // that way regardless), so the native submit event bubbles to the outer
    // form's onSubmit too - without this, completing OAuth also submits
    // "Save provider" and closes the whole modal.
    event.preventDefault();
    event.stopPropagation();
    setError(null);
    setMessage(null);
    const parsed = parseRedirect(redirect);
    if (!parsed) {
      setError(
        "Couldn't find code and state in that — paste the whole localhost:1455 address.",
      );
      return;
    }
    try {
      await apiJson(`/admin/providers/${providerId}/oauth/complete`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(parsed),
      });
      markConnected();
    } catch (error) {
      setError(error instanceof Error ? error.message : "Codex OAuth failed.");
    }
  }

  return (
    <section aria-labelledby="codex-oauth-title">
      <h2 id="codex-oauth-title">Codex OAuth</h2>

      {isConnected && !showLogin ? (
        <>
          <p>
            A ChatGPT account is connected. Log in again only to switch accounts
            or if requests start failing.
          </p>
          <button type="button" onClick={openLogin}>
            Log in again
          </button>
        </>
      ) : null}

      {showLogin ? (
        <>
          {isConnected ? (
            <p>
              Logging in again replaces the current account.{" "}
              <button type="button" onClick={cancelLogin}>
                Cancel
              </button>
            </p>
          ) : null}
          <h3>Headless login (any device)</h3>
          <p>
            Get a one-time code, then sign in to ChatGPT on any phone or
            computer and enter it. Nothing needs to reach this server's
            localhost. If ChatGPT refuses the code, turn on device code login
            under ChatGPT Settings → Security.
          </p>
          <button type="button" onClick={() => void startDeviceLogin()}>
            {device ? "Get a new code" : "Start headless login"}
          </button>
          {device ? (
            <div role="group" aria-label="Device code">
              <p>
                Open{" "}
                <a
                  href={device.verification_url}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  {device.verification_url}
                </a>{" "}
                and enter:
              </p>
              <p>
                <code className="device-code">{device.user_code}</code>
              </p>
              <p>Waiting for approval… (the code expires in 15 minutes)</p>
            </div>
          ) : null}

          <h3>Browser login</h3>
          <p>
            After you approve access, your browser will show a page that fails
            to load at localhost:1455 — that's expected. Copy that page's whole
            address and paste it below.
          </p>
          <button type="button" onClick={startOAuth}>
            Start Codex OAuth
          </button>
          <form onSubmit={completeOAuth}>
            <label>
              Redirect URL
              <input
                value={redirect}
                placeholder="http://localhost:1455/auth/callback?code=…&state=…"
                onChange={(event) => setRedirect(event.target.value)}
              />
            </label>
            <button type="submit">Complete Codex OAuth</button>
          </form>
        </>
      ) : null}
      {message ? <p role="status">{message}</p> : null}
      {error ? <p role="alert">{error}</p> : null}
    </section>
  );
}
