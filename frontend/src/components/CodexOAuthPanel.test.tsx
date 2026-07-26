import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CodexOAuthPanel } from "./CodexOAuthPanel";

describe("CodexOAuthPanel", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/oauth/start" && init?.method === "POST") {
          return new Response(JSON.stringify({ authorize_url: "https://auth.example.test/start" }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/oauth/complete" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
    vi.stubGlobal("open", vi.fn());
  });

  it("shows_localhost_connection_error_copy_before_start", () => {
    render(<CodexOAuthPanel providerId="prov_1" />);

    expect(
      screen.getByText(
        "After you approve access, your browser will show a page that fails to load at localhost:1455 — that's expected. Copy the code value out of that page's address bar and paste it below."
      )
    ).toBeInTheDocument();
  });

  it("starts_oauth_and_opens_authorize_url", async () => {
    render(<CodexOAuthPanel providerId="prov_1" />);

    await userEvent.click(screen.getByRole("button", { name: "Start Codex OAuth" }));

    expect(fetch).toHaveBeenCalledWith("/admin/providers/prov_1/oauth/start", expect.objectContaining({ method: "POST" }));
    expect(window.open).toHaveBeenCalledWith("https://auth.example.test/start", "_blank", "noopener,noreferrer");
  });

  it("completes_oauth_with_pasted_code", async () => {
    render(<CodexOAuthPanel providerId="prov_1" />);

    await userEvent.type(screen.getByLabelText("Code"), "abc123");
    await userEvent.click(screen.getByRole("button", { name: "Complete Codex OAuth" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/providers/prov_1/oauth/complete",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ code: "abc123" })
      })
    );
    expect(await screen.findByRole("status")).toHaveTextContent("Codex OAuth connected.");
  });
});
