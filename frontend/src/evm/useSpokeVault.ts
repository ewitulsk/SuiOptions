// One polled snapshot of the spoke vault (plan §1/§4/§5): flags, fund
// states, fee pot, payout queue, and — when an EVM wallet is connected — the
// user's USDG balance/allowance, per-tranche share mirror, in-flight
// withdraw requests, and deposit history.
//
// All reads go through the app's own viem publicClient against the
// configured RPC on an ~8s interval (no websockets), mirroring the
// `enabled` + `refetchInterval` shape of api/useCoinBalance.ts. Deposit
// history is derived from `Deposited` logs (bounded lookback — public RPCs
// cap ranges) re-checked against the `deposits(seq)` records for live
// status.

import { useQuery } from "@tanstack/react-query";
import { getAbiItem, type Address } from "viem";

import { SPOKE_CONFIG, spokeDeployed } from "../config";
import { DEPOSIT_STATUS, spokeVaultAbi, usdgAbi } from "./abi";
import { getSpokePublicClient } from "./client";

/** How far back to scan for the user's `Deposited` logs. Older deposits fall
 * off the list (their escrow state is still safe on-chain — `reclaim` works
 * regardless); an indexer replaces this once the spoke ships. */
const DEPOSIT_LOG_LOOKBACK_BLOCKS = 500_000n;
/** Newest deposits kept in the UI list. */
const MAX_DEPOSIT_ROWS = 50;
/** Queue entries scanned for depth/position display. */
const MAX_QUEUE_SCAN = 25;

export type DepositStatusCode = (typeof DEPOSIT_STATUS)[keyof typeof DEPOSIT_STATUS];

export type SpokeDepositRow = {
  seq: bigint;
  amountRaw: bigint;
  tranche: number;
  status: DepositStatusCode;
  /** Spoke block timestamp of the deposit, seconds. */
  ts: number;
  /** Pending past DEPOSIT_TIMEOUT — the reclaim button lights up. */
  reclaimable: boolean;
};

export type TrancheRow = {
  tranche: number;
  /** Non-authoritative hub-share mirror (UX only — the hub ledger rules). */
  mirrorShares: bigint;
  /** Open withdraw request, if one is in flight for this tranche. */
  inFlight: { seq: bigint; shares: bigint; all: boolean } | null;
};

export type QueueEntry = {
  index: bigint;
  user: Address;
  requestSeq: bigint;
  owedRaw: bigint;
  reservedRaw: bigint;
};

export type SpokeVaultSnapshot = {
  paused: boolean;
  /** Hub-propagated risk_off flag alone. */
  riskOff: boolean;
  /** risk_off as enforced: hub flag OR stale heartbeat. */
  effectiveRiskOff: boolean;
  /** effectiveRiskOff caused purely by hub silence past HEARTBEAT_TIMEOUT. */
  heartbeatStale: boolean;
  /** Last inbound hub message, unix seconds. */
  lastInboundAt: number;
  heartbeatTimeoutSecs: number;
  depositTimeoutSecs: number;
  /** Native fee-pot balance (wei) that pays outbound message transport. */
  feePotWei: bigint;
  funds: { pending: bigint; active: bigint; reserved: bigint };
  queueDepth: bigint;
  /** Up to MAX_QUEUE_SCAN entries from the head of the FIFO queue. */
  queue: QueueEntry[];
  user: {
    usdgBalanceRaw: bigint;
    allowanceRaw: bigint;
    tranches: TrancheRow[];
    deposits: SpokeDepositRow[];
  } | null;
};

const TRANCHES = [0, 1, 2] as const;

