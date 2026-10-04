import { beforeEach, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { App, SecurityBanner } from "./App";

beforeEach(() => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      if (String(input) === "/admin/settings/security-status") {
        return new Response(
          JSON.stringify({
            shared_secret_is_default: false,
            admin_password_is_default: false,
            require_shared_secret: false,
            listen_addr_is_loopback: false
          }),
          { status: 200 }
        );
      }
      return new Response("[]", { status: 200 });
    })
  );
});

it("shows the critical banner for open non-loopback access", async () => {
  render(
    <MemoryRouter initialEntries={["/ui/providers"]}>
      <SecurityBanner />
    </MemoryRouter>
  );
  expect(
    await screen.findByText(/Open access is on and this gateway isn't bound to localhost/)
  ).toBeInTheDocument();
});

it("shows_a_log_out_button_that_posts_to_logout", async () => {
  const assign = vi.fn();
  vi.stubGlobal("location", { ...window.location, pathname: "/ui/providers", assign });
  render(
    <MemoryRouter initialEntries={["/ui/providers"]}>
      <App />
    </MemoryRouter>
  );
  const { default: userEvent } = await import("@testing-library/user-event");
  await userEvent.click(screen.getByRole("button", { name: "Log out" }));
  expect(fetch).toHaveBeenCalledWith("/admin/auth/logout", expect.objectContaining({ method: "POST" }));
  await vi.waitFor(() => expect(assign).toHaveBeenCalledWith("/ui/login"));
});

it("hides_log_out_on_the_login_page", () => {
  render(
    <MemoryRouter initialEntries={["/ui/login"]}>
      <App />
    </MemoryRouter>
  );
  expect(screen.queryByRole("button", { name: "Log out" })).toBeNull();
});
