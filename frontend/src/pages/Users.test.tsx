import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Users, User } from "./Users";

const alice: User = {
  id: "alice",
  name: "Alice",
  key_prefix: "1r_abcdefg",
  created_at: "2026-09-01T00:00:00Z",
  last_used_at: null,
  revoked_at: null
};

describe("Users", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? "GET";
        if (url === "/admin/users" && method === "GET") {
          return new Response(JSON.stringify([alice]), { status: 200 });
        }
        if (url === "/admin/stats/users") {
          return new Response(
            JSON.stringify({ users: [{ user_id: "alice", total: 7, successes: 7, failures: 0 }] }),
            { status: 200 }
          );
        }
        if (url === "/admin/users" && method === "POST") {
          return new Response(
            JSON.stringify({ ...alice, id: "bob", name: "bob", key_prefix: "1r_1234567", api_key: "1r_1234567secret" }),
            { status: 201 }
          );
        }
        if (url === "/admin/users/alice/rotate" && method === "POST") {
          return new Response(JSON.stringify({ ...alice, key_prefix: "1r_rotated", api_key: "1r_rotatedsecret" }), {
            status: 200
          });
        }
        if (url === "/admin/users/alice/revoke" && method === "POST") {
          return new Response(JSON.stringify({ ...alice, revoked_at: "2026-09-02T00:00:00Z" }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("lists_users_with_request_counts", async () => {
    render(<Users />);
    const row = (await screen.findByRole("cell", { name: "alice" })).closest("tr")!;
    expect(within(row).getByText("7")).toBeInTheDocument();
    expect(within(row).getByText("Active")).toBeInTheDocument();
  });

  it("shows_the_raw_key_once_after_creating", async () => {
    render(<Users />);
    await screen.findByRole("cell", { name: "alice" });
    await userEvent.type(screen.getByLabelText("User id"), "bob");
    await userEvent.click(screen.getByRole("button", { name: "Create user" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/users",
      expect.objectContaining({ method: "POST", body: JSON.stringify({ id: "bob" }) })
    );
    expect(await screen.findByLabelText("New API key")).toHaveValue("1r_1234567secret");
    expect(screen.getAllByRole("cell", { name: "bob" }).length).toBeGreaterThan(0);

    await userEvent.click(screen.getByRole("button", { name: "Done" }));
    expect(screen.queryByDisplayValue("1r_1234567secret")).not.toBeInTheDocument();
  });

  it("rotates_after_confirmation_and_shows_the_new_key", async () => {
    render(<Users />);
    await userEvent.click(await screen.findByRole("button", { name: "Rotate key for alice" }));
    const dialog = screen.getByRole("dialog", { name: "Rotate key" });
    await userEvent.click(within(dialog).getByRole("button", { name: "Rotate key" }));

    expect(fetch).toHaveBeenCalledWith("/admin/users/alice/rotate", expect.objectContaining({ method: "POST" }));
    expect(await screen.findByLabelText("New API key")).toHaveValue("1r_rotatedsecret");
    expect(screen.getByText("1r_rotated…")).toBeInTheDocument();
  });

  it("revokes_after_confirmation", async () => {
    render(<Users />);
    await userEvent.click(await screen.findByRole("button", { name: "Revoke alice" }));
    const dialog = screen.getByRole("dialog", { name: "Revoke user" });
    await userEvent.click(within(dialog).getByRole("button", { name: "Revoke user" }));

    expect(fetch).toHaveBeenCalledWith("/admin/users/alice/revoke", expect.objectContaining({ method: "POST" }));
    expect(await screen.findByText(/^Revoked/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Revoke alice" })).not.toBeInTheDocument();
  });
});
