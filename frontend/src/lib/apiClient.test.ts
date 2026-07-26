import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { apiFetch, apiJson } from "./apiClient";

describe("apiClient", () => {
  const originalLocation = window.location;

  beforeEach(() => {
    vi.restoreAllMocks();
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(new Response(JSON.stringify({ ok: true }), { status: 200 }))
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    Object.defineProperty(window, "location", {
      configurable: true,
      value: originalLocation
    });
  });

  it("apiFetch_sets_csrf_header_on_post_but_not_get", async () => {
    await apiFetch("/admin/providers");
    await apiFetch("/admin/providers", { method: "POST" });

    const fetchMock = vi.mocked(fetch);
    expect(fetchMock.mock.calls[0][1]).toMatchObject({
      credentials: "include"
    });
    expect(new Headers(fetchMock.mock.calls[0][1]?.headers).get("X-Requested-With")).toBeNull();
    expect(new Headers(fetchMock.mock.calls[1][1]?.headers).get("X-Requested-With")).toBe("1router-ui");
  });

  it("apiFetch_redirects_to_login_on_401_unless_already_on_login_page", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("{}", { status: 401 })));
    const assign = vi.fn();
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { pathname: "/ui/providers", assign }
    });

    await apiFetch("/admin/providers");
    expect(assign).toHaveBeenCalledWith("/ui/login");

    assign.mockClear();
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { pathname: "/ui/login", assign }
    });

    await apiFetch("/admin/providers");
    expect(assign).not.toHaveBeenCalled();
  });

  it("apiJson_throws_with_server_error_message_on_non_ok", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(JSON.stringify({ error: { message: "bad password" } }), {
          status: 401,
          headers: { "Content-Type": "application/json" }
        })
      )
    );

    await expect(apiJson("/admin/auth/login", { method: "POST" })).rejects.toThrow("bad password");
  });
});
