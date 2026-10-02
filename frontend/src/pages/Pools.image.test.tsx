import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { isImageModel, Pools } from "./Pools";

describe("isImageModel", () => {
  it("mirrors_the_server_rule", () => {
    for (const m of ["gpt-image-2", "gpt-image-1.5", "gpt-5.5-image"]) {
      expect(isImageModel(m)).toBe(true);
    }
    for (const m of ["", "gpt-5.5", "gpt-image-", "-image", "gpt image-2", "image"]) {
      expect(isImageModel(m)).toBe(false);
    }
  });
});

describe("Pools (image)", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const get = !init || !init.method || init.method === "GET";
        if (url === "/admin/pools" && get) {
          return new Response(
            JSON.stringify([
              { id: "img", wire_format: "openai", strategy: "priority", sticky_limit: null, modality: "image" }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/pools/img/members" && get) {
          return new Response("[]", { status: 200 });
        }
        if (url === "/admin/providers" && get) {
          return new Response(
            JSON.stringify([
              { id: "cx", name: "codex", kind: "oauth_codex", wire_format: "anthropic", upstream_model: "gpt-5.5" },
              { id: "pt", name: "plain", kind: "passthrough", wire_format: "openai", upstream_model: "gpt-4o" }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/pools" && init?.method === "POST") {
          return new Response(init.body as string, { status: 200 });
        }
        if (url === "/admin/pools/img/members" && init?.method === "PUT") {
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("shows_the_image_badge_on_the_list_row", async () => {
    render(<Pools />);
    expect(await screen.findByRole("button", { name: "Open pool img" })).toHaveTextContent("image");
  });

  it("offers_only_codex_providers_and_hides_chat_only_controls", async () => {
    render(<Pools />);
    await userEvent.click(await screen.findByRole("button", { name: "Open pool img" }));
    const dialog = screen.getByRole("dialog", { name: "Pool img" });

    const providerSelect = within(dialog).getByLabelText("Provider to add to img");
    const options = within(providerSelect).getAllByRole("option").map((o) => o.textContent);
    expect(options.some((o) => o?.includes("codex"))).toBe(true);
    expect(options.some((o) => o?.includes("plain"))).toBe(false);

    expect(within(dialog).queryByLabelText("Fetch models for img")).not.toBeInTheDocument();
    expect(within(dialog).queryByLabelText("Validate model for img")).not.toBeInTheDocument();
    expect(within(dialog).queryByLabelText(/Log requests\/responses/)).not.toBeInTheDocument();
    expect(within(dialog).queryByLabelText("Reasoning effort override for img")).not.toBeInTheDocument();
  });

  it("requires_an_image_model_before_adding_a_member", async () => {
    render(<Pools />);
    await userEvent.click(await screen.findByRole("button", { name: "Open pool img" }));
    const dialog = screen.getByRole("dialog", { name: "Pool img" });

    await userEvent.selectOptions(within(dialog).getByLabelText("Provider to add to img"), "cx");
    const submit = within(dialog).getByRole("button", { name: "Add to pool" });
    expect(submit).toBeDisabled();

    const model = within(dialog).getByLabelText("Model override for img");
    await userEvent.type(model, "gpt-5.5");
    expect(submit).toBeDisabled();
    expect(within(dialog).getByRole("alert")).toHaveTextContent("image model");

    await userEvent.clear(model);
    await userEvent.type(model, "gpt-image-2");
    expect(submit).toBeEnabled();
    await userEvent.click(submit);

    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith("/admin/pools/img/members", expect.objectContaining({ method: "PUT" }))
    );
    const call = vi.mocked(fetch).mock.calls.find(([u, i]) => u === "/admin/pools/img/members" && i?.method === "PUT");
    const body = JSON.parse(String(call?.[1]?.body));
    expect(body).toMatchObject({ provider_id: "cx", model_override: "gpt-image-2" });
    expect(body).not.toHaveProperty("dataset_logging_override");
    expect(body).not.toHaveProperty("reasoning_effort_override");
  });

  it("creates_an_image_pool", async () => {
    render(<Pools />);
    await userEvent.click(await screen.findByRole("button", { name: "Create pool" }));
    const dialog = screen.getByRole("dialog", { name: "Create pool" });
    await userEvent.type(within(dialog).getByLabelText("Pool id"), "img2");
    await userEvent.selectOptions(within(dialog).getByLabelText("Pool type"), "image");
    await userEvent.click(within(dialog).getByRole("button", { name: "Submit new pool" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/pools",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ id: "img2", wire_format: "any", strategy: "priority", modality: "image" })
      })
    );
  });
});
