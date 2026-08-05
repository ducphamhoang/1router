import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CommandCodeKeyPanel } from "./CommandCodeKeyPanel";

describe("CommandCodeKeyPanel", () => {
  beforeEach(() => {
    vi.stubGlobal("open", vi.fn());
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("validates_a_pasted_api_key_and_reports_discovered_models", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/commandcode/key" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/list-models") {
          return new Response(JSON.stringify({ ok: true, models: ["cc-model-a", "cc-model-b"] }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/validate-model" && init?.method === "POST") {
          expect(JSON.parse(String(init.body))).toEqual({ model: "cc-model-a" });
          return new Response(JSON.stringify({ ok: true, status: 200 }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
    const onCredentialSaved = vi.fn();

    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={false} onCredentialSaved={onCredentialSaved} />);

    await userEvent.type(screen.getByLabelText("API key"), "cc-secret");
    await userEvent.click(screen.getByRole("button", { name: "Validate key" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/providers/prov_1/commandcode/key",
      expect.objectContaining({ method: "POST", body: JSON.stringify({ api_key: "cc-secret" }) })
    );
    expect(await screen.findByRole("status")).toHaveTextContent("Command Code key validated (tested against cc-model-a).");
    expect(onCredentialSaved).toHaveBeenCalledWith(["cc-model-a", "cc-model-b"]);
  });

  it("probes_the_cheapest-looking_model_first_and_falls_back_past_a_plan-gated_one", async () => {
    const attemptedModels: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/commandcode/key" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/list-models") {
          // Raw order puts the plan-gated flagship first - the panel should
          // still probe (and prefer) the cheap-looking one first.
          return new Response(JSON.stringify({ ok: true, models: ["claude-sonnet-5", "deepseek-v3"] }), {
            status: 200
          });
        }
        if (url === "/admin/providers/prov_1/validate-model" && init?.method === "POST") {
          const { model } = JSON.parse(String(init.body));
          attemptedModels.push(model);
          if (model === "deepseek-v3") {
            return new Response(JSON.stringify({ ok: true, status: 200 }), { status: 200 });
          }
          return new Response(JSON.stringify({ ok: false, message: "MODEL_NOT_IN_PLAN" }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
    const onCredentialSaved = vi.fn();

    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={false} onCredentialSaved={onCredentialSaved} />);

    await userEvent.type(screen.getByLabelText("API key"), "cc-secret");
    await userEvent.click(screen.getByRole("button", { name: "Validate key" }));

    expect(await screen.findByRole("status")).toHaveTextContent(
      "Command Code key validated (tested against deepseek-v3)."
    );
    expect(attemptedModels).toEqual(["deepseek-v3"]);
    expect(onCredentialSaved).toHaveBeenCalledWith(["deepseek-v3", "claude-sonnet-5"]);
  });

  it("surfaces_the_last_failure_when_every_candidate_model_fails_validation", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/commandcode/key" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/list-models") {
          return new Response(JSON.stringify({ ok: true, models: ["claude-sonnet-5", "claude-opus-5"] }), {
            status: 200
          });
        }
        if (url === "/admin/providers/prov_1/validate-model" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: false, message: "MODEL_NOT_IN_PLAN" }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={false} onCredentialSaved={vi.fn()} />);

    await userEvent.type(screen.getByLabelText("API key"), "cc-secret");
    await userEvent.click(screen.getByRole("button", { name: "Validate key" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("MODEL_NOT_IN_PLAN");
  });

  it("surfaces_an_error_when_the_key_fails_real_validation", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/commandcode/key" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/list-models") {
          return new Response(JSON.stringify({ ok: true, models: ["cc-large"] }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/validate-model" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: false, message: "HTTP 401: unauthorized" }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={false} onCredentialSaved={vi.fn()} />);

    await userEvent.type(screen.getByLabelText("API key"), "bad-key");
    await userEvent.click(screen.getByRole("button", { name: "Validate key" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("HTTP 401: unauthorized");
  });

  it("disables_the_validate_button_until_a_key_is_typed", () => {
    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={false} onCredentialSaved={vi.fn()} />);
    expect(screen.getByRole("button", { name: "Validate key" })).toBeDisabled();
  });

  it("logs_in_with_browser_and_polls_until_success", async () => {
    let statusCalls = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/commandcode/browser-login/start" && init?.method === "POST") {
          return new Response(
            JSON.stringify({ authorize_url: "https://commandcode.ai/studio/auth/cli?callback=x&state=y" }),
            { status: 200 }
          );
        }
        if (url === "/admin/providers/prov_1/commandcode/browser-login/status") {
          statusCalls += 1;
          const status = statusCalls < 2 ? "pending" : "success";
          return new Response(JSON.stringify({ status }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/list-models") {
          return new Response(JSON.stringify({ ok: true, models: ["cc-large"] }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
    const onCredentialSaved = vi.fn();

    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={false} onCredentialSaved={onCredentialSaved} />);

    await userEvent.click(screen.getByRole("button", { name: "Login with browser" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/providers/prov_1/commandcode/browser-login/start",
      expect.objectContaining({ method: "POST" })
    );
    expect(window.open).toHaveBeenCalledWith(
      "https://commandcode.ai/studio/auth/cli?callback=x&state=y",
      "_blank",
      "noopener,noreferrer"
    );
    expect(screen.getByRole("button", { name: "Waiting for login..." })).toBeDisabled();

    expect(await screen.findByText("Command Code connected.", {}, { timeout: 4000 })).toBeInTheDocument();
    expect(onCredentialSaved).toHaveBeenCalledWith(["cc-large"]);
  }, 10000);

  it("surfaces_a_login_error_from_the_status_poll", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/commandcode/browser-login/start" && init?.method === "POST") {
          return new Response(JSON.stringify({ authorize_url: "https://commandcode.ai/studio/auth/cli" }), {
            status: 200
          });
        }
        if (url === "/admin/providers/prov_1/commandcode/browser-login/status") {
          return new Response(JSON.stringify({ status: "error", error: "login denied: user cancelled" }), {
            status: 200
          });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={false} onCredentialSaved={vi.fn()} />);

    await userEvent.click(screen.getByRole("button", { name: "Login with browser" }));

    expect(await screen.findByRole("alert", {}, { timeout: 4000 })).toHaveTextContent(
      "login denied: user cancelled"
    );
    expect(screen.getByRole("button", { name: "Login with browser" })).not.toBeDisabled();
  }, 10000);

  it("shows_an_existing_credential_notice_and_discovers_models_on_mount", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/list-models") {
          return new Response(JSON.stringify({ ok: true, models: ["cc-large"] }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
    const onCredentialSaved = vi.fn();

    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={true} onCredentialSaved={onCredentialSaved} />);

    expect(
      screen.getByText("A Command Code API key is already saved. Logging in or validating a new key below replaces it.")
    ).toBeInTheDocument();
    await vi.waitFor(() => expect(onCredentialSaved).toHaveBeenCalledWith(["cc-large"]));
  });

  it("shows_a_masked_placeholder_for_an_existing_key_and_revalidates_it_without_resubmitting", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers/prov_1/commandcode/key") {
          throw new Error("must not re-save the key when the placeholder is left untouched");
        }
        if (url === "/admin/providers/prov_1/list-models") {
          return new Response(JSON.stringify({ ok: true, models: ["cc-large"] }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/validate-model" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true, status: 200 }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={true} onCredentialSaved={vi.fn()} />);

    const input = screen.getByLabelText("API key") as HTMLInputElement;
    expect(input.value).toBe("••••••••••••");
    expect(screen.getByRole("button", { name: "Validate key" })).not.toBeDisabled();

    await userEvent.click(screen.getByRole("button", { name: "Validate key" }));

    expect(await screen.findByRole("status")).toHaveTextContent("Command Code key validated (tested against cc-large).");
  });

  it("clears_the_placeholder_on_focus_so_a_new_key_can_be_typed", async () => {
    render(<CommandCodeKeyPanel providerId="prov_1" hasCredential={true} onCredentialSaved={vi.fn()} />);

    const input = screen.getByLabelText("API key") as HTMLInputElement;
    expect(input.value).toBe("••••••••••••");

    await userEvent.click(input);
    expect(input.value).toBe("");

    await userEvent.type(input, "new-secret");
    expect(input.value).toBe("new-secret");
  });
});
