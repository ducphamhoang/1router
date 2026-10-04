import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Providers } from "./Providers";

const provider = {
  id: "pa",
  name: "pa",
  wire_format: "openai",
  kind: "passthrough",
  base_url: "http://x/v1",
  api_key: "k",
  upstream_model: "m"
};

describe("Providers delete confirmation", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? "GET";
        if (url === "/admin/providers" && method === "GET") return new Response(JSON.stringify([provider]), { status: 200 });
        if (url === "/admin/pools") return new Response(JSON.stringify([{ id: "main" }, { id: "other" }]), { status: 200 });
        if (url === "/admin/pools/main/members") return new Response(JSON.stringify([{ provider_id: "pa" }]), { status: 200 });
        if (url === "/admin/pools/other/members") return new Response("[]", { status: 200 });
        if (url === "/admin/providers/pa" && method === "DELETE") return new Response("{}", { status: 200 });
        return new Response("{}", { status: 200 });
      })
    );
  });

  it("names_the_pools_the_provider_will_be_removed_from_and_does_not_delete_until_confirmed", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "Delete pa" }));
    expect(await screen.findByText(/main/)).toBeInTheDocument();
    expect(fetch).not.toHaveBeenCalledWith("/admin/providers/pa", expect.objectContaining({ method: "DELETE" }));

    await userEvent.click(screen.getByRole("button", { name: "Confirm delete" }));
    expect(fetch).toHaveBeenCalledWith("/admin/providers/pa", expect.objectContaining({ method: "DELETE" }));
  });

  it("cancel_keeps_the_provider", async () => {
    render(<Providers />);
    await userEvent.click(await screen.findByRole("button", { name: "Delete pa" }));
    await userEvent.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(fetch).not.toHaveBeenCalledWith("/admin/providers/pa", expect.objectContaining({ method: "DELETE" }));
  });
});
