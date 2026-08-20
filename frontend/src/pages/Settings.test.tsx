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
        if (url === "/admin/auth/password" && init?.method === "PATCH") {
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
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

  it("surfaces_an_error_when_password_change_fails", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ error: { message: "wrong current password" } }), { status: 400 }))
    );
    render(<Settings />);

    await userEvent.type(screen.getByLabelText("Current password"), "wrong");
    await userEvent.type(screen.getByLabelText("New password"), "new-secret");
    await userEvent.click(screen.getByRole("button", { name: "Change password" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("wrong current password");
  });

  it("does not render integration content", () => {
    render(<Settings />);
    expect(screen.queryByText("Client API access")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("API key for client connections")).not.toBeInTheDocument();
  });
});
