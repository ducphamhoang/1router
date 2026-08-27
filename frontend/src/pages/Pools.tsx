import { FormEvent, useEffect, useRef, useState } from "react";
import { DndContext, DragEndEvent } from "@dnd-kit/core";
import { SortableContext, arrayMove } from "@dnd-kit/sortable";
import { apiJson } from "../lib/apiClient";
import { Modal } from "../components/Modal";

type PoolMember = {
  provider_id: string;
  provider_name?: string;
  priority: number;
  model_override?: string;
  dataset_logging_override?: boolean | null;
};

type Pool = {
  id: string;
  wire_format: string;
  strategy: string;
  sticky_limit?: number | null;
};

const STRATEGY_OPTIONS = [
  { value: "priority", label: "Priority (fallback)" },
  { value: "round_robin", label: "Round Robin (rotate)" }
];

type Provider = {
  id: string;
  name: string;
  wire_format: string;
  upstream_model: string;
};

// A member's real identity is (provider_id, model_override), not just
// provider_id - one provider can now occupy several slots in the same
// pool with different models (see
// migrations/0005_pool_member_model_identity.sql). JSON-encoding the pair
// sidesteps picking a separator character that might collide with either
// half (provider ids are path-id-like strings; model names are free-form
// upstream identifiers with no format guarantee).
function memberKey(member: PoolMember): string {
  return JSON.stringify([member.provider_id, member.model_override ?? null]);
}

export function recomputeMemberPriorities(members: PoolMember[]) {
  return members.map((member, index) => ({
    provider_id: member.provider_id,
    priority: index + 1,
    model_override: member.model_override,
    dataset_logging_override: member.dataset_logging_override
  }));
}

function describeCount(count: number) {
  return count === 0 ? "no providers" : count === 1 ? "1 provider" : `${count} providers`;
}

// Suggestions only - the input still accepts any free-text model name, since
// upstream providers add new ones faster than this list could track.
// DeepSeek is reachable through either wire format (its own OpenAI- and
// Anthropic-compatible endpoints), not a format of its own - so its model
// names belong in both lists rather than a third one keyed off nothing.
const DEEPSEEK_MODEL_SUGGESTIONS = ["deepseek-flash", "deepseek-v4-flash", "deepseek-v4-pro"];

// Unlike DEEPSEEK_MODEL_SUGGESTIONS, these are NOT shared across both
// lists - each OpenCode Go model id only answers on the one wire format
// opencode.ai routes it to (see the OpenCode preset design spec).
// deepseek-v4-pro/deepseek-v4-flash are also OpenCode Go models but are
// omitted here - they're already in DEEPSEEK_MODEL_SUGGESTIONS below, and
// a duplicate <option> value trips React's key-uniqueness warning.
const OPENCODE_OPENAI_MODEL_SUGGESTIONS = [
  "glm-5.2",
  "glm-5.1",
  "kimi-k2.7-code",
  "kimi-k2.6",
  "mimo-v2.5",
  "mimo-v2.5-pro"
];

const OPENCODE_ANTHROPIC_MODEL_SUGGESTIONS = [
  "minimax-m3",
  "minimax-m2.7",
  "minimax-m2.5",
  "qwen3.7-max",
  "qwen3.7-plus",
  "qwen3.6-plus"
];

const OPENAI_MODEL_SUGGESTIONS = [
  "codex-luna",
  "codex-sol",
  "codex-vng",
  "gpt-5.6-sol",
  "gpt-5.6-terra",
  "gpt-5.6-luna",
  "gpt-5.5",
  "gpt-5.4",
  "gpt-5.4-mini",
  "gpt-5.4-nano",
  "gpt-5-codex",
  ...DEEPSEEK_MODEL_SUGGESTIONS,
  ...OPENCODE_OPENAI_MODEL_SUGGESTIONS
];

