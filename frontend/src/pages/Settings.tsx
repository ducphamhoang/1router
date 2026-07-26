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

export function Settings() {
  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [sharedSecret, setSharedSecret] = useState("");
  const [sharedSecretRevealed, setSharedSecretRevealed] = useState(false);
  const [sharedSecretEdited, setSharedSecretEdited] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function loadSharedSecret(reveal = false) {
    const suffix = reveal ? "?reveal=true" : "";
    const body = await apiJson<SharedSecretResponse>(`/admin/settings/shared-secret${suffix}`);
    setSharedSecret(body.shared_secret);
    setSharedSecretRevealed(!body.masked);
    setSharedSecretEdited(false);
  }

  useEffect(() => {
    void loadSharedSecret(false);
  }, []);

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
    </section>
  );
}
