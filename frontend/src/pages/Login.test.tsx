import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { Login } from "./Login";

function renderLogin() {
  render(
    <MemoryRouter initialEntries={["/ui/login"]}>
      <Routes>
        <Route path="/ui/login" element={<Login />} />
        <Route path="/ui/providers" element={<h1>Providers</h1>} />
      </Routes>
    </MemoryRouter>
  );
}

describe("Login", () => {
  beforeEach(() => {
    vi.stubGlobal("fetch", vi.fn());
  });

  it("validates_required_fields_before_submit", async () => {
    renderLogin();
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));

    expect(await screen.findByText("Username and password are required.")).toBeInTheDocument();
    expect(fetch).not.toHaveBeenCalled();
  });

  it("posts_credentials_and_navigates_to_providers", async () => {
    vi.mocked(fetch).mockResolvedValue(new Response("{}", { status: 200 }));
    renderLogin();

    await userEvent.type(screen.getByLabelText("Username"), "admin");
    await userEvent.type(screen.getByLabelText("Password"), "secret");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/auth/login",
      expect.objectContaining({
        method: "POST",
        credentials: "include",
        body: JSON.stringify({ username: "admin", password: "secret" })
      })
    );
    expect(await screen.findByRole("heading", { name: "Providers" })).toBeInTheDocument();
  });

  it("renders_server_error_message", async () => {
    vi.mocked(fetch).mockResolvedValue(
      new Response(JSON.stringify({ error: { message: "too many attempts" } }), { status: 429 })
    );
    renderLogin();

    await userEvent.type(screen.getByLabelText("Username"), "admin");
    await userEvent.type(screen.getByLabelText("Password"), "bad");
    await userEvent.click(screen.getByRole("button", { name: "Sign in" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("too many attempts");
  });
});
