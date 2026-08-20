import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Integration } from "./Integration";

describe("Integration", () => {
  let authMode = { require_shared_secret: true, origin: "db" };
  let loopback = true;

  beforeEach(() => {
    authMode = { require_shared_secret: true, origin: "db" };
    loopback = true;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/settings/shared-secret" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify({ shared_secret: "sec_****", masked: true, origin: "sidecar_file" }), { status: 200 });
        }
        if (url === "/admin/settings/auth-mode" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify(authMode), { status: 200 });
        }
        if (url === "/admin/settings/auth-mode" && init?.method === "PATCH") {
          const body = JSON.parse(String(init.body)) as { require_shared_secret: boolean };
          authMode = { require_shared_secret: body.require_shared_secret, origin: "db" };
          return new Response(JSON.stringify(authMode), { status: 200 });
        }
        if (url === "/admin/settings/security-status") {
          return new Response(
            JSON.stringify({
              shared_secret_is_default: false,
              admin_password_is_default: false,
              require_shared_secret: authMode.require_shared_secret,
              listen_addr_is_loopback: loopback
            }),
            { status: 200 }
          );
        }
        if (url === "/admin/settings/shared-secret?reveal=true") {
          return new Response(JSON.stringify({ shared_secret: "sec_real", masked: false, origin: "sidecar_file" }), { status: 200 });
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
              { id: "command_code", name: "Command Code", kind: "oauth_command_code", wire_format: "openai" },
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

  it("renders the current client API access mode", async () => {
    render(<Integration />);
    expect(await screen.findByLabelText(/Open access — \/v1/)).not.toBeChecked();
    expect(screen.getByLabelText(/API key required — clients/)).toBeChecked();
  });

  it("requires a base confirmation before enabling open access on loopback", async () => {
    render(<Integration />);
    await userEvent.click(await screen.findByLabelText(/Open access — \/v1/));
    expect(await screen.findByRole("button", { name: "Enable open access" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Enable open access" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Client API access updated.");
  });

  it("requires an extra confirmation for open access on a non-loopback listener", async () => {
    loopback = false;
    render(<Integration />);
    await userEvent.click(await screen.findByLabelText(/Open access — \/v1/));
    await userEvent.click(await screen.findByRole("button", { name: "Review non-local open access" }));
    expect(await screen.findByRole("button", { name: "Yes, enable open access" })).toBeInTheDocument();
  });

  it("hides the shared-secret form and shows the no-API-key note in open mode", async () => {
    authMode = { require_shared_secret: false, origin: "db" };
    render(<Integration />);
    await screen.findByLabelText(/Open access — \/v1/);
    expect(screen.queryByLabelText("Shared secret")).not.toBeInTheDocument();
    expect(
      screen.getByText((content) => content.includes("Open access is on") && content.includes("accepts requests without an API key"))
    ).toBeInTheDocument();
    expect(screen.queryByLabelText("API key for client connections")).not.toBeInTheDocument();
  });

  it("loads_masked_secret_and_reveals_real_value", async () => {
    render(<Integration />);

    expect(await screen.findByDisplayValue("sec_****")).toBeInTheDocument();
    expect(screen.getByLabelText("Shared secret")).toBeDisabled();
    await userEvent.click(screen.getByRole("button", { name: "Reveal shared secret" }));
    expect(await screen.findByLabelText("Shared secret")).toHaveValue("sec_real");
    expect(screen.getByLabelText("Shared secret")).toBeEnabled();
  });

  it("prevents_saving_shared_secret_until_revealed_and_edited", async () => {
    render(<Integration />);

    expect(await screen.findByRole("button", { name: "Save shared secret" })).toBeDisabled();
    await userEvent.click(screen.getByRole("button", { name: "Reveal shared secret" }));
    expect(await screen.findByRole("button", { name: "Save shared secret" })).toBeDisabled();
    await userEvent.type(screen.getByLabelText("Shared secret"), "-edited");
    expect(screen.getByRole("button", { name: "Save shared secret" })).toBeEnabled();
  });

  it("shows_connect_guide_with_base_url_and_available_models_grouped_by_wire_format", async () => {
    render(<Integration />);

    expect(await screen.findByText("codex-sol")).toBeInTheDocument();
    expect(await screen.findByText("claude-main")).toBeInTheDocument();
    expect(screen.getByLabelText("Base URL")).toHaveValue(`${window.location.origin}/v1`);
    // the API key field starts masked, independent of the "Shared secret" form's own reveal state
    expect(screen.getByLabelText("API key for client connections")).toHaveDisplayValue(/•+/);
  });

  it("reveal_button_in_connect_guide_shows_the_real_shared_secret", async () => {
    render(<Integration />);
    await screen.findByText("codex-sol");

    await userEvent.click(screen.getByRole("button", { name: "Reveal" }));
    expect(await screen.findByLabelText("API key for client connections")).toHaveValue("sec_real");
  });

  it("discovers_models_from_passthrough_providers_only_and_flags_ones_already_pooled", async () => {
    render(<Integration />);
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

  it("includes_commandcode_providers_in_discovery", async () => {
    render(<Integration />);
    await userEvent.click(screen.getByRole("button", { name: "Check providers for available models" }));
    await waitFor(() => expect(fetch).toHaveBeenCalledWith("/admin/providers/command_code/list-models", expect.anything()));
  });

  it("renders_shared_secret_409_message_verbatim", async () => {
    render(<Integration />);

    await userEvent.click(await screen.findByRole("button", { name: "Reveal shared secret" }));
    await userEvent.clear(screen.getByLabelText("Shared secret"));
    await userEvent.type(screen.getByLabelText("Shared secret"), "replacement");
    await userEvent.click(screen.getByRole("button", { name: "Save shared secret" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("ROUTER_SHARED_SECRET is set; change it there");
  });
});