const ANTHROPIC_MODEL_SUGGESTIONS = [
  "claude-fable-5",
  "claude-opus-5",
  "claude-sonnet-5",
  "claude-haiku-4-5",
  ...DEEPSEEK_MODEL_SUGGESTIONS,
  ...OPENCODE_ANTHROPIC_MODEL_SUGGESTIONS
];

type ValidationState = { state: "checking" | "ok" | "error"; message?: string };

// Live models fetched from the provider's own `/models` endpoint, when it
// has one - falls back to the static suggestion arrays above when it
// doesn't (Codex OAuth) or the call fails (e.g. a mirror without `/models`).
type ModelFetchState = { status: "loading" } | { status: "ok"; models: string[] } | { status: "error"; message: string };

export function Pools() {
  const [pools, setPools] = useState<Pool[]>([]);
  const [providers, setProviders] = useState<Provider[]>([]);
  const [membersByPool, setMembersByPool] = useState<Record<string, PoolMember[]>>({});
  const [poolId, setPoolId] = useState("");
  const [wireFormat, setWireFormat] = useState("openai");
  const [strategy, setStrategy] = useState("priority");
  const [stickyLimit, setStickyLimit] = useState("");
  const [addMemberDraft, setAddMemberDraft] = useState<
    Record<string, { providerId: string; modelOverride: string; datasetLoggingOverride: boolean }>
  >({});
  const [validation, setValidation] = useState<Record<string, ValidationState>>({});
  const [modelFetch, setModelFetch] = useState<Record<string, ModelFetchState>>({});
  const [error, setError] = useState<string | null>(null);
  // The page is list-and-modal: the list identifies pools, everything you can
  // *do* to a pool happens inside a dialog so the list stays scannable.
  const [createOpen, setCreateOpen] = useState(false);
  const [openPoolId, setOpenPoolId] = useState<string | null>(null);
  // Destructive actions are two-step: the first click arms an inline
  // "are you sure?" bar, the second one actually calls the API. Only one
  // thing can be armed at a time so the dialog never shows two red prompts.
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const latestRequestId = useRef(0);

  async function loadPools() {
    setPools(await apiJson<Pool[]>("/admin/pools"));
  }

  async function loadProviders() {
    setProviders(await apiJson<Provider[]>("/admin/providers"));
  }

  async function loadMembers(poolIds: string[]) {
    const entries = await Promise.all(
      poolIds.map(async (id) => {
        const members = await apiJson<PoolMember[]>(`/admin/pools/${id}/members`);
        return [id, members] as const;
      })
    );
    // Merge rather than replace: this is also called with a single pool id
    // after adding/removing one member, and must not drop other pools' data.
    setMembersByPool((current) => ({ ...current, ...Object.fromEntries(entries) }));
  }

  useEffect(() => {
    void loadPools();
    void loadProviders();
  }, []);

  useEffect(() => {
    if (pools.length === 0) {
      setMembersByPool({});
      return;
    }
    void loadMembers(pools.map((pool) => pool.id));
  }, [pools]);

  const openPool = pools.find((pool) => pool.id === openPoolId) ?? null;

  function closeDetail() {
    setOpenPoolId(null);
    setPendingDelete(null);
  }

  async function createPool(event: FormEvent) {
    event.preventDefault();
    try {
      const parsedStickyLimit = Number.parseInt(stickyLimit, 10);
      await apiJson("/admin/pools", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          id: poolId,
          wire_format: wireFormat,
          strategy,
          sticky_limit: Number.isFinite(parsedStickyLimit) ? parsedStickyLimit : undefined
        })
      });
      setPoolId("");
      setStrategy("priority");
      setStickyLimit("");
      setCreateOpen(false);
      await loadPools();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Creating pool failed.");
    }
  }

  // Round Robin only - Priority pools ignore sticky_limit entirely. Refetches
  // the pool list afterward so the header badge reflects what's persisted,
  // not just what was requested.
  async function updateStrategy(pool: Pool, patch: { strategy?: string; sticky_limit?: number }) {
    try {
      await apiJson(`/admin/pools/${encodeURIComponent(pool.id)}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(patch)
      });
      await loadPools();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Updating pool strategy failed.");
    }
  }

  async function deletePool(id: string) {
    setPendingDelete(null);
    try {
      await apiJson(`/admin/pools/${encodeURIComponent(id)}`, { method: "DELETE" });
      setPools((current) => current.filter((pool) => pool.id !== id));
      setMembersByPool((current) => {
        const next = { ...current };
        delete next[id];
        return next;
      });
      setOpenPoolId(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Deleting pool failed.");
    }
  }

  async function persistMembers(poolId: string, members: PoolMember[]) {
    const requestId = latestRequestId.current + 1;
    latestRequestId.current = requestId;
    try {
      await Promise.all(
        recomputeMemberPriorities(members).map((member) =>
          apiJson(`/admin/pools/${encodeURIComponent(poolId)}/members`, {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(member)
          })
        )
      );
    } catch (error) {
      if (requestId === latestRequestId.current) {
        throw error;
      }
    }
  }

  function draftFor(poolId: string) {
    return addMemberDraft[poolId] ?? { providerId: "", modelOverride: "", datasetLoggingOverride: false };
  }

  function setDraftFor(
    poolId: string,
    patch: Partial<{ providerId: string; modelOverride: string; datasetLoggingOverride: boolean }>
  ) {
    setAddMemberDraft((current) => ({ ...current, [poolId]: { ...draftFor(poolId), ...patch } }));
    // Any edit to what's being added invalidates a prior "this model is
    // reachable" check, so it can't be mistaken for a check of the new value.
    setValidation((current) => {
      const next = { ...current };
      delete next[poolId];
      return next;
    });
    // A fetched model list is specific to the previously-chosen provider -
    // switching providers must drop it rather than keep offering the old
    // provider's models under the new one.
    if (patch.providerId !== undefined) {
      setModelFetch((current) => {
        const next = { ...current };
        delete next[poolId];
        return next;
      });
    }
  }

  async function fetchModels(pool: Pool) {
    const draft = draftFor(pool.id);
    if (!draft.providerId) {
      return;
    }
    setModelFetch((current) => ({ ...current, [pool.id]: { status: "loading" } }));
    try {
      const result = await apiJson<{ ok: boolean; models?: string[]; reason?: string }>(
        `/admin/providers/${encodeURIComponent(draft.providerId)}/list-models`
      );
      if (result.ok && result.models && result.models.length > 0) {
        setModelFetch((current) => ({ ...current, [pool.id]: { status: "ok", models: result.models! } }));
      } else {
        setModelFetch((current) => ({
          ...current,
          [pool.id]: { status: "error", message: result.reason ?? "No models returned." }
        }));
      }
    } catch (err) {
      setModelFetch((current) => ({
        ...current,
        [pool.id]: { status: "error", message: err instanceof Error ? err.message : "Fetching models failed." }
      }));
    }
  }

  async function validateModel(pool: Pool) {
    const draft = draftFor(pool.id);
    if (!draft.providerId) {
      return;
    }
    setValidation((current) => ({ ...current, [pool.id]: { state: "checking" } }));
    try {
      const result = await apiJson<{ ok: boolean; status?: number; message?: string }>(
        `/admin/providers/${encodeURIComponent(draft.providerId)}/validate-model`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ model: draft.modelOverride.trim() || undefined })
        }
      );
      setValidation((current) => ({
        ...current,
        [pool.id]: result.ok
          ? { state: "ok" }
          : { state: "error", message: result.message || `Upstream returned HTTP ${result.status}.` }
      }));
    } catch (err) {
      setValidation((current) => ({
        ...current,
        [pool.id]: { state: "error", message: err instanceof Error ? err.message : "Validation request failed." }
      }));
    }
  }

  // A provider's OAuth credentials (e.g. one Codex/ChatGPT login) can be
  // reused across several pools by giving each pool's membership its own
  // `model_override` - so the same provider is offered for every pool
  // regardless of whether it's already a member elsewhere.
  async function addMember(pool: Pool) {
    const draft = draftFor(pool.id);
    if (!draft.providerId) {
      return;
    }
    const existing = membersByPool[pool.id] ?? [];
    const priority = existing.reduce((max, m) => Math.max(max, m.priority), 0) + 1;
    try {
      await apiJson(`/admin/pools/${encodeURIComponent(pool.id)}/members`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          provider_id: draft.providerId,
          priority,
          model_override: draft.modelOverride.trim() || undefined,
          // Unchecked -> omitted entirely (inherit the provider's own
          // setting), same "only sent when opted in" pattern as
          // model_override above - v1 has no UI for explicitly forcing a
          // member's logging *off* against a provider default of on, only
          // "inherit" or "on".
          ...(draft.datasetLoggingOverride ? { dataset_logging_override: true } : {})
        })
      });
      // Only the model field clears - the provider stays selected, so
      // adding several models from the same provider (the whole point of
      // this pool's model_override support) is pick-provider-once,
      // pick-model-per-add instead of re-selecting the provider every time.
      setDraftFor(pool.id, { modelOverride: "", datasetLoggingOverride: false });
      await loadMembers([pool.id]);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Adding pool member failed.");
    }
  }

  async function removeMember(pool: Pool, providerId: string, modelOverride: string | undefined) {
    setPendingDelete(null);
    const targetKey = memberKey({ provider_id: providerId, priority: 0, model_override: modelOverride });
    try {
      // Always target this exact member by (provider, model) - never the
      // bulk "delete every member for this provider" form, since the
      // per-row button is removing one specific row.
      await apiJson(
        `/admin/pools/${encodeURIComponent(pool.id)}/members/${encodeURIComponent(providerId)}?model=${encodeURIComponent(modelOverride ?? "")}`,
        { method: "DELETE" }
      );
      setMembersByPool((current) => ({
        ...current,
        [pool.id]: (current[pool.id] ?? []).filter((m) => memberKey(m) !== targetKey)
      }));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Removing pool member failed.");
    }
  }

  async function reorder(pool: Pool, oldIndex: number, newIndex: number) {
    const poolMembers = membersByPool[pool.id] ?? [];
    if (oldIndex < 0 || newIndex < 0 || newIndex >= poolMembers.length) {
      return;
    }
    const members = arrayMove(poolMembers, oldIndex, newIndex);
    setMembersByPool((current) => ({ ...current, [pool.id]: members }));
    try {
      await persistMembers(pool.id, members);
    } catch (error) {
      setError(error instanceof Error ? error.message : "Pool reorder failed.");
    }
  }

  async function moveMember(pool: Pool, key: string, direction: -1 | 1) {
    const poolMembers = membersByPool[pool.id] ?? [];
    const oldIndex = poolMembers.findIndex((member) => memberKey(member) === key);
    await reorder(pool, oldIndex, oldIndex + direction);
  }

  async function onDragEnd(pool: Pool, event: DragEndEvent) {
    if (!event.over || event.active.id === event.over.id) {
      return;
    }
    const poolMembers = membersByPool[pool.id] ?? [];
    await reorder(
      pool,
      poolMembers.findIndex((member) => memberKey(member) === event.active.id),
      poolMembers.findIndex((member) => memberKey(member) === event.over?.id)
    );
  }

  function renderDetail(pool: Pool) {
    const members = membersByPool[pool.id] ?? [];
    const eligibleProviders = providers.filter((provider) => provider.wire_format === pool.wire_format);
    const poolDeleteKey = `pool:${pool.id}`;
    const fetchState = modelFetch[pool.id];
    return (
      <Modal label={`Pool ${pool.id}`} onClose={closeDetail}>
        <header className="modal-header">
          <div className="pool-identity">
            <h2>{pool.id}</h2>
            <span className="badge">{pool.wire_format}</span>
            <span className="pool-meta">{describeCount(members.length)}</span>
          </div>
          <div className="pool-strategy">
            <label>
              Strategy
              <select
                aria-label={`Strategy for ${pool.id}`}
                value={pool.strategy ?? "priority"}
                onChange={(event) => void updateStrategy(pool, { strategy: event.target.value })}
              >
                {STRATEGY_OPTIONS.map((option) => (
                  <option key={option.value} value={option.value}>
                    {option.label}
                  </option>
                ))}
              </select>
            </label>
            {pool.strategy === "round_robin" ? (
              <label>
                Sticky limit
                <input
                  aria-label={`Sticky limit for ${pool.id}`}
                  type="number"
                  min={1}
                  defaultValue={pool.sticky_limit ?? 1}
                  onBlur={(event) => {
                    const parsed = Number.parseInt(event.target.value, 10);
                    if (Number.isFinite(parsed) && parsed > 0) {
                      void updateStrategy(pool, { sticky_limit: parsed });
                    }
                  }}
                />
              </label>
            ) : null}
          </div>
        </header>

        {pendingDelete === poolDeleteKey ? (
          <div className="confirm-bar" role="group" aria-label={`Confirm deleting ${pool.id}`}>
            <span>
              Delete pool <strong>{pool.id}</strong>? This cannot be undone.
            </span>
            <div className="confirm-actions">
              <button type="button" className="btn-ghost" onClick={() => setPendingDelete(null)} aria-label={`Keep ${pool.id}`}>
                Cancel
              </button>
              <button
                type="button"
                className="btn-danger"
                onClick={() => void deletePool(pool.id)}
                aria-label={`Confirm delete ${pool.id}`}
              >
                Yes, delete pool
              </button>
            </div>
          </div>
        ) : null}

        {members.length === 0 ? (
          <p className="empty-state">This pool has no providers yet, so it can't serve traffic. Add one below.</p>
        ) : (
          // This ordering still matters for a Round Robin pool: it's the
          // base rotation order (which member the cursor starts pointing at
          // after a rotation), not just the fallback order used by Priority
          // pools - reordering isn't hidden for round-robin strategies.
          <DndContext onDragEnd={(event) => void onDragEnd(pool, event)}>
            <SortableContext items={members.map((member) => memberKey(member))}>
              <ol className="member-list">
                {members.map((member, index) => {
                  const name = member.provider_name ?? member.provider_id;
                  const key = memberKey(member);
                  const memberDeleteKey = `member:${pool.id}:${key}`;
                  return (
                    <li key={key} className="member-row">
                      <span className="member-rank" aria-hidden="true">
                        {index + 1}
                      </span>
                      <span className="member-name">{name}</span>
                      {member.model_override ? (
                        <code className="member-override" title="model override for this pool">
                          {member.model_override}
                        </code>
                      ) : (
                        <span className="member-override member-override-default">provider default</span>
                      )}
                      {pendingDelete === memberDeleteKey ? (
                        <span className="member-confirm" role="group" aria-label={`Confirm removing ${name} from ${pool.id}`}>
                          <span>Remove from pool?</span>
                          <button
                            type="button"
                            className="btn-ghost"
                            onClick={() => setPendingDelete(null)}
                            aria-label={`Keep ${name} in ${pool.id}`}
                          >
                            Cancel
                          </button>
                          <button
                            type="button"
                            className="btn-danger"
                            onClick={() => void removeMember(pool, member.provider_id, member.model_override)}
                            aria-label={`Confirm removing ${name} from ${pool.id}`}
                          >
                            Yes, remove
                          </button>
                        </span>
                      ) : (
                        <span className="member-actions">
                          <span className="reorder-group" role="group" aria-label={`Reorder ${name} in ${pool.id}`}>
                            <button
                              type="button"
                              className="btn-reorder"
                              onClick={() => void moveMember(pool, key, -1)}
                              aria-label={`Move ${name} up`}
                              title="Move up (higher priority)"
                              disabled={index === 0}
                            >
                              <span aria-hidden="true">↑</span>
                            </button>
                            <button
                              type="button"
                              className="btn-reorder"
                              onClick={() => void moveMember(pool, key, 1)}
                              aria-label={`Move ${name} down`}
                              title="Move down (lower priority)"
                              disabled={index === members.length - 1}
                            >
                              <span aria-hidden="true">↓</span>
                            </button>
                          </span>
                          <button
                            type="button"
                            className="btn-danger-quiet"
                            onClick={() => setPendingDelete(memberDeleteKey)}
                            aria-label={`Remove ${name} from ${pool.id}`}
                          >
                            Remove
                          </button>
                        </span>
                      )}
                    </li>
                  );
                })}
              </ol>
            </SortableContext>
          </DndContext>
        )}

        <form
          className="add-member-form"
          onSubmit={(event) => {
            event.preventDefault();
            void addMember(pool);
          }}
        >
          <label>
            Add provider
            <select
              aria-label={`Provider to add to ${pool.id}`}
              value={draftFor(pool.id).providerId}
              onChange={(event) => setDraftFor(pool.id, { providerId: event.target.value })}
              disabled={eligibleProviders.length === 0}
            >
              <option value="">-- choose a provider --</option>
              {eligibleProviders.map((provider) => (
                <option key={provider.id} value={provider.id}>
                  {provider.name}
                </option>
              ))}
            </select>
          </label>
          <label>
            Model override <span className="optional">optional</span>
            <div className="model-override-row">
              <input
                aria-label={`Model override for ${pool.id}`}
                placeholder="blank = provider's own upstream_model"
                list={`model-suggestions-${pool.id}`}
                value={draftFor(pool.id).modelOverride}
                onChange={(event) => setDraftFor(pool.id, { modelOverride: event.target.value })}
              />
              <datalist id={`model-suggestions-${pool.id}`}>
                {(fetchState?.status === "ok"
                  ? fetchState.models
                  : pool.wire_format === "anthropic"
                    ? ANTHROPIC_MODEL_SUGGESTIONS
                    : OPENAI_MODEL_SUGGESTIONS
                ).map((model) => (
                  <option key={model} value={model} />
                ))}
              </datalist>
              <button
                type="button"
                className="btn-ghost"
                onClick={() => void fetchModels(pool)}
                disabled={!draftFor(pool.id).providerId || fetchState?.status === "loading"}
                aria-label={`Fetch models for ${pool.id}`}
              >
                {fetchState?.status === "loading" ? "Fetching…" : "Fetch models"}
              </button>
              <button
                type="button"
                className="btn-ghost"
                onClick={() => void validateModel(pool)}
                disabled={!draftFor(pool.id).providerId || validation[pool.id]?.state === "checking"}
                aria-label={`Validate model for ${pool.id}`}
              >
                Validate
              </button>
            </div>
            {fetchState?.status === "ok" ? (
              <span className="validation-result validation-ok" role="status">
                ✓ Showing {fetchState.models.length} live model{fetchState.models.length === 1 ? "" : "s"} from the provider.
              </span>
            ) : fetchState?.status === "error" ? (
              <span className="validation-result validation-error" role="alert">
                ✗ Could not fetch live models ({fetchState.message}) — showing static suggestions instead.
              </span>
            ) : null}
            {validation[pool.id] ? (
              <span
                className={
                  validation[pool.id].state === "ok"
                    ? "validation-result validation-ok"
                    : validation[pool.id].state === "error"
                      ? "validation-result validation-error"
                      : "validation-result"
                }
                role={validation[pool.id].state === "error" ? "alert" : "status"}
              >
                {validation[pool.id].state === "checking"
                  ? "Sending a test request…"
                  : validation[pool.id].state === "ok"
                    ? "✓ Model responded successfully."
                    : `✗ ${validation[pool.id].message}`}
              </span>
            ) : null}
          </label>
          <label className="checkbox-row">
            <input
              type="checkbox"
              aria-label={`Log requests/responses for this membership in ${pool.id}`}
              checked={draftFor(pool.id).datasetLoggingOverride}
              onChange={(event) => setDraftFor(pool.id, { datasetLoggingOverride: event.target.checked })}
            />
            Log requests/responses for this membership (overrides the provider default)
          </label>
          <button type="submit" disabled={!draftFor(pool.id).providerId}>
            Add to pool
          </button>
          {eligibleProviders.length === 0 ? (
            <p className="add-member-hint">
              No <code>{pool.wire_format}</code> providers exist yet — create one on the Providers page first.
            </p>
          ) : null}
        </form>

        {pendingDelete === poolDeleteKey ? null : (
          <footer className="modal-footer">
            <button
              type="button"
              className="btn-danger-quiet"
              onClick={() => setPendingDelete(poolDeleteKey)}
              aria-label={`Delete ${pool.id}`}
            >
              Delete pool
            </button>
          </footer>
        )}
      </Modal>
    );
  }

  return (
    <section aria-labelledby="pools-title">
      <h1 id="pools-title">Pools</h1>
      <p className="page-intro">
        A pool is an ordered list of providers. Requests try the first provider, then fall back down the list. Tap a pool to
        see and reorder its providers.
      </p>
      <div className="list-toolbar">
        <button type="button" className="btn-primary" onClick={() => setCreateOpen(true)} aria-label="Create pool">
          + Create Pool
        </button>
      </div>
      {error ? <p role="alert">{error}</p> : null}
      {pools.length === 0 ? (
        <p className="empty-state">No pools yet. Use “Create Pool”, then add providers to it.</p>
      ) : (
        <ul className="pool-list">
          {pools.map((pool) => (
            <li key={pool.id}>
              <button type="button" className="pool-row" onClick={() => setOpenPoolId(pool.id)} aria-label={`Open pool ${pool.id}`}>
                <span className="pool-row-id">{pool.id}</span>
                <span className="badge">{pool.wire_format}</span>
                <span className="pool-meta">{describeCount((membersByPool[pool.id] ?? []).length)}</span>
                <span className="pool-row-chevron" aria-hidden="true">
                  ›
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}

      {createOpen ? (
        <Modal label="Create pool" onClose={() => setCreateOpen(false)}>
          <header className="modal-header">
            <h2>Create pool</h2>
          </header>
          <form onSubmit={createPool} className="create-pool-form">
            <label>
              Pool id
              <input value={poolId} onChange={(event) => setPoolId(event.target.value)} />
            </label>
            <label>
              Wire format
              <select value={wireFormat} onChange={(event) => setWireFormat(event.target.value)}>
                <option value="openai">openai</option>
                <option value="anthropic">anthropic</option>
              </select>
            </label>
            <label>
              Strategy
              <select aria-label="Strategy" value={strategy} onChange={(event) => setStrategy(event.target.value)}>
                {STRATEGY_OPTIONS.map((option) => (
                  <option key={option.value} value={option.value}>
                    {option.label}
                  </option>
                ))}
              </select>
            </label>
            {strategy === "round_robin" ? (
              <label>
                Sticky limit <span className="optional">requests per provider before rotating, default 1</span>
                <input
                  aria-label="Sticky limit"
                  type="number"
                  min={1}
                  value={stickyLimit}
                  onChange={(event) => setStickyLimit(event.target.value)}
                />
              </label>
            ) : null}
            <div className="modal-actions">
              <button type="button" className="btn-ghost" onClick={() => setCreateOpen(false)} aria-label="Cancel creating pool">
                Cancel
              </button>
              <button type="submit" aria-label="Submit new pool" disabled={!poolId.trim() || poolId.includes("/")}>
                Create pool
              </button>
            </div>
          </form>
        </Modal>
      ) : null}

      {openPool ? renderDetail(openPool) : null}
    </section>
  );
}
