import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Providers } from "./Providers";

const providers = [
  {
    id: "prov_1",
    name: "openai",
    wire_format: "openai",
    kind: "passthrough",
    base_url: "https://api.openai.com/v1",
    api_key: "sk-***",
    upstream_model: "gpt-4.1"
  }
];

describe("Providers", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify(providers), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/state") {
          return new Response(JSON.stringify({ provider_id: "prov_1", backoff_level: 0, status: "healthy", unavailable_in_secs: null }), { status: 200 });
        }
        if (url === "/admin/providers" && init?.method === "POST") {
          const sent = JSON.parse(String(init.body));
          return new Response(JSON.stringify({ ...providers[0], ...sent, id: sent.id ?? "prov_2" }), { status: 200 });
        }
        if (url === "/admin/pools" && init?.method === "POST") {
          return new Response(JSON.stringify({ id: "prov_2", wire_format: "anthropic" }), { status: 201 });
        }
        if (url === "/admin/pools/prov_2/members" && init?.method === "PUT") {
          return new Response(JSON.stringify({ pool_id: "prov_2", provider_id: "prov_2", priority: 1 }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1" && init?.method === "PATCH") {
          return new Response(JSON.stringify({ ...providers[0], upstream_model: "gpt-4.1-mini" }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1" && init?.method === "DELETE") {
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/providers/prov_1/validate-model" && init?.method === "POST") {
          const body = JSON.parse(String(init.body));
          return body.model === "not-a-real-model"
            ? new Response(JSON.stringify({ ok: false, status: 404, message: "model not found" }), { status: 200 })
            : new Response(JSON.stringify({ ok: true, status: 200 }), { status: 200 });
        }
        if (url === "/admin/providers/command-code/commandcode/key" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        if (url === "/admin/providers/command-code/list-models") {
          return new Response(JSON.stringify({ ok: true, models: ["cc-model-a", "cc-model-b"] }), { status: 200 });
        }
        if (url === "/admin/providers/command-code/validate-model" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true, status: 200 }), { status: 200 });
        }
        if (url === "/admin/providers/command-code" && init?.method === "PATCH") {
          const sent = JSON.parse(String(init.body));
          return new Response(JSON.stringify({ ...providers[0], id: "command-code", ...sent }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("lists_providers_and_polls_state_badges", async () => {
    render(<Providers />);

    expect(await screen.findByText("gpt-4.1")).toBeInTheDocument();
    expect(await screen.findByText("healthy")).toBeInTheDocument();
    expect(fetch).toHaveBeenCalledWith("/admin/providers/prov_1/state", expect.objectContaining({ credentials: "include" }));
  });

  it("creates_provider", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));
    await userEvent.type(screen.getByLabelText("Provider ID"), "prov_2");
    await userEvent.type(screen.getByLabelText("Name"), "anthropic");
    await userEvent.selectOptions(screen.getByLabelText("API format"), "anthropic");
    await userEvent.selectOptions(screen.getByLabelText("Kind"), "passthrough");
    await userEvent.type(screen.getByLabelText("Base URL"), "https://api.anthropic.com");
    await userEvent.type(screen.getByLabelText("API key"), "secret");
    await userEvent.type(screen.getByLabelText("Upstream model"), "claude-sonnet-4");
    await userEvent.click(screen.getByRole("button", { name: "Save provider" }));

    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith(
        "/admin/providers",
        expect.objectContaining({
          method: "POST",
          body: expect.stringContaining("\"id\":\"prov_2\"")
        })
      )
    );
  });

  it("offers_commandcode_kind_in_the_dropdown", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));
    expect(screen.getByRole("option", { name: "OAuth (Command Code)" })).toBeInTheDocument();
  });

  it("choosing_a_template_prefills_wire_format_base_url_and_model_but_stays_editable", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

    await userEvent.selectOptions(screen.getByLabelText(/Template/), "DeepSeek (Anthropic-compatible)");

    expect(screen.getByLabelText("Kind")).toHaveValue("passthrough");
    expect(screen.getByLabelText("API format")).toHaveValue("anthropic");
    expect(screen.getByLabelText("Base URL")).toHaveValue("https://api.deepseek.com/anthropic/v1/messages");
    expect(screen.getByLabelText("Upstream model")).toHaveValue("deepseek-flash");
    expect(screen.getByLabelText("Provider ID")).toHaveValue("deepseek-anthropic");
    expect(screen.getByLabelText("Name")).toHaveValue("DeepSeek (Anthropic-compatible)");

    // still fully editable after a template is applied
    await userEvent.clear(screen.getByLabelText("Base URL"));
    await userEvent.type(screen.getByLabelText("Base URL"), "https://my-mirror.example.com/v1/messages");
    expect(screen.getByLabelText("Base URL")).toHaveValue("https://my-mirror.example.com/v1/messages");

    // and typing a custom id afterward isn't clobbered by re-picking a template
    await userEvent.clear(screen.getByLabelText("Provider ID"));
    await userEvent.type(screen.getByLabelText("Provider ID"), "my-deepseek");
    await userEvent.selectOptions(screen.getByLabelText(/Template/), "DeepSeek (OpenAI-compatible)");
    expect(screen.getByLabelText("Provider ID")).toHaveValue("my-deepseek");
  });

  it("suggests_a_numbered_id_when_the_templates_default_id_is_already_taken", async () => {
    // The fixture's only provider has id "prov_1" (not "openai") so this
    // exercises the collision path with a second provider whose id really
    // does match the OpenAI template's suggestedId.
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([...providers, { ...providers[0], id: "openai", name: "openai" }]), {
            status: 200
          });
        }
        if (url === "/admin/providers/openai/state") {
          return new Response(JSON.stringify({ provider_id: "openai", backoff_level: 0, status: "healthy", unavailable_in_secs: null }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1/state") {
          return new Response(JSON.stringify({ provider_id: "prov_1", backoff_level: 0, status: "healthy", unavailable_in_secs: null }), { status: 200 });
        }
        throw new Error(`unexpected fetch: ${url}`);
      })
    );

    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));
    await userEvent.selectOptions(screen.getByLabelText(/Template/), "OpenAI");

    expect(screen.getByLabelText("Provider ID")).toHaveValue("openai-2");
  });

  it("switching_back_to_custom_clears_the_previous_templates_fields", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

    await userEvent.selectOptions(screen.getByLabelText(/Template/), "OpenCode Free");
    expect(screen.getByLabelText("Base URL")).toHaveValue("https://opencode.ai/zen/v1/chat/completions");
    expect(screen.getByLabelText("API key")).toHaveValue("public");

    await userEvent.selectOptions(screen.getByLabelText(/Template/), "Custom");

    expect(screen.getByLabelText("Base URL")).toHaveValue("");
    expect(screen.getByLabelText("Upstream model")).toHaveValue("");
    expect(screen.getByLabelText("API key")).toHaveValue("");
    expect(screen.getByLabelText("Kind")).toHaveValue("passthrough");
    expect(screen.getByLabelText("API format")).toHaveValue("openai");
  });

  it("choosing_the_opencode_openai_template_prefills_its_fields", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

    await userEvent.selectOptions(screen.getByLabelText(/Template/), "OpenCode (OpenAI-compatible)");

    expect(screen.getByLabelText("Kind")).toHaveValue("passthrough");
    expect(screen.getByLabelText("API format")).toHaveValue("openai");
    expect(screen.getByLabelText("Base URL")).toHaveValue("https://opencode.ai/zen/go/v1/chat/completions");
    expect(screen.getByLabelText("Upstream model")).toHaveValue("kimi-k2.7-code");
  });

  it("choosing_the_opencode_anthropic_template_prefills_its_fields", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

    await userEvent.selectOptions(screen.getByLabelText(/Template/), "OpenCode (Anthropic-compatible)");

    expect(screen.getByLabelText("Kind")).toHaveValue("passthrough");
    expect(screen.getByLabelText("API format")).toHaveValue("anthropic");
    expect(screen.getByLabelText("Base URL")).toHaveValue("https://opencode.ai/zen/go/v1/messages");
    expect(screen.getByLabelText("Upstream model")).toHaveValue("qwen3.7-max");
  });

  it("choosing_the_opencode_free_template_prefills_the_public_api_key", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

    await userEvent.selectOptions(screen.getByLabelText(/Template/), "OpenCode Free");

    expect(screen.getByLabelText("Kind")).toHaveValue("passthrough");
    expect(screen.getByLabelText("API format")).toHaveValue("openai");
    expect(screen.getByLabelText("Base URL")).toHaveValue("https://opencode.ai/zen/v1/chat/completions");
    expect(screen.getByLabelText("Upstream model")).toHaveValue("deepseek-v4-flash-free");
    expect(screen.getByLabelText("API key")).toHaveValue("public");
  });

  it("choosing_the_gemini_template_prefills_its_fields", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

    await userEvent.selectOptions(screen.getByLabelText(/Template/), "Gemini (OpenAI-compatible)");

    expect(screen.getByLabelText("Kind")).toHaveValue("passthrough");
    expect(screen.getByLabelText("API format")).toHaveValue("openai");
    expect(screen.getByLabelText("Base URL")).toHaveValue(
      "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
    );
    expect(screen.getByLabelText("Upstream model")).toHaveValue("gemini-2.5-flash");
  });

  it("choosing_the_command_code_template_selects_its_kind_and_hides_passthrough_only_fields", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

    await userEvent.selectOptions(screen.getByLabelText(/Template/), "Command Code");

    expect(screen.getByLabelText("Kind")).toHaveValue("oauth_command_code");
    expect(screen.getByLabelText("Provider ID")).toHaveValue("command-code");
    expect(screen.getByLabelText("Name")).toHaveValue("Command Code");
    expect(screen.queryByLabelText("API format")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Base URL")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("API key")).not.toBeInTheDocument();
  });

  it("creating_an_oauth_kind_provider_flips_the_modal_into_edit_mode_for_the_connect_step", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));
    await userEvent.selectOptions(screen.getByLabelText(/Template/), "Command Code");
    // Skip pool auto-creation here - it's covered by its own tests, and this
    // test only cares about the create -> edit-mode transition.
    await userEvent.click(screen.getByLabelText(/Make it directly callable/));
    await userEvent.click(screen.getByRole("button", { name: "Save provider" }));

    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith(
        "/admin/providers",
        expect.objectContaining({ method: "POST", body: expect.stringContaining("\"kind\":\"oauth_command_code\"") })
      )
    );
    // still open, now in edit mode with the validate-key panel visible
    expect(await screen.findByRole("button", { name: "Validate key" })).toBeInTheDocument();
    expect(screen.queryByLabelText("Provider ID")).not.toBeInTheDocument();
  });

  it("command_code_upstream_model_becomes_a_populated_select_after_the_key_is_validated", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));
    await userEvent.selectOptions(screen.getByLabelText(/Template/), "Command Code");
    await userEvent.click(screen.getByLabelText(/Make it directly callable/));
    await userEvent.click(screen.getByRole("button", { name: "Save provider" }));

    expect(await screen.findByRole("button", { name: "Validate key" })).toBeInTheDocument();
    expect(screen.getByLabelText("Upstream model")).toBeDisabled();
    expect(screen.getByText("Log in or paste an API key above to fetch the model list.")).toBeInTheDocument();

    await userEvent.type(screen.getByLabelText("API key"), "cc-secret");
    await userEvent.click(screen.getByRole("button", { name: "Validate key" }));

    await waitFor(() => expect(screen.getByLabelText("Upstream model")).not.toBeDisabled());
    expect(screen.getByLabelText("Upstream model")).toHaveValue("cc-model-a");
    expect(screen.getByRole("option", { name: "cc-model-b" })).toBeInTheDocument();

    await userEvent.selectOptions(screen.getByLabelText("Upstream model"), "cc-model-b");
    await userEvent.click(screen.getByRole("button", { name: "Save provider" }));

    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith(
        "/admin/providers/command-code",
        expect.objectContaining({ method: "PATCH", body: expect.stringContaining("\"upstream_model\":\"cc-model-b\"") })
      )
    );
  });

  it("exposes_the_new_provider_as_a_matching_1_member_pool_by_default", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));
    await userEvent.type(screen.getByLabelText("Provider ID"), "prov_2");
    await userEvent.type(screen.getByLabelText("Name"), "anthropic");
    await userEvent.selectOptions(screen.getByLabelText("API format"), "anthropic");
    await userEvent.type(screen.getByLabelText("Base URL"), "https://api.anthropic.com");
    await userEvent.type(screen.getByLabelText("API key"), "secret");
    await userEvent.type(screen.getByLabelText("Upstream model"), "claude-sonnet-4");
    await userEvent.click(screen.getByRole("button", { name: "Save provider" }));

    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools",
        expect.objectContaining({ method: "POST", body: JSON.stringify({ id: "prov_2", wire_format: "anthropic" }) })
      )
    );
    expect(fetch).toHaveBeenCalledWith(
      "/admin/pools/prov_2/members",
      expect.objectContaining({ method: "PUT", body: JSON.stringify({ provider_id: "prov_2", priority: 1 }) })
    );
  });

  it("skips_pool_creation_when_the_checkbox_is_unchecked", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));
    await userEvent.type(screen.getByLabelText("Provider ID"), "prov_2");
    await userEvent.type(screen.getByLabelText("Name"), "anthropic");
    await userEvent.click(screen.getByLabelText(/Make it directly callable/));
    await userEvent.click(screen.getByRole("button", { name: "Save provider" }));

    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith("/admin/providers", expect.objectContaining({ method: "POST" }))
    );
    expect(fetch).not.toHaveBeenCalledWith("/admin/pools", expect.anything());
  });

  it("edits_and_deletes_provider", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "Edit openai" }));
    await userEvent.clear(screen.getByLabelText("Upstream model"));
    await userEvent.type(screen.getByLabelText("Upstream model"), "gpt-4.1-mini");
    await userEvent.click(screen.getByRole("button", { name: "Save provider" }));
    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith("/admin/providers/prov_1", expect.objectContaining({ method: "PATCH" }));
    });
    const patchCall = vi.mocked(fetch).mock.calls.find(
      ([url, init]) => String(url) === "/admin/providers/prov_1" && init?.method === "PATCH"
    );
    expect(JSON.parse(String(patchCall?.[1]?.body))).toEqual({
      name: "openai",
      base_url: "https://api.openai.com/v1",
      upstream_model: "gpt-4.1-mini"
    });

    await userEvent.click(screen.getByRole("button", { name: "Delete openai" }));
    expect(fetch).toHaveBeenCalledWith("/admin/providers/prov_1", expect.objectContaining({ method: "DELETE" }));
  });

  it("validates_an_existing_passthrough_providers_saved_credentials", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "Edit openai" }));

    await userEvent.click(screen.getByRole("button", { name: "Validate" }));

    expect(await screen.findByText("✓ Model responded successfully.")).toBeInTheDocument();
    expect(fetch).toHaveBeenCalledWith(
      "/admin/providers/prov_1/validate-model",
      expect.objectContaining({ method: "POST", body: JSON.stringify({ model: "gpt-4.1" }) })
    );

    await userEvent.clear(screen.getByLabelText("Upstream model"));
    await userEvent.type(screen.getByLabelText("Upstream model"), "not-a-real-model");
    await userEvent.click(screen.getByRole("button", { name: "Validate" }));

    expect(await screen.findByText("✗ model not found")).toBeInTheDocument();
  });

  it("does_not_offer_validate_for_a_brand_new_unsaved_provider", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "New provider" }));

    expect(screen.queryByRole("button", { name: "Validate" })).not.toBeInTheDocument();
  });
});
