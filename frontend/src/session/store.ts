// Session-login store (siws_session). Mirrors `state/sponsor.ts`: a
// module-level store exposed via `useSyncExternalStore`.
//
// Owns the whole session lifecycle: sign-in with Phantom (SIWS) or MetaMask
// (SIWE), restore-on-reload, the user's options Account (custody vault —
// created on first sign-in), custody balances/positions, fund/withdraw, and
// root-signed revocation. Every transaction is sponsored through the gas
// station — the ephemeral session key holds no gas, so there is no
// wallet-paid fallback.

import { useSyncExternalStore } from "react";
import { SuiJsonRpcClient, getJsonRpcFullnodeUrl } from "@mysten/sui/jsonRpc";
import type { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import {
  GasStationSponsorClient,
  SessionHandle,
  clearSession,
  createSession,
  createSessionEth,
  readAccountBalances,
  restoreSession,
  suiOptionsGasStation,
  type SessionStatus,
} from "@yourorg/sui-siws-session";

import {
  ENV,
  GAS_STATION_URL,
  PACKAGE_ID,
  SESSION_PACKAGE_ID,
  SESSION_REGISTRY_ID,
  type TestToken,
} from "../config";
import {
  cacheOptionsAccountId,
  readCustodyPositionIds,
  resolveOptionsAccountId,
} from "./accounts";
import { SESSION_ALLOWED, SESSION_TTL_MS, defaultLimits } from "./policy";
import { connectMetaMask, connectPhantom } from "./wallets";

export type SessionPhase = "idle" | "restoring" | "signing-in" | "active";

export type SessionSnapshot = {
  phase: SessionPhase;
  handle: SessionHandle | null;
  status: SessionStatus | null;
  /** The user's options Account (custody vault) — null until created. */
  optionsAccountId: string | null;
  /** Owner address queries/events attribute by (session account id as address). */
  ownerAddress: string | null;
  /** Custody balances keyed by canonical coin type. */
  balances: Record<string, bigint>;
  /** Ids of custodied Positions. */
  positionIds: string[];
  /** Label of an in-flight action, or null. */
  busy: string | null;
  error: string | null;
};

let snap: SessionSnapshot = {
  phase: "idle",
  handle: null,
  status: null,
  optionsAccountId: null,
  ownerAddress: null,
  balances: {},
  positionIds: [],
  busy: null,
  error: null,
};

const listeners = new Set<() => void>();
function emit() {
  listeners.forEach((l) => l());
}
function set(patch: Partial<SessionSnapshot>) {
  snap = { ...snap, ...patch };
  emit();
}

export function useSession(): SessionSnapshot {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => snap,
  );
}

export function getSession(): SessionSnapshot {
  return snap;
}

export function sessionLoginAvailable(): boolean {
  return Boolean(SESSION_PACKAGE_ID && SESSION_REGISTRY_ID);
}

// Module-level client + sponsor (the store runs outside React).
const client = new SuiJsonRpcClient({
  url: getJsonRpcFullnodeUrl(ENV),
  network: ENV,
});
const sponsor = new GasStationSponsorClient(client, suiOptionsGasStation(GAS_STATION_URL));

function sessionConfig() {
  if (!sessionLoginAvailable()) {
    throw new Error("session login is not deployed in this environment");
  }
  return {
    client,
    network: ENV,
    packageId: SESSION_PACKAGE_ID!,
    registryId: SESSION_REGISTRY_ID!,
    sponsor,
  };
}

/** The session-account-id-as-address an options Account is owned by. */
function ownerAddressOf(handle: SessionHandle): string {
  return handle.accountId;
}

// --- lifecycle ---

/** Restore a persisted session at boot. Non-blocking; safe to fire and forget. */
export async function initSession(): Promise<void> {
  if (!sessionLoginAvailable()) return;
  set({ phase: "restoring" });
  try {
    const handle = await restoreSession(sessionConfig());
    if (!handle) {
      set({ phase: "idle" });
      return;
    }
    await activate(handle);
  } catch (err) {
    console.warn("session restore failed:", err);
    set({ phase: "idle" });
  }
}

