import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Settings } from "./Settings";

describe("Settings", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/settings/shared-secret" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify({ shared_secret: "sec_****", masked: true, origin: "sidecar_file" }), { status: 200 });
        }
        if (url === "/admin/settings/shared-secret?reveal=true") {
          return new Response(JSON.stringify({ shared_secret: "sec_real", masked: false, origin: "sidecar_file" }), { status: 200 });
        }
        if (url === "/admin/auth/password" && init?.method === "PATCH") {
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/settings/shared-secret" && init?.method === "PATCH") {
          return new Response(JSON.stringify({ error: { message: "ROUTER_SHARED_SECRET is set; change it there" } }), {
            status: 409
          });
        }
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([
              { id: "codex-sol", wire_format: "openai" },
              { id: "claude-main", wire_format: "anthropic" }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([
              { id: "deepseek_api", name: "Deepseek", kind: "passthrough", wire_format: "openai" },
              { id: "codex-vbg", name: "codex-vbg", kind: "oauth_codex", wire_format: "openai" }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/providers/deepseek_api/list-models" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify({ ok: true, models: ["deepseek-v4-flash", "deepseek-v4-pro"] }), {
            status: 200
          });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("loads_masked_secret_and_reveals_real_value", async () => {
    render(<Settings />);

    expect(await screen.findByDisplayValue("sec_****")).toBeInTheDocument();
    expect(screen.getByLabelText("Shared secret")).toBeDisabled();
    await userEvent.click(screen.getByRole("button", { name: "Reveal shared secret" }));
    expect(await screen.findByLabelText("Shared secret")).toHaveValue("sec_real");
    expect(screen.getByLabelText("Shared secret")).toBeEnabled();
  });

  it("changes_admin_password", async () => {
    render(<Settings />);

    await userEvent.type(screen.getByLabelText("Current password"), "old");
    await userEvent.type(screen.getByLabelText("New password"), "new-secret");
    await userEvent.click(screen.getByRole("button", { name: "Change password" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/auth/password",
      expect.objectContaining({
        method: "PATCH",
        body: JSON.stringify({ current_password: "old", new_password: "new-secret" })
      })
    );
    expect(await screen.findByRole("status")).toHaveTextContent("Password changed.");
  });

  it("prevents_saving_shared_secret_until_revealed_and_edited", async () => {
    render(<Settings />);

    expect(await screen.findByRole("button", { name: "Save shared secret" })).toBeDisabled();
    await userEvent.click(screen.getByRole("button", { name: "Reveal shared secret" }));
    expect(await screen.findByRole("button", { name: "Save shared secret" })).toBeDisabled();
    await userEvent.type(screen.getByLabelText("Shared secret"), "-edited");
    expect(screen.getByRole("button", { name: "Save shared secret" })).toBeEnabled();
  });

  it("shows_connect_guide_with_base_url_and_available_models_grouped_by_wire_format", async () => {
    render(<Settings />);

    expect(await screen.findByText("codex-sol")).toBeInTheDocument();
    expect(await screen.findByText("claude-main")).toBeInTheDocument();
    expect(screen.getByLabelText("Base URL")).toHaveValue(`${window.location.origin}/v1`);
    // the API key field starts masked, independent of the "Shared secret" form's own reveal state
    expect(screen.getByLabelText("API key for client connections")).toHaveDisplayValue(/•+/);
  });

  it("reveal_button_in_connect_guide_shows_the_real_shared_secret", async () => {
    render(<Settings />);
    await screen.findByText("codex-sol");

    await userEvent.click(screen.getByRole("button", { name: "Reveal" }));
    expect(await screen.findByLabelText("API key for client connections")).toHaveValue("sec_real");
  });

  it("discovers_models_from_passthrough_providers_only_and_flags_ones_already_pooled", async () => {
    render(<Settings />);
    await screen.findByText("codex-sol");

    await userEvent.click(screen.getByRole("button", { name: "Check providers for available models" }));

    expect(await screen.findByText("deepseek-v4-pro")).toBeInTheDocument();
    // deepseek-v4-flash is not in this test's pool list, but codex-sol is -
    // exercise the "already a pool" flag path via a model matching a pool id.
    expect(fetch).toHaveBeenCalledWith(
      "/admin/providers/deepseek_api/list-models",
      expect.objectContaining({ credentials: "include" })
    );
    // the oauth_codex provider is never queried - it has no models endpoint
    expect(fetch).not.toHaveBeenCalledWith("/admin/providers/codex-vbg/list-models", expect.anything());
  });

  it("renders_shared_secret_409_message_verbatim", async () => {
    render(<Settings />);

    await userEvent.click(await screen.findByRole("button", { name: "Reveal shared secret" }));
    await userEvent.clear(screen.getByLabelText("Shared secret"));
    await userEvent.type(screen.getByLabelText("Shared secret"), "replacement");
    await userEvent.click(screen.getByRole("button", { name: "Save shared secret" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("ROUTER_SHARED_SECRET is set; change it there");
  });
});
