import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Pools, recomputeMemberPriorities } from "./Pools";

describe("recomputeMemberPriorities", () => {
  it("priority_recompute_on_drag_reassigns_whole_reordered_array", () => {
    const result = recomputeMemberPriorities([
      { provider_id: "b", priority: 20 },
      { provider_id: "a", priority: 10 },
      { provider_id: "c", priority: 40 }
    ]);

    expect(result).toEqual([
      { provider_id: "b", priority: 1 },
      { provider_id: "a", priority: 2 },
      { provider_id: "c", priority: 3 }
    ]);
  });
});

describe("Pools", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([
              {
                id: "openai",
                wire_format: "openai",
                members: [
                  { provider_id: "a", provider_name: "alpha", priority: 1 },
                  { provider_id: "b", provider_name: "beta", priority: 2 }
                ]
              }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/pools" && init?.method === "POST") {
          return new Response(JSON.stringify({ id: "anthropic", wire_format: "anthropic", members: [] }), { status: 200 });
        }
        if (url === "/admin/pools/openai" && init?.method === "DELETE") {
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && init?.method === "PUT") {
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("creates_and_deletes_pools", async () => {
    render(<Pools />);

    await userEvent.type(await screen.findByLabelText("Pool id"), "anthropic");
    await userEvent.selectOptions(screen.getByLabelText("Wire format"), "anthropic");
    await userEvent.click(screen.getByRole("button", { name: "Create pool" }));
    expect(fetch).toHaveBeenCalledWith("/admin/pools", expect.objectContaining({ method: "POST" }));

    await userEvent.click(screen.getByRole("button", { name: "Delete openai" }));
    expect(fetch).toHaveBeenCalledWith("/admin/pools/openai", expect.objectContaining({ method: "DELETE" }));
  });

  it("persists_reordered_members_with_dense_priorities", async () => {
    render(<Pools />);

    await userEvent.click(await screen.findByRole("button", { name: "Move beta up" }));
    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members",
        expect.objectContaining({
          method: "PUT",
          body: JSON.stringify({
            members: [
              { provider_id: "b", priority: 1 },
              { provider_id: "a", priority: 2 }
            ]
          })
        })
      )
    );
  });
});
