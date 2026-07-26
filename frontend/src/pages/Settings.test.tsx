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
          return new Response(JSON.stringify({ shared_secret: "sec_****" }), { status: 200 });
        }
        if (url === "/admin/settings/shared-secret?reveal=true") {
          return new Response(JSON.stringify({ shared_secret: "sec_real" }), { status: 200 });
        }
        if (url === "/admin/auth/password" && init?.method === "PATCH") {
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/settings/shared-secret" && init?.method === "PATCH") {
          return new Response(JSON.stringify({ error: { message: "ROUTER_SHARED_SECRET is set; change it there" } }), {
            status: 409
          });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("loads_masked_secret_and_reveals_real_value", async () => {
    render(<Settings />);

    expect(await screen.findByDisplayValue("sec_****")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Reveal shared secret" }));
    expect(await screen.findByDisplayValue("sec_real")).toBeInTheDocument();
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

  it("renders_shared_secret_409_message_verbatim", async () => {
    render(<Settings />);

    await userEvent.clear(await screen.findByLabelText("Shared secret"));
    await userEvent.type(screen.getByLabelText("Shared secret"), "replacement");
    await userEvent.click(screen.getByRole("button", { name: "Save shared secret" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("ROUTER_SHARED_SECRET is set; change it there");
  });
});
