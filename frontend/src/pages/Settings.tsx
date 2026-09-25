import { FormEvent, useEffect, useState } from "react";
import { apiJson } from "../lib/apiClient";

export function Settings() {
  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // null until loaded: the toggle stays disabled rather than guessing.
  const [imagesEnabled, setImagesEnabled] = useState<boolean | null>(null);

  useEffect(() => {
    void apiJson<{ images_enabled: boolean }>("/admin/settings/images")
      .then((body) => setImagesEnabled(body.images_enabled))
      .catch(() => undefined);
  }, []);

  async function toggleImages(value: boolean) {
    setMessage(null);
    setError(null);
    try {
      const body = await apiJson<{ images_enabled: boolean }>("/admin/settings/images", {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ images_enabled: value })
      });
      setImagesEnabled(body.images_enabled);
      setMessage(body.images_enabled ? "Image generation enabled." : "Image generation disabled.");
    } catch (error) {
      setError(error instanceof Error ? error.message : "Image setting update failed.");
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

      <h2>Image generation</h2>
      <label className="checkbox-row">
        <input
          type="checkbox"
          checked={imagesEnabled ?? false}
          disabled={imagesEnabled === null}
          onChange={(event) => void toggleImages(event.target.checked)}
        />
        Enable <code>POST /v1/images/generations</code> (served by image pools with Codex providers)
      </label>
    </section>
  );
}
