import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CodexOAuthPanel, parseRedirect } from "./CodexOAuthPanel";

describe("parseRedirect", () => {
  it("accepts_a_full_redirect_url_or_a_bare_query", () => {
    expect(parseRedirect("http://localhost:1455/auth/callback?code=abc&scope=openid&state=st-9")).toEqual({
      code: "abc",
      state: "st-9"
    });
    expect(parseRedirect("  code=abc&state=st-9 ")).toEqual({ code: "abc", state: "st-9" });
    expect(parseRedirect("http://localhost:1455/auth/callback?code=abc")).toBeNull();
    expect(parseRedirect("")).toBeNull();
  });
});

describe("CodexOAuthPanel", () => {
  let deviceStatus = "pending";

  beforeEach(() => {
    deviceStatus = "pending";
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
        if (url === "/admin/providers/prov_1/oauth/device/start" && init?.method === "POST") {
          return new Response(
            JSON.stringify({
              user_code: "ABCD-1234",
              verification_url: "https://auth.openai.com/codex/device",
              interval: 5,
              expires_in: 900
            }),
            { status: 200 }
          );
        }
        if (url === "/admin/providers/prov_1/oauth/device/status") {
          return new Response(JSON.stringify({ status: deviceStatus }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
    vi.stubGlobal("open", vi.fn());
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("explains_the_localhost_error_page_before_start", () => {
    render(<CodexOAuthPanel providerId="prov_1" />);
    expect(screen.getByText(/fails to load at localhost:1455 — that's expected/)).toBeInTheDocument();
  });

  it("starts_oauth_and_opens_authorize_url", async () => {
    render(<CodexOAuthPanel providerId="prov_1" />);

    await userEvent.click(screen.getByRole("button", { name: "Start Codex OAuth" }));

    expect(fetch).toHaveBeenCalledWith("/admin/providers/prov_1/oauth/start", expect.objectContaining({ method: "POST" }));
    expect(window.open).toHaveBeenCalledWith("https://auth.example.test/start", "_blank", "noopener,noreferrer");
  });

  it("completes_oauth_from_a_pasted_redirect_url", async () => {
    render(<CodexOAuthPanel providerId="prov_1" />);

    await userEvent.type(
      screen.getByLabelText("Redirect URL"),
      "http://localhost:1455/auth/callback?code=abc123&scope=openid&state=csrf-state"
    );
    await userEvent.click(screen.getByRole("button", { name: "Complete Codex OAuth" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/providers/prov_1/oauth/complete",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ code: "abc123", state: "csrf-state" })
      })
    );
    expect(await screen.findByRole("status")).toHaveTextContent("Codex OAuth connected.");
  });

  it("rejects_a_paste_without_code_and_state", async () => {
    render(<CodexOAuthPanel providerId="prov_1" />);
    await userEvent.type(screen.getByLabelText("Redirect URL"), "http://localhost:1455/auth/callback");
    await userEvent.click(screen.getByRole("button", { name: "Complete Codex OAuth" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't find code and state");
    expect(fetch).not.toHaveBeenCalledWith("/admin/providers/prov_1/oauth/complete", expect.anything());
  });

  it("headless_login_shows_the_code_and_polls_until_connected", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<CodexOAuthPanel providerId="prov_1" />);

    await user.click(screen.getByRole("button", { name: "Start headless login" }));
    expect(await screen.findByText("ABCD-1234")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "https://auth.openai.com/codex/device" })).toBeInTheDocument();

    await vi.advanceTimersByTimeAsync(5000);
    expect(screen.queryByRole("status")).not.toBeInTheDocument();

    deviceStatus = "success";
    await vi.advanceTimersByTimeAsync(5000);
    expect(await screen.findByRole("status")).toHaveTextContent("Codex OAuth connected.");
    expect(screen.queryByText("ABCD-1234")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Start headless login" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Start Codex OAuth" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Log in again" })).toBeInTheDocument();
  });

  it("hides_login_options_when_already_connected_until_log_in_again", async () => {
    render(<CodexOAuthPanel providerId="prov_1" connected />);

    expect(screen.getByText(/A ChatGPT account is connected/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Start headless login" })).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Redirect URL")).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Log in again" }));
    expect(screen.getByRole("button", { name: "Start headless login" })).toBeInTheDocument();
    expect(screen.getByLabelText("Redirect URL")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("button", { name: "Start headless login" })).not.toBeInTheDocument();
  });
});
