// Indexer GraphQL reads. The events feed stores the tagged ChainEvent
// envelope in `payload`, so `payloadContains` filters MUST nest the
// fields under `payload` (JSONB `@>` silently matches nothing
// otherwise), and object ids inside payloads are hex WITHOUT the 0x
// prefix.

import { useQuery } from "@tanstack/react-query";

import { useServiceUrls } from "../config";
import {
  laneFromCode,
  trancheFromCode,
  type LaneLabel,
  type TrancheLabel,
} from "./vault";

export type IndexedEvent = {
  sequence: string;
  timestampMs: string;
  eventType: string;
  payload: {
    type: string;
    payload: Record<string, unknown>;
  };
};

async function queryEvents(
  graphqlUrl: string,
  filter: unknown,
  limit: number,
): Promise<IndexedEvent[]> {
  const res = await fetch(graphqlUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      query: `query($filter: EventFilterInput, $limit: Int) {
        events(filter: $filter, order: SEQUENCE_DESC, limit: $limit) {
          nodes { sequence timestampMs eventType payload }
        }
      }`,
      variables: { filter, limit },
    }),
  });
  if (!res.ok) throw new Error(`indexer graphql failed: ${res.status}`);
  const body = (await res.json()) as {
    data?: { events: { nodes: IndexedEvent[] } };
    errors?: Array<{ message: string }>;
  };
  if (body.errors?.length) throw new Error(body.errors[0].message);
  return body.data?.events.nodes ?? [];
}

/** Strip a 0x prefix — payload object ids serialize without it. */
export function payloadHex(id: string): string {
  return id.startsWith("0x") ? id.slice(2) : id;
}

function num(v: unknown): number {
  return typeof v === "string" ? Number(v) : typeof v === "number" ? v : 0;
}

function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}

// ── withdraw queue (v2: TvWithdrawRequested − TvWithdrawFulfilled −
//    TvSettlementRedeemed(from_queue), keyed by global_seq per lane) ────

export type WithdrawQueueEntry = {
  globalSeq: number;
  /** Wire code 0=senior 1=junior (untranched vaults queue on junior). */
  lane: LaneLabel;
  tranche: TrancheLabel;
  positionId: string | null;
  capitalGeneration: number;
  recipient: string | null;
  sharesRaw: number;
  basisRaw: number;
  requestedAtMs: number;
};

export type WithdrawQueueView = {
  pending: WithdrawQueueEntry[];
  fulfilledCount: number;
  /** Requests drained through the settlement pool rather than the queue. */
  settledCount: number;
  /** True when the scan window (limit) may have truncated history. */
  truncated: boolean;
};

const EVENT_SCAN_LIMIT = 1000;

export async function fetchWithdrawQueue(
  graphqlUrl: string,
  vaultId: string,
): Promise<WithdrawQueueView> {
  const vault = payloadHex(vaultId);
  const [requested, fulfilled, settled] = await Promise.all([
    queryEvents(
      graphqlUrl,
      {
        eventType: ["TvWithdrawRequested"],
        payloadContains: { payload: { vault_id: vault } },
      },
      EVENT_SCAN_LIMIT,
    ),
    queryEvents(
      graphqlUrl,
      {
        eventType: ["TvWithdrawFulfilled"],
        payloadContains: { payload: { vault_id: vault } },
      },
      EVENT_SCAN_LIMIT,
    ),
    // Wallet-position redemptions carry from_queue=false + global_seq=0
    // and never touch the queue — filter them out at the source.
    queryEvents(
      graphqlUrl,
      {
        eventType: ["TvSettlementRedeemed"],
        payloadContains: { payload: { vault_id: vault, from_queue: true } },
      },
      EVENT_SCAN_LIMIT,
    ),
  ]);
  const doneSeqs = new Set([
    ...fulfilled.map((e) => num(e.payload.payload.global_seq)),
    ...settled.map((e) => num(e.payload.payload.global_seq)),
  ]);
  const pending = requested
    .filter((e) => !doneSeqs.has(num(e.payload.payload.global_seq)))
    .map((e) => {
      const p = e.payload.payload;
      const recipient = str(p.recipient);
      const positionId = str(p.position_id);
      return {
        globalSeq: num(p.global_seq),
        lane: laneFromCode(num(p.lane)),
        tranche: trancheFromCode(num(p.tranche)),
        positionId: positionId ? `0x${payloadHex(positionId)}` : null,
        capitalGeneration: num(p.capital_generation),
        recipient: recipient ? `0x${payloadHex(recipient)}` : null,
        sharesRaw: num(p.shares),
        basisRaw: num(p.basis),
        requestedAtMs: num(p.requested_at_ms) || Number(e.timestampMs),
      };
    })
    .sort((a, b) => a.globalSeq - b.globalSeq);
  return {
    pending,
    fulfilledCount: fulfilled.length,
    settledCount: settled.length,
    truncated:
      requested.length >= EVENT_SCAN_LIMIT ||
      fulfilled.length >= EVENT_SCAN_LIMIT ||
      settled.length >= EVENT_SCAN_LIMIT,
  };
}

