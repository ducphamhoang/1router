import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ClientKeys, ClientKey } from "./ClientKeys";

const existing: ClientKey = {
  id: "k1",
  name: "alice",
  key_prefix: "1r_abcdefg",
  created_at: "2026-09-01T00:00:00Z",
  last_used_at: null,
  revoked_at: null
};

describe("ClientKeys", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? "GET";
        if (url === "/admin/client-keys" && method === "GET") {
          return new Response(JSON.stringify([existing]), { status: 200 });
        }
        if (url === "/admin/stats/callers") {
          return new Response(
            JSON.stringify({ callers: [{ caller_key_id: "k1", caller_name: "alice", total: 7, successes: 7, failures: 0 }] }),
            { status: 200 }
          );
        }
        if (url === "/admin/client-keys" && method === "POST") {
          return new Response(
            JSON.stringify({ ...existing, id: "k2", name: "bob", key_prefix: "1r_1234567", api_key: "1r_1234567secret" }),
            { status: 201 }
          );
        }
        if (url === "/admin/client-keys/k1" && method === "DELETE") {
          return new Response(JSON.stringify({ ...existing, revoked_at: "2026-09-02T00:00:00Z" }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("lists_keys_with_request_counts", async () => {
    render(<ClientKeys />);
    const row = (await screen.findByText("alice")).closest("tr")!;
    expect(within(row).getByText("7")).toBeInTheDocument();
    expect(within(row).getByText("Active")).toBeInTheDocument();
  });

  it("shows_the_raw_key_once_after_creating", async () => {
    render(<ClientKeys />);
    await screen.findByText("alice");
    await userEvent.type(screen.getByLabelText("Name"), "bob");
    await userEvent.click(screen.getByRole("button", { name: "Create key" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/client-keys",
      expect.objectContaining({ method: "POST", body: JSON.stringify({ name: "bob" }) })
    );
    expect(await screen.findByLabelText("New API key")).toHaveValue("1r_1234567secret");
    expect(screen.getByRole("cell", { name: "bob" })).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Done" }));
    expect(screen.queryByDisplayValue("1r_1234567secret")).not.toBeInTheDocument();
  });

  it("revokes_after_confirmation", async () => {
    render(<ClientKeys />);
    await userEvent.click(await screen.findByRole("button", { name: "Revoke alice" }));
    const dialog = screen.getByRole("dialog", { name: "Revoke API key" });
    await userEvent.click(within(dialog).getByRole("button", { name: "Revoke key" }));

    expect(fetch).toHaveBeenCalledWith("/admin/client-keys/k1", expect.objectContaining({ method: "DELETE" }));
    expect(await screen.findByText(/^Revoked/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Revoke alice" })).not.toBeInTheDocument();
  });
});