export async function signInWithPhantom(): Promise<void> {
  set({ phase: "signing-in", error: null });
  try {
    const adapter = await connectPhantom();
    const handle = await createSession({
      ...sessionConfig(),
      solanaWallet: adapter,
      limits: defaultLimits(),
      ttlMs: SESSION_TTL_MS,
      allowed: SESSION_ALLOWED,
      persist: true,
    });
    await activate(handle);
  } catch (err) {
    set({ phase: "idle", error: message(err) });
    throw err;
  }
}

export async function signInWithMetaMask(): Promise<void> {
  set({ phase: "signing-in", error: null });
  try {
    const { adapter, chainId } = await connectMetaMask();
    const handle = await createSessionEth({
      ...sessionConfig(),
      ethereumWallet: adapter,
      chainId,
      limits: defaultLimits(),
      ttlMs: SESSION_TTL_MS,
      allowed: SESSION_ALLOWED,
      persist: true,
    });
    await activate(handle);
  } catch (err) {
    set({ phase: "idle", error: message(err) });
    throw err;
  }
}

async function activate(handle: SessionHandle): Promise<void> {
  const ownerAddress = ownerAddressOf(handle);
  set({ phase: "active", handle, ownerAddress, error: null });
  const optionsAccountId = await resolveOptionsAccountId(
    client,
    handle.accountId,
    ownerAddress,
  );
  set({ optionsAccountId });
  if (!optionsAccountId) {
    await ensureOptionsAccount().catch((err) =>
      console.warn("options account create failed:", err),
    );
  }
  await refreshSession().catch(() => {});
}

/** Forget the local session (the cap simply expires on chain). */
export async function signOutSession(): Promise<void> {
  await clearSession().catch(() => {});
  set({
    phase: "idle",
    handle: null,
    status: null,
    optionsAccountId: null,
    ownerAddress: null,
    balances: {},
    positionIds: [],
    busy: null,
    error: null,
  });
}

/**
 * Root-signed `revoke_all`: bumps the account generation, killing every
 * outstanding cap immediately. Reconnects the root wallet if this session
 * was restored from storage (the wallet reference doesn't persist).
 */
export async function revokeSession(): Promise<void> {
  const { handle } = snap;
  if (!handle) return;
  set({ busy: "revoking" });
  try {
    if (!handle.canRevoke) {
      if (handle.scheme === "solana") {
        handle.attachRootWallet(await connectPhantom());
      } else {
        handle.attachRootWallet((await connectMetaMask()).adapter);
      }
    }
    await handle.revoke();
    await signOutSession();
  } catch (err) {
    set({ busy: null, error: message(err) });
    throw err;
  }
}

// --- options account (custody vault) ---

/** Create the user's options Account if they don't have one yet. */
export async function ensureOptionsAccount(): Promise<string> {
  const { handle, optionsAccountId } = snap;
  if (!handle) throw new Error("no active session");
  if (optionsAccountId) return optionsAccountId;
  if (!PACKAGE_ID) throw new Error("no protocol deployment configured");

  set({ busy: "creating account" });
  try {
    const result = await handle.execute((tx, ctx) => {
      tx.moveCall({
        target: `${PACKAGE_ID}::account::create_and_share_account_with_session`,
        arguments: [
          tx.object(ctx.capId),
          tx.object(ctx.accountId),
          tx.object(SUI_CLOCK_OBJECT_ID),
          // Quote-signing key: session users don't sign MM quotes, so park a
          // zero ed25519 key; `set_quote_signing_key_with_session` can rotate
          // it later if this account ever quotes.
          tx.pure.u8(0),
          tx.pure.vector("u8", Array.from(new Uint8Array(32))),
        ],
      });
    });
    const created = (result.objectChanges ?? []).find(
      (c) => c.type === "created" && c.objectType?.endsWith("::account::Account"),
    );
    if (!created || created.type !== "created") {
      throw new Error("account creation did not produce an Account object");
    }
    cacheOptionsAccountId(handle.accountId, created.objectId);
    set({ optionsAccountId: created.objectId, busy: null });
    return created.objectId;
  } catch (err) {
    set({ busy: null, error: message(err) });
    throw err;
  }
}