async function fetchSnapshot(user: Address | null): Promise<SpokeVaultSnapshot> {
  const cfg = SPOKE_CONFIG!;
  const pub = getSpokePublicClient();
  const vault = { address: cfg.spokeVaultAddress, abi: spokeVaultAbi } as const;

  const [
    paused,
    riskOff,
    effectiveRiskOff,
    lastInboundAt,
    heartbeatTimeout,
    depositTimeout,
    feePot,
    funds,
    queueDepth,
    head,
  ] = await Promise.all([
    pub.readContract({ ...vault, functionName: "paused" }),
    pub.readContract({ ...vault, functionName: "riskOff" }),
    pub.readContract({ ...vault, functionName: "effectiveRiskOff" }),
    pub.readContract({ ...vault, functionName: "lastInboundAt" }),
    pub.readContract({ ...vault, functionName: "HEARTBEAT_TIMEOUT" }),
    pub.readContract({ ...vault, functionName: "DEPOSIT_TIMEOUT" }),
    pub.readContract({ ...vault, functionName: "feePot" }),
    pub.readContract({ ...vault, functionName: "funds", args: [cfg.assetCode] }),
    pub.readContract({ ...vault, functionName: "queueLength", args: [cfg.assetCode] }),
    pub.readContract({ ...vault, functionName: "payoutHead", args: [cfg.assetCode] }),
  ]);

  const scan = queueDepth < BigInt(MAX_QUEUE_SCAN) ? queueDepth : BigInt(MAX_QUEUE_SCAN);
  const queue: QueueEntry[] = await Promise.all(
    Array.from({ length: Number(scan) }, (_, i) => {
      const index = head + BigInt(i);
      return pub
        .readContract({ ...vault, functionName: "payoutQueue", args: [cfg.assetCode, index] })
        .then(([qUser, requestSeq, owed, reservedAmt]) => ({
          index,
          user: qUser,
          requestSeq,
          owedRaw: owed,
          reservedRaw: reservedAmt,
        }));
    }),
  );

  const nowSecs = Math.floor(Date.now() / 1000);
  let userState: SpokeVaultSnapshot["user"] = null;
  if (user) {
    const [usdgBalance, allowance, mirrors, inFlightSeqs] = await Promise.all([
      pub.readContract({
        address: cfg.usdgAddress,
        abi: usdgAbi,
        functionName: "balanceOf",
        args: [user],
      }),
      pub.readContract({
        address: cfg.usdgAddress,
        abi: usdgAbi,
        functionName: "allowance",
        args: [user, cfg.spokeVaultAddress],
      }),
      Promise.all(
        TRANCHES.map((t) =>
          pub.readContract({ ...vault, functionName: "shareMirror", args: [user, t] }),
        ),
      ),
      Promise.all(
        TRANCHES.map((t) =>
          pub.readContract({ ...vault, functionName: "inFlightRequest", args: [user, t] }),
        ),
      ),
    ]);

    const tranches: TrancheRow[] = await Promise.all(
      TRANCHES.map(async (t, i) => {
        const seq = inFlightSeqs[i];
        let inFlight: TrancheRow["inFlight"] = null;
        if (seq !== 0n) {
          const [, , all, open, shares] = await pub.readContract({
            ...vault,
            functionName: "withdrawals",
            args: [seq],
          });
          if (open) inFlight = { seq, shares, all };
        }
        return { tranche: t, mirrorShares: mirrors[i], inFlight };
      }),
    );

    // Deposit history: user's Deposited logs (bounded lookback), then the
    // deposits(seq) records for the authoritative current status.
    let seqs: bigint[] = [];
    try {
      const latest = await pub.getBlockNumber();
      const fromBlock =
        latest > DEPOSIT_LOG_LOOKBACK_BLOCKS ? latest - DEPOSIT_LOG_LOOKBACK_BLOCKS : 0n;
      const logs = await pub.getLogs({
        address: cfg.spokeVaultAddress,
        event: getAbiItem({ abi: spokeVaultAbi, name: "Deposited" }),
        args: { depositor: user },
        fromBlock,
        toBlock: "latest",
      });
      seqs = logs
        .map((l) => l.args.depositSeq)
        .filter((s): s is bigint => s !== undefined);
    } catch {
      // RPCs with tight getLogs caps: degrade to an empty history rather
      // than failing the whole snapshot.
    }
    seqs.sort((a, b) => (a < b ? 1 : a > b ? -1 : 0)); // newest first
    const deposits: SpokeDepositRow[] = await Promise.all(
      seqs.slice(0, MAX_DEPOSIT_ROWS).map(async (seq) => {
        const [, , tranche, status, amount, ts] = await pub.readContract({
          ...vault,
          functionName: "deposits",
          args: [seq],
        });
        return {
          seq,
          amountRaw: amount,
          tranche,
          status: status as DepositStatusCode,
          ts: Number(ts),
          reclaimable:
            status === DEPOSIT_STATUS.Pending &&
            nowSecs >= Number(ts) + Number(depositTimeout),
        };
      }),
    );
    userState = {
      usdgBalanceRaw: usdgBalance,
      allowanceRaw: allowance,
      tranches,
      deposits,
    };
  }

  return {
    paused,
    riskOff,
    effectiveRiskOff,
    heartbeatStale: effectiveRiskOff && !riskOff,
    lastInboundAt: Number(lastInboundAt),
    heartbeatTimeoutSecs: Number(heartbeatTimeout),
    depositTimeoutSecs: Number(depositTimeout),
    feePotWei: feePot,
    funds: { pending: funds[0], active: funds[1], reserved: funds[2] },
    queueDepth,
    queue,
    user: userState,
  };
}

export function useSpokeVault(user: Address | null) {
  const deployed = SPOKE_CONFIG !== undefined && spokeDeployed(SPOKE_CONFIG);
  return useQuery<SpokeVaultSnapshot, Error>({
    queryKey: ["spoke-vault", user],
    enabled: deployed,
    // Polling only — the deposit pending → active flip and queue drain both
    // ride the hub round-trip, so ~8s keeps the screen honest without a ws.
    refetchInterval: 8_000,
    queryFn: () => fetchSnapshot(user),
  });
}
