// DeepBook v3 PTB builders (SO-154).
//
// Creating a trading venue for a bucket means calling
// `pool::create_permissionless_pool<CALL_i, TUSDC>` on DeepBook's UPGRADED
// package (calls execute there; only event/struct types resolve to the
// original publish). The contract hard-asserts the creation fee coin is
// exactly 500 DEEP — the REAL DeepBook testnet DEEP coin, not our TDEEP test
// token — so the connected wallet must hold it. The PTB shape mirrors the
// proven Rust harness (`rust-backend/tools/deepbook-pool-test`), and the
// gas-station template `deepbook_create_pool` pins exactly this shape.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import {
  DEEPBOOK_ORIGINAL_PACKAGE_ID,
  DEEPBOOK_PACKAGE_ID,
  DEEPBOOK_POOL_CREATION_FEE,
  DEEPBOOK_REGISTRY_ID,
  DEEP_COIN_TYPE,
  ENV,
} from "../config";

// DeepBook order constants (deployed v3; see DEEPBOOK-FINDINGS.md).
// NO_RESTRICTION lets a crossing limit order fill immediately as a taker —
// the behavior users expect from a UI limit order.
const ORDER_NO_RESTRICTION = 0;
const SELF_MATCHING_CANCEL_TAKER = 1;
/** Resting UI orders self-expire on-chain after this long. */
const ORDER_LIFETIME_MS = 7 * 24 * 3_600_000;

/** True when token-info served a DeepBook deployment for this network. */
export function deepbookConfigured(): boolean {
  return Boolean(
    DEEPBOOK_PACKAGE_ID &&
      DEEPBOOK_REGISTRY_ID &&
      DEEP_COIN_TYPE &&
      DEEPBOOK_POOL_CREATION_FEE !== undefined,
  );
}

function requireDeepbook(): {
  pkg: string;
  registry: string;
  deepCoin: string;
  fee: bigint;
} {
  if (!deepbookConfigured()) {
    throw new Error(
      `No DeepBook deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no deepbook ids)`,
    );
  }
  return {
    pkg: DEEPBOOK_PACKAGE_ID!,
    registry: DEEPBOOK_REGISTRY_ID!,
    deepCoin: DEEP_COIN_TYPE!,
    fee: DEEPBOOK_POOL_CREATION_FEE!,
  };
}

export type VenueParams = {
  tickSize: bigint;
  lotSize: bigint;
  minSize: bigint;
};

/**
 * Derive DeepBook pool sizing from the pair's decimals.
 *
 * DeepBook prices are quote-per-base scaled by `10^(9 − baseDec + quoteDec)`
 * (verified against a live fill — DEEPBOOK-FINDINGS.md §C). All three values
 * are kept powers of 10 with `min_size` a multiple of `lot_size`, matching
 * the constraints the deployed contract accepts (the reference values for an
 * 8-dec base / 6-dec quote pair were validated by dry-run in
 * deepbook-pool-test).
 *
 * - `tickSize`: a $0.01 price step (scaled), floored at 1.
 * - `lotSize`: order-size step in base atomic units — 10^3 minimum (DeepBook
 *   floor), targeting ~10^-5 of a whole coin.
 * - `minSize`: smallest order, 10 lots.
 */
export function deriveVenueParams(baseDecimals: number, quoteDecimals: number): VenueParams {
  const priceExponent = 9 - baseDecimals + quoteDecimals;
  const tickSize = 10n ** BigInt(Math.max(0, priceExponent - 2));
  const lotSize = 10n ** BigInt(Math.max(3, baseDecimals - 5));
  const minSize = 10n * lotSize;
  return { tickSize, lotSize, minSize };
}

export type CreateVenueParams = {
  /** `bucket.call_coin_type` — the pool's base asset. */
  callCoinType: string;
  /** `series.settlement_coin_type` — the pool's quote asset (TUSDC). */
  settlementCoinType: string;
  venue: VenueParams;
};

/**
 * Build the create-venue PTB. `coinWithBalance` gathers exactly 500 DEEP
 * from the sender's coins (benign SplitCoins/MergeCoins prelude — the only
 * non-Move-call commands the gas-station template tolerates).
 */
