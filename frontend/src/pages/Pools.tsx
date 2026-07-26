import { FormEvent, useEffect, useRef, useState } from "react";
import { DndContext, DragEndEvent } from "@dnd-kit/core";
import { SortableContext, arrayMove } from "@dnd-kit/sortable";
import { apiJson } from "../lib/apiClient";

type PoolMember = {
  provider_id: string;
  provider_name?: string;
  priority: number;
};

type Pool = {
  id: string;
  wire_format: string;
};

export function recomputeMemberPriorities(members: PoolMember[]) {
  return members.map((member, index) => ({
    provider_id: member.provider_id,
    priority: index + 1
  }));
}

export function Pools() {
  const [pools, setPools] = useState<Pool[]>([]);
  const [membersByPool, setMembersByPool] = useState<Record<string, PoolMember[]>>({});
  const [poolId, setPoolId] = useState("");
  const [wireFormat, setWireFormat] = useState("openai");
  const [error, setError] = useState<string | null>(null);
  const latestRequestId = useRef(0);

  async function loadPools() {
    setPools(await apiJson<Pool[]>("/admin/pools"));
  }

  async function loadMembers(poolIds: string[]) {
    const entries = await Promise.all(
      poolIds.map(async (id) => {
        const members = await apiJson<PoolMember[]>(`/admin/pools/${id}/members`);
        return [id, members] as const;
      })
    );
    setMembersByPool(Object.fromEntries(entries));
  }

  useEffect(() => {
    void loadPools();
  }, []);

  useEffect(() => {
    if (pools.length === 0) {
      setMembersByPool({});
      return;
    }
    void loadMembers(pools.map((pool) => pool.id));
  }, [pools]);

  async function createPool(event: FormEvent) {
    event.preventDefault();
    await apiJson("/admin/pools", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ id: poolId, wire_format: wireFormat })
    });
    setPoolId("");
    await loadPools();
  }

  async function deletePool(id: string) {
    await apiJson(`/admin/pools/${id}`, { method: "DELETE" });
    setPools((current) => current.filter((pool) => pool.id !== id));
    setMembersByPool((current) => {
      const next = { ...current };
      delete next[id];
      return next;
    });
  }

  async function persistMembers(poolId: string, members: PoolMember[]) {
    const requestId = latestRequestId.current + 1;
    latestRequestId.current = requestId;
    try {
      await Promise.all(
        recomputeMemberPriorities(members).map((member) =>
          apiJson(`/admin/pools/${poolId}/members`, {
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

  async function moveMember(pool: Pool, providerId: string, direction: -1 | 1) {
    const poolMembers = membersByPool[pool.id] ?? [];
    const oldIndex = poolMembers.findIndex((member) => member.provider_id === providerId);
    const newIndex = oldIndex + direction;
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

  async function onDragEnd(pool: Pool, event: DragEndEvent) {
    if (!event.over || event.active.id === event.over.id) {
      return;
    }
    const poolMembers = membersByPool[pool.id] ?? [];
    const oldIndex = poolMembers.findIndex((member) => member.provider_id === event.active.id);
    const newIndex = poolMembers.findIndex((member) => member.provider_id === event.over?.id);
    if (oldIndex < 0 || newIndex < 0) {
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

  return (
    <section aria-labelledby="pools-title">
      <h1 id="pools-title">Pools</h1>
      <form onSubmit={createPool}>
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
        <button type="submit">Create pool</button>
      </form>
      {error ? <p role="alert">{error}</p> : null}
      {pools.map((pool) => (
        <section key={pool.id} aria-label={`Pool ${pool.id}`}>
          <h2>{pool.id}</h2>
          <p>{pool.wire_format}</p>
          <button type="button" onClick={() => deletePool(pool.id)} aria-label={`Delete ${pool.id}`}>
            Delete
          </button>
          <DndContext onDragEnd={(event) => onDragEnd(pool, event)}>
            <SortableContext items={(membersByPool[pool.id] ?? []).map((member) => member.provider_id)}>
              <ol>
                {(membersByPool[pool.id] ?? []).map((member, index, members) => (
                  <li key={member.provider_id}>
                    <span>{member.provider_name ?? member.provider_id}</span>
                    <button type="button" onClick={() => moveMember(pool, member.provider_id, -1)} aria-label={`Move ${member.provider_name ?? member.provider_id} up`} disabled={index === 0}>
                      Up
                    </button>
                    <button type="button" onClick={() => moveMember(pool, member.provider_id, 1)} aria-label={`Move ${member.provider_name ?? member.provider_id} down`} disabled={index === members.length - 1}>
                      Down
                    </button>
                  </li>
                ))}
              </ol>
            </SortableContext>
          </DndContext>
        </section>
      ))}
    </section>
  );
}
