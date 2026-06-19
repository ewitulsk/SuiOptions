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
import { connectMetaMask, connectPhantom, connectWalletConnect } from "./wallets";
import { posthog } from "../lib/posthog";

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
    posthog.capture("session_restored", { scheme: handle.scheme });
  } catch (err) {
    posthog.captureException(err, { action: "session_restore" });
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
    posthog.capture("session_signed_in", { scheme: "solana" });
  } catch (err) {
    posthog.captureException(err, { action: "session_sign_in", scheme: "solana" });
    set({ phase: "idle", error: message(err) });
    throw err;
  }
}

// The persisted session records only scheme="ethereum" — it can't tell which
// Ethereum connector signed in. Remember it so revoke / re-sign-in after a
// reload reconnect the SAME root wallet; a different connector would yield a
// different address and resolve to the wrong (or no) account.
type EthConnector = "metamask" | "walletconnect";
const ETH_CONNECTOR_KEY = "siws.ethConnector";

function rememberEthConnector(c: EthConnector): void {
  try {
    localStorage.setItem(ETH_CONNECTOR_KEY, c);
  } catch {
    /* private mode / storage disabled — hint is best-effort */
  }
}

function lastEthConnector(): EthConnector {
  try {
    return localStorage.getItem(ETH_CONNECTOR_KEY) === "walletconnect"
      ? "walletconnect"
      : "metamask";
  } catch {
    return "metamask";
  }
}

function connectEth(c: EthConnector) {
  return c === "walletconnect" ? connectWalletConnect() : connectMetaMask();
}

/** Open an Ethereum (SIWE) session via the given connector. */
async function openEthSession(connector: EthConnector): Promise<void> {
  set({ phase: "signing-in", error: null });
  try {
    const { adapter, chainId } = await connectEth(connector);
    rememberEthConnector(connector);
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
    posthog.capture("session_signed_in", { scheme: "ethereum", connector });
  } catch (err) {
    posthog.captureException(err, {
      action: "session_sign_in",
      scheme: "ethereum",
      connector,
    });
    set({ phase: "idle", error: message(err) });
    throw err;
  }
}

export function signInWithMetaMask(): Promise<void> {
  return openEthSession("metamask");
}

export function signInWithWalletConnect(): Promise<void> {
  return openEthSession("walletconnect");
}

/** Re-open an Ethereum session with whichever connector last signed in. */
export function signInWithLastEth(): Promise<void> {
  return openEthSession(lastEthConnector());
}

async function activate(handle: SessionHandle): Promise<void> {
  const ownerAddress = ownerAddressOf(handle);
  // Session users have no dapp-kit wallet; identify by the session's owner
  // address (the address custody balances/positions are attributed to).
  posthog.identify(ownerAddress, {
    wallet_address: ownerAddress,
    auth: "session",
    scheme: handle.scheme,
  });
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
  posthog.capture("session_signed_out", { scheme: snap.handle?.scheme });
  posthog.reset();
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
        handle.attachRootWallet((await connectEth(lastEthConnector())).adapter);
      }
    }
    await handle.revoke();
    posthog.capture("session_revoked", { scheme: handle.scheme });
    await signOutSession();
  } catch (err) {
    posthog.captureException(err, { action: "session_revoke", scheme: handle.scheme });
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
        target: `${PACKAGE_ID}::session_account::create_and_share_account_with_session`,
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
    posthog.capture("session_account_created", { account_id: created.objectId });
    return created.objectId;
  } catch (err) {
    posthog.captureException(err, { action: "session_account_create" });
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
    posthog.capture("custody_funded", {
      token_symbol: token.symbol,
      amount: Number(amountRaw) / 10 ** token.decimals,
    });
    await refreshSession();
  } catch (err) {
    posthog.captureException(err, { action: "custody_fund", token_symbol: token.symbol });
    set({ busy: null, error: message(err) });
    throw err;
  }
}

/**
 * Withdrawal from custody to an external Sui address. Unlike every other
 * session action, this is NOT authorized by the session key: it requires a
 * fresh signature from the user's HOST wallet (Solana/Ethereum), verified
 * on-chain, binding this exact (account, coin type, amount, recipient). The
 * tx is still session-signed + sponsored, but the session key alone can't
 * move funds out — it triggers a wallet prompt here.
 */
export async function withdrawFromCustody(
  coinType: string,
  amountRaw: bigint,
  recipient: string,
): Promise<void> {
  const { handle, optionsAccountId } = snap;
  if (!handle) throw new Error("no active session");
  if (!optionsAccountId) throw new Error("no options account");
  if (!SESSION_REGISTRY_ID) throw new Error("no session registry configured");
  if (!handle.canRevoke) {
    throw new Error("reconnect your wallet to authorize a withdrawal");
  }
  set({ busy: "awaiting wallet signature" });
  try {
    // Prompt the host wallet to root-sign this specific withdrawal.
    const auth = await handle.signWithdraw({
      accountId: optionsAccountId,
      coinType,
      amount: amountRaw,
      recipient,
    });
    set({ busy: "withdrawing" });
    await handle.execute((tx, ctx) => {
      const args = [
        tx.object(optionsAccountId),
        tx.object(ctx.accountId), // the session (siws) account holds the root identity
        tx.object(SESSION_REGISTRY_ID!),
        tx.object(SUI_CLOCK_OBJECT_ID),
        tx.pure.u64(amountRaw),
        tx.pure.address(recipient),
        tx.pure.vector("u8", Array.from(auth.signature)),
        tx.pure.vector("u8", Array.from(auth.nonce)),
        tx.pure.u64(BigInt(auth.expiresAtMs)),
      ];
      if (auth.scheme === "ethereum") {
        tx.moveCall({
          target: `${PACKAGE_ID}::session_account::withdraw_with_root_sig_eth`,
          typeArguments: [coinType],
          arguments: [
            ...args,
            tx.pure.u64(BigInt(auth.chainId)),
            tx.pure.vector("u8", Array.from(new TextEncoder().encode(auth.issuedAt))),
          ],
        });
      } else {
        tx.moveCall({
          target: `${PACKAGE_ID}::session_account::withdraw_with_root_sig`,
          typeArguments: [coinType],
          arguments: args,
        });
      }
    });
    set({ busy: null });
    posthog.capture("custody_withdrawn", {
      coin_type: coinType,
      amount_raw: amountRaw.toString(),
    });
    await refreshSession();
  } catch (err) {
    posthog.captureException(err, { action: "custody_withdraw", coin_type: coinType });
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
