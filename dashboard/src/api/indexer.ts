// Indexer GraphQL reads. The events feed stores the tagged ChainEvent
// envelope in `payload`, so `payloadContains` filters MUST nest the
// fields under `payload` (JSONB `@>` silently matches nothing
// otherwise), and object ids inside payloads are hex WITHOUT the 0x
// prefix.

import { useQuery } from "@tanstack/react-query";

import { useServiceUrls } from "../config";

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

// ── withdraw queue (TvWithdrawRequested − TvWithdrawFulfilled) ─────────

export type WithdrawQueueEntry = {
  seq: number;
  recipient: string | null;
  sharesRaw: number;
  basisRaw: number;
  requestedAtMs: number;
};

export type WithdrawQueueView = {
  pending: WithdrawQueueEntry[];
  fulfilledCount: number;
  /** True when the scan window (limit) may have truncated history. */
  truncated: boolean;
};

const EVENT_SCAN_LIMIT = 1000;

export async function fetchWithdrawQueue(
  graphqlUrl: string,
  vaultId: string,
): Promise<WithdrawQueueView> {
  const vault = payloadHex(vaultId);
  const [requested, fulfilled] = await Promise.all([
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
  ]);
  const fulfilledSeqs = new Set(fulfilled.map((e) => num(e.payload.payload.seq)));
  const pending = requested
    .filter((e) => !fulfilledSeqs.has(num(e.payload.payload.seq)))
    .map((e) => {
      const p = e.payload.payload;
      const recipient = str(p.recipient);
      return {
        seq: num(p.seq),
        recipient: recipient ? `0x${payloadHex(recipient)}` : null,
        sharesRaw: num(p.shares),
        basisRaw: num(p.basis),
        requestedAtMs: num(p.requested_at_ms) || Number(e.timestampMs),
      };
    })
    .sort((a, b) => a.seq - b.seq);
  return {
    pending,
    fulfilledCount: fulfilled.length,
    truncated:
      requested.length >= EVENT_SCAN_LIMIT || fulfilled.length >= EVENT_SCAN_LIMIT,
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

// ── vault flows (LP P&L = NAV vs net deposits) ─────────────────────────

export type VaultFlows = {
  depositedRaw: number;
  withdrawnRaw: number;
  depositCount: number;
  withdrawalCount: number;
  truncated: boolean;
};

export async function fetchVaultFlows(graphqlUrl: string, vaultId: string): Promise<VaultFlows> {
  const vault = payloadHex(vaultId);
  const [deposits, fulfilled] = await Promise.all([
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
  ]);
  return {
    depositedRaw: deposits.reduce((s, e) => s + num(e.payload.payload.amount), 0),
    withdrawnRaw: fulfilled.reduce((s, e) => s + num(e.payload.payload.value), 0),
    depositCount: deposits.length,
    withdrawalCount: fulfilled.length,
    truncated:
      deposits.length >= EVENT_SCAN_LIMIT || fulfilled.length >= EVENT_SCAN_LIMIT,
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