export function useWithdrawQueue(vaultId: string | undefined) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["withdrawQueue", urls.indexerGraphql, vaultId],
    queryFn: () => fetchWithdrawQueue(urls.indexerGraphql, vaultId as string),
    enabled: Boolean(vaultId),
    refetchInterval: 30_000,
  });
}

// ── vault flows (LP P&L = NAV vs net deposits, per tranche) ────────────

export type TrancheFlows = { depositedRaw: number; withdrawnRaw: number };

export type VaultFlows = {
  depositedRaw: number;
  withdrawnRaw: number;
  depositCount: number;
  withdrawalCount: number;
  /** Accounting-value flows attributed by tranche (v2: TvDeposited /
   * TvWithdrawFulfilled / TvSettlementRedeemed all carry the code). */
  byTranche: Record<TrancheLabel, TrancheFlows>;
  truncated: boolean;
};

export async function fetchVaultFlows(graphqlUrl: string, vaultId: string): Promise<VaultFlows> {
  const vault = payloadHex(vaultId);
  const [deposits, fulfilled, settled] = await Promise.all([
    queryEvents(
      graphqlUrl,
      { eventType: ["TvDeposited"], payloadContains: { payload: { vault_id: vault } } },
      EVENT_SCAN_LIMIT,
    ),
    queryEvents(
      graphqlUrl,
      { eventType: ["TvWithdrawFulfilled"], payloadContains: { payload: { vault_id: vault } } },
      EVENT_SCAN_LIMIT,
    ),
    // Settlement redemptions (queued or wallet-held) drain vault value too.
    queryEvents(
      graphqlUrl,
      { eventType: ["TvSettlementRedeemed"], payloadContains: { payload: { vault_id: vault } } },
      EVENT_SCAN_LIMIT,
    ),
  ]);
  const byTranche: Record<TrancheLabel, TrancheFlows> = {
    untranched: { depositedRaw: 0, withdrawnRaw: 0 },
    senior: { depositedRaw: 0, withdrawnRaw: 0 },
    junior: { depositedRaw: 0, withdrawnRaw: 0 },
  };
  // TvDeposited.value is the accounting-asset valuation (equal to `amount`
  // for accounting-asset deposits) — the NAV-comparable figure.
  for (const e of deposits) {
    const p = e.payload.payload;
    byTranche[trancheFromCode(num(p.tranche))].depositedRaw += num(p.value);
  }
  for (const e of fulfilled) {
    const p = e.payload.payload;
    byTranche[trancheFromCode(num(p.tranche))].withdrawnRaw += num(p.value);
  }
  for (const e of settled) {
    const p = e.payload.payload;
    byTranche[trancheFromCode(num(p.tranche))].withdrawnRaw += num(p.entitlement);
  }
  const totals = Object.values(byTranche);
  return {
    depositedRaw: totals.reduce((s, t) => s + t.depositedRaw, 0),
    withdrawnRaw: totals.reduce((s, t) => s + t.withdrawnRaw, 0),
    depositCount: deposits.length,
    withdrawalCount: fulfilled.length + settled.length,
    byTranche,
    truncated:
      deposits.length >= EVENT_SCAN_LIMIT ||
      fulfilled.length >= EVENT_SCAN_LIMIT ||
      settled.length >= EVENT_SCAN_LIMIT,
  };
}

export function useVaultFlows(vaultId: string | undefined) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["vaultFlows", urls.indexerGraphql, vaultId],
    queryFn: () => fetchVaultFlows(urls.indexerGraphql, vaultId as string),
    enabled: Boolean(vaultId),
    refetchInterval: 60_000,
  });
}

// ── recent desk fills ──────────────────────────────────────────────────

export type DeskFill = {
  sequence: number;
  timestampMs: number;
  kind: "call" | "put";
  /** Bought = option tokens routed to the vault; wrote = to retail. */
  side: "bought" | "wrote";
  bucketId: string;
  amount: number;
  premium: number;
};

export async function fetchDeskFills(graphqlUrl: string, vaultId: string): Promise<DeskFill[]> {
  const vault = payloadHex(vaultId);
  const events = await queryEvents(
    graphqlUrl,
    {
      eventType: ["WriteExecuted", "PutWriteExecuted"],
      payloadContains: { payload: { collateral_source: vault } },
    },
    100,
  );
  return events.map((e) => {
    const p = e.payload.payload;
    const isPut = e.eventType === "PutWriteExecuted";
    const tokenRecipient = payloadHex(str(isPut ? p.put_token_recipient : p.call_token_recipient));
    const bought = tokenRecipient === vault;
    return {
      sequence: Number(e.sequence),
      timestampMs: Number(e.timestampMs),
      kind: isPut ? "put" : "call",
      side: bought ? "bought" : "wrote",
      bucketId: `0x${payloadHex(str(p.bucket_id))}`,
      amount: num(p.write_amount),
      premium: bought ? num(p.gross_premium) : num(p.net_premium),
    };
  });
}

export function useDeskFills(vaultId: string | undefined) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["deskFills", urls.indexerGraphql, vaultId],
    queryFn: () => fetchDeskFills(urls.indexerGraphql, vaultId as string),
    enabled: Boolean(vaultId),
    refetchInterval: 30_000,
  });
}
