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
          return new Response(JSON.stringify({ ...providers[0], id: "prov_2", name: "anthropic" }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1" && init?.method === "PATCH") {
          return new Response(JSON.stringify({ ...providers[0], upstream_model: "gpt-4.1-mini" }), { status: 200 });
        }
        if (url === "/admin/providers/prov_1" && init?.method === "DELETE") {
          return new Response("{}", { status: 200 });
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
    await userEvent.selectOptions(screen.getByLabelText("Wire format"), "anthropic");
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
});