// --- custody actions ---

/** Testnet funding: faucet `mint` → `account::deposit`, one sponsored PTB. */
export async function fundFromFaucet(token: TestToken, amountRaw: bigint): Promise<void> {
  const { handle } = snap;
  if (!handle) throw new Error("no active session");
  const accountId = await ensureOptionsAccount();
  set({ busy: `minting ${token.symbol}` });
  try {
    await handle.execute((tx) => {
      const coin = tx.moveCall({
        target: `${token.packageId}::${token.module}::mint`,
        arguments: [tx.object(token.faucetId), tx.pure.u64(amountRaw)],
      });
      tx.moveCall({
        target: `${PACKAGE_ID}::account::deposit`,
        typeArguments: [token.coinType],
        arguments: [tx.object(accountId), coin],
      });
    });
    set({ busy: null });
    await refreshSession();
  } catch (err) {
    set({ busy: null, error: message(err) });
    throw err;
  }
}

/** Cap-gated withdrawal from custody to an external Sui address. */
export async function withdrawFromCustody(
  coinType: string,
  amountRaw: bigint,
  recipient: string,
): Promise<void> {
  const { handle, optionsAccountId } = snap;
  if (!handle) throw new Error("no active session");
  if (!optionsAccountId) throw new Error("no options account");
  set({ busy: "withdrawing" });
  try {
    await handle.execute((tx, ctx) => {
      const coin = tx.moveCall({
        target: `${PACKAGE_ID}::account::withdraw_with_session`,
        typeArguments: [coinType],
        arguments: [
          tx.object(optionsAccountId),
          tx.object(ctx.capId),
          tx.object(ctx.accountId),
          tx.object(SUI_CLOCK_OBJECT_ID),
          tx.pure.u64(amountRaw),
        ],
      });
      tx.transferObjects([coin], recipient);
    });
    set({ busy: null });
    await refreshSession();
  } catch (err) {
    set({ busy: null, error: message(err) });
    throw err;
  }
}

/**
 * Run an app PTB under the session (sponsored, signed by the ephemeral key)
 * and refresh custody state afterwards.
 */
export async function executeWithSession(
  label: string,
  build: (
    tx: Transaction,
    ctx: { capId: string; accountId: string; optionsAccountId: string },
  ) => void,
): Promise<void> {
  const { handle } = snap;
  if (!handle) throw new Error("no active session");
  const optionsAccountId = await ensureOptionsAccount();
  set({ busy: label });
  try {
    await handle.execute((tx, ctx) => build(tx, { ...ctx, optionsAccountId }));
    set({ busy: null });
    await refreshSession();
  } catch (err) {
    set({ busy: null, error: message(err) });
    throw err;
  }
}

// --- reads ---

export async function refreshSession(): Promise<void> {
  const { handle, optionsAccountId } = snap;
  if (!handle) return;
  const status = await handle.status().catch(() => null);
  let balances: Record<string, bigint> = {};
  let positionIds: string[] = [];
  if (optionsAccountId) {
    [balances, positionIds] = await Promise.all([
      readAccountBalances(client, optionsAccountId).catch(() => ({})),
      readCustodyPositionIds(client, optionsAccountId).catch(() => []),
    ]);
  }
  // An expired/revoked cap renders as inactive; the UI offers re-sign-in,
  // which lands back on the same account (the re-access guarantee).
  set({ status, balances, positionIds });
}

export function clearSessionError(): void {
  set({ error: null });
}

function message(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
