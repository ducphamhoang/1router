import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
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

  // Regression test: `recomputeMemberPriorities` used to rebuild each
  // member from a field whitelist that dropped `dataset_logging_override`
  // entirely - since `persistMembers` PUTs this rebuilt object for every
  // member on every reorder, the very next drag after that feature shipped
  // would have silently reset every member's override back to "inherit".
  it("priority_recompute_preserves_dataset_logging_override", () => {
    const result = recomputeMemberPriorities([
      { provider_id: "b", priority: 20, dataset_logging_override: true },
      { provider_id: "a", priority: 10, dataset_logging_override: false },
      { provider_id: "c", priority: 40 }
    ]);

    expect(result).toEqual([
      { provider_id: "b", priority: 1, model_override: undefined, dataset_logging_override: true },
      { provider_id: "a", priority: 2, model_override: undefined, dataset_logging_override: false },
      { provider_id: "c", priority: 3, model_override: undefined, dataset_logging_override: undefined }
    ]);
  });
});

/** Opens the detail dialog for `poolId` and returns it. */
async function openPool(poolId: string) {
  await userEvent.click(await screen.findByRole("button", { name: `Open pool ${poolId}` }));
  return screen.getByRole("dialog", { name: `Pool ${poolId}` });
}

describe("Pools", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([
              { id: "openai", wire_format: "openai", strategy: "priority", sticky_limit: null },
              { id: "claude", wire_format: "anthropic", strategy: "priority", sticky_limit: null }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/pools/openai/members" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([
              { pool_id: "openai", provider_id: "a", provider_name: "alpha", priority: 1 },
              { pool_id: "openai", provider_id: "b", provider_name: "beta", priority: 2, model_override: "gpt-5.6-sol" }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/pools/claude/members" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([]), { status: 200 });
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([
              { id: "a", name: "alpha", wire_format: "openai", upstream_model: "gpt-4o" },
              { id: "b", name: "beta", wire_format: "openai", upstream_model: "gpt-5-codex" }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/pools" && init?.method === "POST") {
          const body = JSON.parse(String(init.body));
          return new Response(
            JSON.stringify({
              id: "extra",
              wire_format: "anthropic",
              strategy: body.strategy ?? "priority",
              sticky_limit: body.sticky_limit ?? null
            }),
            { status: 200 }
          );
        }
        if (url === "/admin/pools/openai" && init?.method === "DELETE") {
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/pools/openai" && init?.method === "PUT") {
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && init?.method === "PUT") {
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/pools/openai/members/b?model=gpt-5.6-sol" && init?.method === "DELETE") {
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );
  });

  it("lists_pools_as_rows_without_any_inline_forms_or_member_detail", async () => {
    render(<Pools />);

    expect(await screen.findByRole("button", { name: "Open pool openai" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Open pool claude" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create pool" })).toBeInTheDocument();

    // The clutter the list view must not show:
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Pool id")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Provider to add to openai")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Delete openai" })).not.toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("button", { name: "Open pool openai" })).toHaveTextContent("2 providers"));
    expect(screen.getByRole("button", { name: "Open pool claude" })).toHaveTextContent("no providers");
  });

  it("opens_a_pool_detail_dialog_with_that_pools_members_and_closes_again", async () => {
    render(<Pools />);

    const dialog = await openPool("openai");
    const rows = within(dialog).getAllByRole("listitem");
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent("alpha");
    expect(rows[1]).toHaveTextContent("beta");
    expect(within(dialog).getByText("gpt-5.6-sol")).toBeInTheDocument();
    expect(dialog).toHaveAttribute("aria-modal", "true");

    await userEvent.click(within(dialog).getByRole("button", { name: "Close dialog" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("shows_the_empty_state_inside_the_dialog_not_on_the_list", async () => {
    render(<Pools />);

    expect(screen.queryByText(/no providers yet/i)).not.toBeInTheDocument();
    const dialog = await openPool("claude");
    expect(within(dialog).getByText(/no providers yet/i)).toBeInTheDocument();
  });

  it("creates_a_pool_through_the_create_dialog", async () => {
    render(<Pools />);

    await userEvent.click(await screen.findByRole("button", { name: "Create pool" }));
    const dialog = screen.getByRole("dialog", { name: "Create pool" });
    await userEvent.type(within(dialog).getByLabelText("Pool id"), "extra");
    await userEvent.selectOptions(within(dialog).getByLabelText("Wire format"), "anthropic");
    await userEvent.click(within(dialog).getByRole("button", { name: "Submit new pool" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/pools",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ id: "extra", wire_format: "anthropic", strategy: "priority" })
      })
    );
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("create_form_defaults_strategy_to_priority", async () => {
    render(<Pools />);

    await userEvent.click(await screen.findByRole("button", { name: "Create pool" }));
    const dialog = screen.getByRole("dialog", { name: "Create pool" });
    expect(within(dialog).getByLabelText("Strategy")).toHaveValue("priority");
    // Sticky limit only makes sense for round_robin - hidden by default.
    expect(within(dialog).queryByLabelText("Sticky limit")).not.toBeInTheDocument();
  });

  it("create_form_can_select_round_robin_and_sets_a_sticky_limit", async () => {
    render(<Pools />);

    await userEvent.click(await screen.findByRole("button", { name: "Create pool" }));
    const dialog = screen.getByRole("dialog", { name: "Create pool" });
    await userEvent.type(within(dialog).getByLabelText("Pool id"), "extra");
    await userEvent.selectOptions(within(dialog).getByLabelText("Strategy"), "round_robin");
    await userEvent.type(within(dialog).getByLabelText("Sticky limit"), "3");
    await userEvent.click(within(dialog).getByRole("button", { name: "Submit new pool" }));

    expect(fetch).toHaveBeenCalledWith(
      "/admin/pools",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ id: "extra", wire_format: "openai", strategy: "round_robin", sticky_limit: 3 })
      })
    );
  });

  it("pool_detail_shows_a_strategy_editor_that_puts_on_change", async () => {
    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.selectOptions(within(dialog).getByLabelText("Strategy for openai"), "round_robin");

    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai",
        expect.objectContaining({ method: "PUT", body: JSON.stringify({ strategy: "round_robin" }) })
      );
    });
  });

  it("cancels_the_create_dialog_without_creating_anything", async () => {
    render(<Pools />);

    await userEvent.click(await screen.findByRole("button", { name: "Create pool" }));
    await userEvent.click(screen.getByRole("button", { name: "Cancel creating pool" }));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(fetch).not.toHaveBeenCalledWith("/admin/pools", expect.objectContaining({ method: "POST" }));
  });

  it("deletes_a_pool_from_its_dialog_after_confirming", async () => {
    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete openai" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Confirm delete openai" }));

    expect(fetch).toHaveBeenCalledWith("/admin/pools/openai", expect.objectContaining({ method: "DELETE" }));
    // Deleting closes the dialog and drops the row from the list.
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "Open pool openai" })).not.toBeInTheDocument();
  });

  it("requires_a_confirmation_before_deleting_a_pool", async () => {
    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete openai" }));
    expect(fetch).not.toHaveBeenCalledWith("/admin/pools/openai", expect.objectContaining({ method: "DELETE" }));

    await userEvent.click(within(dialog).getByRole("button", { name: "Keep openai" }));
    expect(fetch).not.toHaveBeenCalledWith("/admin/pools/openai", expect.objectContaining({ method: "DELETE" }));
    expect(within(dialog).getByRole("button", { name: "Delete openai" })).toBeInTheDocument();
  });

  it("requires_a_confirmation_before_removing_a_member", async () => {
    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.click(within(dialog).getByRole("button", { name: "Remove beta from openai" }));
    expect(fetch).not.toHaveBeenCalledWith("/admin/pools/openai/members/b?model=gpt-5.6-sol", expect.objectContaining({ method: "DELETE" }));

    await userEvent.click(within(dialog).getByRole("button", { name: "Keep beta in openai" }));
    expect(fetch).not.toHaveBeenCalledWith("/admin/pools/openai/members/b?model=gpt-5.6-sol", expect.objectContaining({ method: "DELETE" }));
    expect(within(dialog).getByRole("button", { name: "Remove beta from openai" })).toBeInTheDocument();
  });

  it("removes_a_member_from_a_pool", async () => {
    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.click(within(dialog).getByRole("button", { name: "Remove beta from openai" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Confirm removing beta from openai" }));

    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith("/admin/pools/openai/members/b?model=gpt-5.6-sol", expect.objectContaining({ method: "DELETE" }));
    });
  });

  it("persists_reordered_members_with_dense_priorities", async () => {
    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.click(within(dialog).getByRole("button", { name: "Move beta up" }));
    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members",
        expect.objectContaining({
          method: "PUT",
          body: JSON.stringify({ provider_id: "b", priority: 1, model_override: "gpt-5.6-sol" })
        })
      );
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members",
        expect.objectContaining({ method: "PUT", body: JSON.stringify({ provider_id: "a", priority: 2 }) })
      );
    });
    // The dialog reflects the new order immediately.
    const names = within(dialog)
      .getAllByRole("listitem")
      .map((row) => row.textContent);
    expect(names[0]).toContain("beta");
    expect(names[1]).toContain("alpha");
  });

  it("adds_a_provider_to_a_pool_with_a_model_override", async () => {
    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.selectOptions(within(dialog).getByLabelText("Provider to add to openai"), "a");
    await userEvent.type(within(dialog).getByLabelText("Model override for openai"), "gpt-5.6-terra");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add to pool" }));

    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members",
        expect.objectContaining({
          method: "PUT",
          body: JSON.stringify({ provider_id: "a", priority: 3, model_override: "gpt-5.6-terra" })
        })
      );
    });
  });

  it("adds_a_provider_with_dataset_logging_enabled_when_the_checkbox_is_checked", async () => {
    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.selectOptions(within(dialog).getByLabelText("Provider to add to openai"), "a");
    await userEvent.click(
      within(dialog).getByLabelText("Log requests/responses for this membership in openai")
    );
    await userEvent.click(within(dialog).getByRole("button", { name: "Add to pool" }));

    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members",
        expect.objectContaining({
          method: "PUT",
          body: JSON.stringify({ provider_id: "a", priority: 3, dataset_logging_override: true })
        })
      );
    });
  });

  // Full-component regression test for the reorder-wipe bug: member "b"
  // starts with dataset_logging_override: true; reordering must PUT that
  // value back unchanged, not silently drop it.
  it("reordering_a_pool_preserves_each_members_dataset_logging_override", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([{ id: "openai", wire_format: "openai" }]), { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([
              { pool_id: "openai", provider_id: "a", provider_name: "alpha", priority: 1 },
              {
                pool_id: "openai",
                provider_id: "b",
                provider_name: "beta",
                priority: 2,
                dataset_logging_override: true
              }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([
              { id: "a", name: "alpha", wire_format: "openai", upstream_model: "gpt-4o" },
              { id: "b", name: "beta", wire_format: "openai", upstream_model: "gpt-5-codex" }
            ]),
            { status: 200 }
          );
        }
        if (url === "/admin/pools/openai/members" && init?.method === "PUT") {
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<Pools />);
    const dialog = await openPool("openai");
    await userEvent.click(within(dialog).getByRole("button", { name: "Move beta up" }));

    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members",
        expect.objectContaining({
          method: "PUT",
          body: JSON.stringify({ provider_id: "b", priority: 1, dataset_logging_override: true })
        })
      );
    });
  });

  it("suggests_openai_models_for_an_openai_pool_and_anthropic_models_for_an_anthropic_pool", async () => {
    render(<Pools />);

    const openaiDialog = await openPool("openai");
    const openaiInput = within(openaiDialog).getByLabelText("Model override for openai");
    const openaiList = openaiDialog.querySelector(`#${openaiInput.getAttribute("list")}`) as HTMLDataListElement;
    expect(openaiList.querySelector('option[value="codex-luna"]')).not.toBeNull();
    expect(openaiList.querySelector('option[value="codex-sol"]')).not.toBeNull();
    expect(openaiList.querySelector('option[value="codex-vng"]')).not.toBeNull();
    expect(openaiList.querySelector('option[value="deepseek-flash"]')).not.toBeNull();
    expect(openaiList.querySelector('option[value="gpt-5.6-sol"]')).not.toBeNull();
    expect(openaiList.querySelector('option[value="claude-sonnet-5"]')).toBeNull();
    expect(openaiList.querySelector('option[value="deepseek-v4-flash"]')).not.toBeNull();

    await userEvent.click(within(openaiDialog).getByRole("button", { name: "Close dialog" }));

    const claudeDialog = await openPool("claude");
    const claudeInput = within(claudeDialog).getByLabelText("Model override for claude");
    const claudeList = claudeDialog.querySelector(`#${claudeInput.getAttribute("list")}`) as HTMLDataListElement;
    expect(claudeList.querySelector('option[value="claude-sonnet-5"]')).not.toBeNull();
    expect(claudeList.querySelector('option[value="gpt-5.6-sol"]')).toBeNull();
    // DeepSeek has no wire_format of its own - it's reachable through either,
    // so its models must show up as suggestions regardless of pool format.
    expect(claudeList.querySelector('option[value="deepseek-flash"]')).not.toBeNull();
    expect(claudeList.querySelector('option[value="deepseek-v4-flash"]')).not.toBeNull();
  });

  it("validates_a_model_name_against_the_providers_own_adapter_before_adding_it", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([{ id: "openai", wire_format: "openai" }]), { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([]), { status: 200 });
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([{ id: "a", name: "alpha", wire_format: "openai", upstream_model: "gpt-4o" }]),
            { status: 200 }
          );
        }
        if (url === "/admin/providers/a/validate-model" && init?.method === "POST") {
          const body = JSON.parse(String(init.body));
          return body.model === "not-a-real-model"
            ? new Response(JSON.stringify({ ok: false, status: 404, message: "model not found" }), { status: 200 })
            : new Response(JSON.stringify({ ok: true, status: 200 }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<Pools />);
    const dialog = await openPool("openai");

    expect(within(dialog).getByRole("button", { name: "Validate model for openai" })).toBeDisabled();

    await userEvent.selectOptions(within(dialog).getByLabelText("Provider to add to openai"), "a");
    await userEvent.type(within(dialog).getByLabelText("Model override for openai"), "gpt-5.6-sol");
    await userEvent.click(within(dialog).getByRole("button", { name: "Validate model for openai" }));

    expect(await within(dialog).findByText("✓ Model responded successfully.")).toBeInTheDocument();
    expect(fetch).toHaveBeenCalledWith(
      "/admin/providers/a/validate-model",
      expect.objectContaining({ method: "POST", body: JSON.stringify({ model: "gpt-5.6-sol" }) })
    );

    await userEvent.clear(within(dialog).getByLabelText("Model override for openai"));
    await userEvent.type(within(dialog).getByLabelText("Model override for openai"), "not-a-real-model");
    await userEvent.click(within(dialog).getByRole("button", { name: "Validate model for openai" }));

    expect(await within(dialog).findByText("✗ model not found")).toBeInTheDocument();
  });

  it("fetches_the_providers_live_model_list_and_uses_it_as_datalist_options", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([{ id: "openai", wire_format: "openai" }]), { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([]), { status: 200 });
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([{ id: "a", name: "alpha", wire_format: "openai", upstream_model: "gpt-4o" }]),
            { status: 200 }
          );
        }
        if (url === "/admin/providers/a/list-models" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify({ ok: true, models: ["live-model-1", "live-model-2"] }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<Pools />);
    const dialog = await openPool("openai");

    expect(within(dialog).getByRole("button", { name: "Fetch models for openai" })).toBeDisabled();

    await userEvent.selectOptions(within(dialog).getByLabelText("Provider to add to openai"), "a");
    await userEvent.click(within(dialog).getByRole("button", { name: "Fetch models for openai" }));

    expect(await within(dialog).findByText(/Showing 2 live models from the provider\./)).toBeInTheDocument();
    const input = within(dialog).getByLabelText("Model override for openai");
    const list = dialog.querySelector(`#${input.getAttribute("list")}`) as HTMLDataListElement;
    expect(list.querySelector('option[value="live-model-1"]')).not.toBeNull();
    expect(list.querySelector('option[value="gpt-5.6-sol"]')).toBeNull();
  });

  it("falls_back_to_static_suggestions_when_the_provider_has_no_models_endpoint", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([{ id: "openai", wire_format: "openai" }]), { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([]), { status: 200 });
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([{ id: "cx", name: "codex", wire_format: "openai", upstream_model: "pending" }]),
            { status: 200 }
          );
        }
        if (url === "/admin/providers/cx/list-models" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify({ ok: false, reason: "this provider kind has no discoverable /models endpoint" }),
            { status: 200 }
          );
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<Pools />);
    const dialog = await openPool("openai");

    await userEvent.selectOptions(within(dialog).getByLabelText("Provider to add to openai"), "cx");
    await userEvent.click(within(dialog).getByRole("button", { name: "Fetch models for openai" }));

    expect(
      await within(dialog).findByText(/Could not fetch live models .*no discoverable \/models endpoint/)
    ).toBeInTheDocument();
    const input = within(dialog).getByLabelText("Model override for openai");
    const list = dialog.querySelector(`#${input.getAttribute("list")}`) as HTMLDataListElement;
    expect(list.querySelector('option[value="gpt-5.6-sol"]')).not.toBeNull();
  });

  it("clears_a_stale_validation_result_as_soon_as_the_model_or_provider_choice_changes", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([{ id: "openai", wire_format: "openai" }]), { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([]), { status: 200 });
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([{ id: "a", name: "alpha", wire_format: "openai", upstream_model: "gpt-4o" }]),
            { status: 200 }
          );
        }
        if (url === "/admin/providers/a/validate-model" && init?.method === "POST") {
          return new Response(JSON.stringify({ ok: true, status: 200 }), { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<Pools />);
    const dialog = await openPool("openai");
    await userEvent.selectOptions(within(dialog).getByLabelText("Provider to add to openai"), "a");
    await userEvent.type(within(dialog).getByLabelText("Model override for openai"), "gpt-5.6-sol");
    await userEvent.click(within(dialog).getByRole("button", { name: "Validate model for openai" }));
    expect(await within(dialog).findByText("✓ Model responded successfully.")).toBeInTheDocument();

    await userEvent.type(within(dialog).getByLabelText("Model override for openai"), "-mini");
    expect(within(dialog).queryByText("✓ Model responded successfully.")).not.toBeInTheDocument();
  });

  it("shows_an_error_instead_of_silently_doing_nothing_when_delete_pool_fails", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([{ id: "openai", wire_format: "openai" }]), { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([]), { status: 200 });
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([]), { status: 200 });
        }
        if (url === "/admin/pools/openai" && init?.method === "DELETE") {
          return new Response(JSON.stringify({ error: { message: "not found" } }), { status: 404 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete openai" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Confirm delete openai" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("not found");
    // Dialog stays open so the failure is visible next to the action.
    expect(screen.getByRole("dialog", { name: "Pool openai" })).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Delete openai" })).toBeInTheDocument();
  });

  it("percent_encodes_ids_containing_slashes_so_the_delete_hits_the_right_route", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([{ id: "openai", wire_format: "openai" }]), { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([{ pool_id: "openai", provider_id: "team/gpt", provider_name: "team/gpt", priority: 1 }]),
            { status: 200 }
          );
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([]), { status: 200 });
        }
        if (url === "/admin/pools/openai/members/team%2Fgpt?model=" && init?.method === "DELETE") {
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<Pools />);

    const dialog = await openPool("openai");
    await userEvent.click(within(dialog).getByRole("button", { name: "Remove team/gpt from openai" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Confirm removing team/gpt from openai" }));

    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members/team%2Fgpt?model=",
        expect.objectContaining({ method: "DELETE" })
      );
    });
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  // Regression test for the pool-member-model-identity fix: a provider can
  // now occupy two slots in the same pool with different model_overrides
  // (migrations/0005_pool_member_model_identity.sql). Every member action
  // used to be keyed by bare provider_id; this proves reorder and delete
  // both target the CORRECT row once two rows share a provider_id.
  //
  // Uses the up/down buttons, not a simulated drag - there is no
  // useSortable/useDraggable anywhere in this app, so DndContext/
  // SortableContext are currently inert and onDragEnd never fires. The
  // buttons (moveMember) are the only reorder path that actually works.
  it("reorders_and_deletes_the_correct_row_when_one_provider_backs_two_models", async () => {
    let members = [
      { pool_id: "openai", provider_id: "shared", provider_name: "shared", priority: 1, model_override: "model-a" },
      { pool_id: "openai", provider_id: "shared", provider_name: "shared", priority: 2, model_override: "model-b" }
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url === "/admin/pools" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify([{ id: "openai", wire_format: "openai" }]), { status: 200 });
        }
        if (url === "/admin/pools/openai/members" && (!init || init.method === "GET")) {
          return new Response(JSON.stringify(members), { status: 200 });
        }
        if (url === "/admin/providers" && (!init || init.method === "GET")) {
          return new Response(
            JSON.stringify([{ id: "shared", name: "shared", wire_format: "openai", upstream_model: "m" }]),
            { status: 200 }
          );
        }
        if (url === "/admin/pools/openai/members" && init?.method === "PUT") {
          // Mirrors persistMembers' reorder PUTs by updating the local
          // fixture in place, so a follow-up GET reflects the new order -
          // the real regression only shows up if reorder targets the
          // wrong row's model_override.
          const body = JSON.parse(String(init.body));
          const idx = members.findIndex((m) => m.model_override === body.model_override);
          if (idx !== -1) {
            members[idx] = { ...members[idx], priority: body.priority };
          }
          return new Response("{}", { status: 200 });
        }
        if (url === "/admin/pools/openai/members/shared?model=model-a" && init?.method === "DELETE") {
          members = members.filter((m) => m.model_override !== "model-a");
          return new Response("{}", { status: 200 });
        }
        return new Response("{}", { status: 404 });
      })
    );

    render(<Pools />);
    let dialog = await openPool("openai");
    let rows = within(dialog).getAllByRole("listitem");
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent("model-a");
    expect(rows[1]).toHaveTextContent("model-b");

    // Move model-b (the second row) up - must not be confused with
    // model-a just because they share a provider_id.
    await userEvent.click(within(rows[1]).getByRole("button", { name: "Move shared up" }));
    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members",
        expect.objectContaining({
          method: "PUT",
          body: JSON.stringify({ provider_id: "shared", priority: 1, model_override: "model-b" })
        })
      );
    });

    // The reorder is applied optimistically to local state before the PUT
    // resolves, so the still-open dialog already reflects the swap.
    rows = within(dialog).getAllByRole("listitem");
    expect(rows[0]).toHaveTextContent("model-b");
    expect(rows[1]).toHaveTextContent("model-a");

    // Delete model-a specifically - model-b (same provider_id) must survive.
    const modelARow = rows.find((row) => row.textContent?.includes("model-a"))!;
    await userEvent.click(within(modelARow).getByRole("button", { name: "Remove shared from openai" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Confirm removing shared from openai" }));

    await waitFor(() => {
      expect(fetch).toHaveBeenCalledWith(
        "/admin/pools/openai/members/shared?model=model-a",
        expect.objectContaining({ method: "DELETE" })
      );
    });
    await waitFor(() => {
      const remaining = within(dialog).getAllByRole("listitem");
      expect(remaining).toHaveLength(1);
      expect(remaining[0]).toHaveTextContent("model-b");
    });
  });
});