export function buildCreateVenueTx(p: CreateVenueParams): Transaction {
  const { pkg, registry, deepCoin, fee } = requireDeepbook();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::pool::create_permissionless_pool`,
    typeArguments: [p.callCoinType, p.settlementCoinType],
    arguments: [
      tx.object(registry), // &mut Registry (shared; dapp-kit resolves)
      tx.pure.u64(p.venue.tickSize),
      tx.pure.u64(p.venue.lotSize),
      tx.pure.u64(p.venue.minSize),
      coinWithBalance({ type: deepCoin, balance: fee }),
    ],
  });
  return tx;
}

// ---- unit conversions -------------------------------------------------------
//
// DeepBook raw price = quote-atomic per base-atomic × 10^9, so a human price
// (quote display units per base display unit) scales by
// 10^(9 − baseDecimals + quoteDecimals). Verified live (DEEPBOOK-FINDINGS §C).

export function priceScale(baseDecimals: number, quoteDecimals: number): number {
  return 10 ** (9 - baseDecimals + quoteDecimals);
}

/**
 * Human price → raw, snapped onto the tick grid. `mode` rounds in the
 * direction that can only improve the order for its owner ("bid" down,
 * "ask" up).
 */
export function toRawPrice(
  human: number,
  baseDecimals: number,
  quoteDecimals: number,
  tick: bigint,
  mode: "bid" | "ask",
): bigint {
  const raw = human * priceScale(baseDecimals, quoteDecimals);
  if (!Number.isFinite(raw) || raw <= 0) return 0n;
  const t = Number(tick);
  const snapped = mode === "bid" ? Math.floor(raw / t) * t : Math.ceil(raw / t) * t;
  return BigInt(Math.max(0, snapped));
}

export function fromRawPrice(raw: bigint | number, baseDecimals: number, quoteDecimals: number): number {
  return Number(raw) / priceScale(baseDecimals, quoteDecimals);
}

/** Human base quantity → atomic, floored onto the lot grid. */
export function toRawQty(human: number, baseDecimals: number, lot: bigint): bigint {
  const raw = Math.floor(human * 10 ** baseDecimals);
  if (!Number.isFinite(raw) || raw <= 0) return 0n;
  return (BigInt(raw) / lot) * lot;
}

/** Quote atomic a bid of (qtyRaw @ priceRaw) locks up. */
export function bidNotional(priceRaw: bigint, qtyRaw: bigint): bigint {
  return (priceRaw * qtyRaw + 999_999_999n) / 1_000_000_000n;
}

// ---- BalanceManager ---------------------------------------------------------

/**
 * One-time "enable trading" PTB: `new → register_balance_manager → share`.
 * Registering emits `BalanceManagerEvent{id, owner}` — the on-chain record
 * `findBalanceManager` (api/deepbook.ts) recovers the BM from, so a cleared
 * browser profile is never fatal.
 */
export function buildEnableTradingTx(): Transaction {
  const { pkg, registry } = requireDeepbook();
  const orig = DEEPBOOK_ORIGINAL_PACKAGE_ID!;
  const tx = new Transaction();
  const bm = tx.moveCall({ target: `${pkg}::balance_manager::new` });
  tx.moveCall({
    target: `${pkg}::balance_manager::register_balance_manager`,
    arguments: [bm, tx.object(registry)],
  });
  tx.moveCall({
    target: "0x2::transfer::public_share_object",
    typeArguments: [`${orig}::balance_manager::BalanceManager`],
    arguments: [bm],
  });
  return tx;
}

// ---- orders -----------------------------------------------------------------

export type OrderCommonParams = {
  poolId: string;
  bmId: string;
  /** Bucket call coin (pool base). */
  baseCoinType: string;
  /** Settlement coin (pool quote). */
  quoteCoinType: string;
  isBid: boolean;
  /** Base atomic units, already lot-aligned. */
  qtyRaw: bigint;
  /** Top up the BM from the wallet before placing; 0 = skip. */
  depositCoinType?: string;
  depositAmount?: bigint;
};

function startOrderTx(p: OrderCommonParams): { tx: Transaction; proof: ReturnType<Transaction["moveCall"]> } {
  const { pkg } = requireDeepbook();
  const tx = new Transaction();
  if (p.depositCoinType && p.depositAmount && p.depositAmount > 0n) {
    tx.moveCall({
      target: `${pkg}::balance_manager::deposit`,
      typeArguments: [p.depositCoinType],
      arguments: [
        tx.object(p.bmId),
        coinWithBalance({ type: p.depositCoinType, balance: p.depositAmount }),
      ],
    });
  }
  const proof = tx.moveCall({
    target: `${pkg}::balance_manager::generate_proof_as_owner`,
    arguments: [tx.object(p.bmId)],
  });
  return { tx, proof };
}

export function buildPlaceLimitOrderTx(p: OrderCommonParams & { priceRaw: bigint }): Transaction {
  const { pkg } = requireDeepbook();
  const { tx, proof } = startOrderTx(p);
  tx.moveCall({
    target: `${pkg}::pool::place_limit_order`,
    typeArguments: [p.baseCoinType, p.quoteCoinType],
    arguments: [
      tx.object(p.poolId),
      tx.object(p.bmId),
      proof,
      tx.pure.u64(BigInt(Date.now())), // client_order_id
      tx.pure.u8(ORDER_NO_RESTRICTION),
      tx.pure.u8(SELF_MATCHING_CANCEL_TAKER),
      tx.pure.u64(p.priceRaw),
      tx.pure.u64(p.qtyRaw),
      tx.pure.bool(p.isBid),
      tx.pure.bool(false), // pay_with_deep: input-token fees (1.25× penalty)
      tx.pure.u64(BigInt(Date.now() + ORDER_LIFETIME_MS)),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export function buildPlaceMarketOrderTx(p: OrderCommonParams): Transaction {
  const { pkg } = requireDeepbook();
  const { tx, proof } = startOrderTx(p);
  tx.moveCall({
    target: `${pkg}::pool::place_market_order`,
    typeArguments: [p.baseCoinType, p.quoteCoinType],
    arguments: [
      tx.object(p.poolId),
      tx.object(p.bmId),
      proof,
      tx.pure.u64(BigInt(Date.now())),
      tx.pure.u8(SELF_MATCHING_CANCEL_TAKER),
      tx.pure.u64(p.qtyRaw),
      tx.pure.bool(p.isBid),
      tx.pure.bool(false),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export type PoolRefParams = {
  poolId: string;
  bmId: string;
  baseCoinType: string;
  quoteCoinType: string;
};

export function buildCancelOrderTx(p: PoolRefParams & { orderId: bigint }): Transaction {
  const { pkg } = requireDeepbook();
  const tx = new Transaction();
  const proof = tx.moveCall({
    target: `${pkg}::balance_manager::generate_proof_as_owner`,
    arguments: [tx.object(p.bmId)],
  });
  tx.moveCall({
    target: `${pkg}::pool::cancel_order`,
    typeArguments: [p.baseCoinType, p.quoteCoinType],
    arguments: [
      tx.object(p.poolId),
      tx.object(p.bmId),
      proof,
      tx.pure.u128(p.orderId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export function buildCancelAllTx(p: PoolRefParams): Transaction {
  const { pkg } = requireDeepbook();
  const tx = new Transaction();
  const proof = tx.moveCall({
    target: `${pkg}::balance_manager::generate_proof_as_owner`,
    arguments: [tx.object(p.bmId)],
  });
  tx.moveCall({
    target: `${pkg}::pool::cancel_all_orders`,
    typeArguments: [p.baseCoinType, p.quoteCoinType],
    arguments: [tx.object(p.poolId), tx.object(p.bmId), proof, tx.object(SUI_CLOCK_OBJECT_ID)],
  });
  return tx;
}

/**
 * Settle pool-held fills into the BM, then drain BOTH assets back to the
 * wallet. Withdrawing everything keeps the mental model simple: funds only
 * sit in the BM while orders need them.
 */
export function buildWithdrawAllTx(p: PoolRefParams & { recipient: string }): Transaction {
  const { pkg } = requireDeepbook();
  const tx = new Transaction();
  const proof = tx.moveCall({
    target: `${pkg}::balance_manager::generate_proof_as_owner`,
    arguments: [tx.object(p.bmId)],
  });
  tx.moveCall({
    target: `${pkg}::pool::withdraw_settled_amounts`,
    typeArguments: [p.baseCoinType, p.quoteCoinType],
    arguments: [tx.object(p.poolId), tx.object(p.bmId), proof],
  });
  const base = tx.moveCall({
    target: `${pkg}::balance_manager::withdraw_all`,
    typeArguments: [p.baseCoinType],
    arguments: [tx.object(p.bmId)],
  });
  const quote = tx.moveCall({
    target: `${pkg}::balance_manager::withdraw_all`,
    typeArguments: [p.quoteCoinType],
    arguments: [tx.object(p.bmId)],
  });
  tx.transferObjects([base, quote], p.recipient);
  return tx;
}
