// SDK surface (spec §2.1, §2.3, §2.4, §2.6):
//   createSession(opts)        -> SessionHandle  (Solana / ed25519 root)
//   createSessionEth(opts)     -> SessionHandle  (Ethereum / SIWE root)
//   SessionHandle.execute(fn)  -> auto-signed, sponsored app call
//   SessionHandle.revoke()     -> root-signed generation bump
//   SessionHandle.status()     -> { expiresAt, limits (per-type spend), … }
//   restoreSession(config)     -> SessionHandle | null

import { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import type { SuiTransactionBlockResponse } from "./client.js";
import {
  canonicalCoinType,
  serializeRevokeMessage,
  serializeSessionMessage,
  serializeWithdrawMessage,
  type SpendLimit,
} from "./message.js";
import { fetchGeneration, readGeneration, readSpent, resolveAccountId } from "./reads.js";
import {
  buildSiweRevokeMessage,
  buildSiweSessionMessage,
  buildSiweWithdrawMessage,
  ethAddressToBytes,
  ethSignatureToSui,
} from "./siwe.js";
import {
  makeSessionSigner,
  sessionSignerFromCryptoKey,
  type SessionSigner,
} from "./signer.js";
import { clearSession, loadSession, saveSession } from "./store.js";
import type {
  CreateSessionOptions,
  EthereumSignMessage,
  Network,
  SessionConfig,
  SessionStatus,
  SolanaSignMessage,
  SpendLimitStatus,
} from "./types.js";

export class SessionExpiredError extends Error {
  constructor() {
    super("session expired");
    this.name = "SessionExpiredError";
  }
}

/** Root identity + (optionally attached) root wallet, discriminated by scheme. */
type RootState =
  | { scheme: "solana"; wallet?: SolanaSignMessage; identity: Uint8Array }
  | {
      scheme: "ethereum";
      wallet?: EthereumSignMessage;
      identity: Uint8Array;
      chainId: number;
    };

function randomNonce(): Uint8Array {
  return crypto.getRandomValues(new Uint8Array(32));
}

/**
 * A host-wallet-signed authorization for one external withdrawal. The app
 * passes these fields to its `withdraw_with_root_sig[_eth]` entrypoint; the tx
 * is still session-signed + sponsored, but the *authority* is this root
 * signature, so a compromised session key cannot withdraw on its own.
 */
export type WithdrawAuth =
  | {
      scheme: "solana";
      coinType: string;
      amount: bigint;
      recipient: string;
      signature: Uint8Array;
      nonce: Uint8Array;
      expiresAtMs: number;
    }
  | {
      scheme: "ethereum";
      coinType: string;
      amount: bigint;
      recipient: string;
      signature: Uint8Array;
      nonce: Uint8Array;
      expiresAtMs: number;
      chainId: number;
      issuedAt: string;
    };

export interface SignWithdrawParams {
  /** The options custody Account object id funds leave. */
  accountId: string;
  /** Coin type to withdraw (any 0x form; canonicalized internally). */
  coinType: string;
  amount: bigint | number;
  /** Recipient Sui address (bound by the signature). */
  recipient: string;
  /** Signature validity window; default 60s. */
  ttlMs?: number;
}

const enc = new TextEncoder();

function toBytes(strings: string[]): number[][] {
  return strings.map((s) => Array.from(enc.encode(s)));
}

/** Normalize every limit's coin type to the canonical on-chain form once. */
function canonicalizeLimits(limits: SpendLimit[]): SpendLimit[] {
  return limits.map((l) => ({
    coinType: canonicalCoinType(l.coinType),
    perTx: BigInt(l.perTx),
    total: BigInt(l.total),
  }));
}

/** Append the limit args (parallel vectors, entry-fun friendly) to a call. */
function limitArgs(tx: Transaction, limits: SpendLimit[]) {
  return [
    tx.pure.vector("vector<u8>", toBytes(limits.map((l) => l.coinType))),
    tx.pure.vector("u64", limits.map((l) => l.perTx)),
    tx.pure.vector("u64", limits.map((l) => l.total)),
  ];
}

function findCreatedObject(
  result: SuiTransactionBlockResponse,
  typeSuffix: string,
): string | null {
  for (const change of result.objectChanges ?? []) {
    if (change.type === "created" && change.objectType?.endsWith(typeSuffix)) {
      return change.objectId;
    }
  }
  return null;
}

interface HandleParams extends SessionConfig {
  signer: SessionSigner;
  root: RootState;
  capId: string;
  accountId: string;
  generation: number;
  expiresAtMs: number;
  limits: SpendLimit[];
  allowed: string[];
  /** Digest of the sign-in tx (undefined for restored sessions). */
  openDigest?: string;
}

export class SessionHandle {
  readonly capId: string;
  readonly accountId: string;
  readonly sessionKey: string;
  readonly expiresAtMs: number;
  readonly openDigest?: string;

  #p: HandleParams;

  constructor(params: HandleParams) {
    this.#p = params;
    this.capId = params.capId;
    this.accountId = params.accountId;
    this.sessionKey = params.signer.toSuiAddress();
    this.expiresAtMs = params.expiresAtMs;
    this.openDigest = params.openDigest;
  }

  get packageId(): string {
    return this.#p.packageId;
  }

  /** Root scheme of this session ("solana" or "ethereum"). */
  get scheme(): "solana" | "ethereum" {
    return this.#p.root.scheme;
  }

  /** The root identity bytes (32-byte ed25519 pubkey or 20-byte eth address). */
  get rootIdentity(): Uint8Array {
    return this.#p.root.identity;
  }

  /** The cap's per-type spend limits (canonical coin types). */
  get limits(): SpendLimit[] {
    return this.#p.limits;
  }

  /** True once a root wallet is attached — i.e. `revoke()` is callable. */
  get canRevoke(): boolean {
    return this.#p.root.wallet !== undefined;
  }

  /**
   * Attach the root wallet to a session restored from storage (restored
   * sessions have no wallet reference, so `revoke()` would throw). Call after
   * the user reconnects the same wallet.
   */
  attachRootWallet(wallet: SolanaSignMessage | EthereumSignMessage): void {
    this.#p.root.wallet = wallet as never;
  }

  /** Add a moveCall referencing `capId`/`accountId`, then sign+sponsor it. */
  async execute(
    build: (tx: Transaction, ctx: { capId: string; accountId: string }) => void,
  ): Promise<SuiTransactionBlockResponse> {
    if (Date.now() > this.expiresAtMs) throw new SessionExpiredError();
    const tx = new Transaction();
    build(tx, { capId: this.capId, accountId: this.accountId });
    return this.#p.sponsor.executeSponsored(tx, this.#p.signer);
  }

  async status(): Promise<SessionStatus> {
    const { client, packageId } = this.#p;
    const accountGeneration = await readGeneration(client, this.accountId);
    const limits: SpendLimitStatus[] = await Promise.all(
      this.#p.limits.map(async (l) => {
        const spent = await readSpent(
          client,
          packageId,
          this.accountId,
          this.capId,
          l.coinType,
        );
        return {
          coinType: l.coinType,
          perTx: l.perTx,
          total: l.total,
          spent,
          remaining: l.total > spent ? l.total - spent : 0n,
        };
      }),
    );
    const active =
      Date.now() < this.expiresAtMs && accountGeneration === this.#p.generation;
    return {
      capId: this.capId,
      accountId: this.accountId,
      expiresAt: this.expiresAtMs,
      limits,
      generation: this.#p.generation,
      accountGeneration,
      active,
    };
  }

  /**
   * Root-sign an external withdrawal of `amount` of `coinType` from the given
   * custody account to `recipient`. Prompts the HOST wallet (Solana/Ethereum)
   * — this is the one action a session key cannot self-authorize. Returns the
   * signature + nonce + expiry; the caller passes them to the app's
   * `withdraw_with_root_sig[_eth]` entrypoint and submits via `execute()`
   * (still session-signed + sponsored). The recipient and amount are bound by
   * the signature, so the sponsor/session key cannot redirect or resize it.
   */
  async signWithdraw(params: SignWithdrawParams): Promise<WithdrawAuth> {
    const { root, registryId } = this.#p;
    if (!root.wallet) {
      throw new Error("withdrawal requires the root wallet (not available after restore)");
    }
    const nonce = randomNonce();
    const expiresAtMs = Date.now() + (params.ttlMs ?? 60_000);
    const coinType = canonicalCoinType(params.coinType);
    const amount = BigInt(params.amount);

    if (root.scheme === "solana") {
      const message = serializeWithdrawMessage({
        domain: registryId,
        network: this.#p.network,
        solanaPk: root.identity,
        accountId: params.accountId,
        coinType,
        amount,
        recipient: params.recipient,
        nonce,
        expiresAtMs,
      });
      const { signature } = await root.wallet.signMessage(message);
      return { scheme: "solana", coinType, amount, recipient: params.recipient, signature, nonce, expiresAtMs };
    }

    const issuedAt = new Date().toISOString();
    const message = buildSiweWithdrawMessage({
      registryDomain: registryId,
      ethAddress: root.identity,
      accountId: params.accountId,
      coinType,
      amount,
      recipient: params.recipient,
      nonce,
      expiresAtMs,
      chainId: root.chainId,
      issuedAt,
    });
    const signature = ethSignatureToSui(await root.wallet.personalSign(message));
    return {
      scheme: "ethereum",
      coinType,
      amount,
      recipient: params.recipient,
      signature,
      nonce,
      expiresAtMs,
      chainId: root.chainId,
      issuedAt,
    };
  }

  /** Root-signed revocation: bumps generation, killing every cap at once. */
  async revoke(): Promise<SuiTransactionBlockResponse> {
    const { root, registryId, packageId, sponsor, client } = this.#p;
    if (!root.wallet) {
      throw new Error("revoke requires the root wallet (not available after restore)");
    }
    const nonce = randomNonce();
    const expiresAtMs = Date.now() + 60_000;
    const tx = new Transaction();

    if (root.scheme === "solana") {
      const message = serializeRevokeMessage({
        domain: registryId,
        network: this.#p.network,
        solanaPk: root.identity,
        accountId: this.accountId,
        nonce,
        expiresAtMs,
      });
      const { signature } = await root.wallet.signMessage(message);
      tx.moveCall({
        target: `${packageId}::session::revoke_all`,
        arguments: [
          tx.object(registryId),
          tx.object(this.accountId),
          tx.object(SUI_CLOCK_OBJECT_ID),
          tx.pure.vector("u8", Array.from(root.identity)),
          tx.pure.vector("u8", Array.from(signature)),
          tx.pure.vector("u8", Array.from(nonce)),
          tx.pure.u64(BigInt(expiresAtMs)),
        ],
      });
    } else {
      // The contract binds the account's CURRENT generation into the message.
      const generation = await readGeneration(client, this.accountId);
      const issuedAt = new Date().toISOString();
      const message = buildSiweRevokeMessage({
        registryDomain: registryId,
        ethAddress: root.identity,
        accountId: this.accountId,
        generation,
        nonce,
        expiresAtMs,
        chainId: root.chainId,
        issuedAt,
      });
      const sig = ethSignatureToSui(await root.wallet.personalSign(message));
      tx.moveCall({
        target: `${packageId}::session::revoke_all_eth`,
        arguments: [
          tx.object(registryId),
          tx.object(this.accountId),
          tx.object(SUI_CLOCK_OBJECT_ID),
          tx.pure.vector("u8", Array.from(root.identity)),
          tx.pure.vector("u8", Array.from(sig)),
          tx.pure.vector("u8", Array.from(nonce)),
          tx.pure.u64(BigInt(expiresAtMs)),
          tx.pure.u64(BigInt(root.chainId)),
          tx.pure.vector("u8", Array.from(enc.encode(issuedAt))),
        ],
      });
    }

    const res = await sponsor.executeSponsored(tx, this.#p.signer);
    await clearSession();
    return res;
  }
}

// --- internal: shared post-verify steps ---

async function finalizeOpen(
  config: SessionConfig,
  signer: SessionSigner,
  root: RootState,
  tx: Transaction,
  fields: {
    generation: number;
    expiresAtMs: number;
    limits: SpendLimit[];
    allowed: string[];
    persist?: boolean;
  },
): Promise<SessionHandle> {
  const result = await config.sponsor.executeSponsored(tx, signer);

  const capId = findCreatedObject(result, "::session::SessionCap");
  if (!capId) throw new Error("sign-in did not mint a SessionCap");
  const accountId = await resolveAccountId(config.client, config.registryId, root.identity);
  if (!accountId) throw new Error("could not resolve the user's Account after sign-in");

  const handle = new SessionHandle({
    ...config,
    signer,
    root,
    capId,
    accountId,
    generation: fields.generation,
    expiresAtMs: fields.expiresAtMs,
    limits: fields.limits,
    allowed: fields.allowed,
    openDigest: result.digest,
  });

  // Persist ONLY when backed by a non-extractable key (spec §2.2).
  if (fields.persist) {
    if (signer.nonExtractable && signer.cryptoKey) {
      await saveSession({
        packageId: config.packageId,
        registryId: config.registryId,
        network: config.network,
        scheme: root.scheme,
        chainId: root.scheme === "ethereum" ? root.chainId : undefined,
        capId,
        accountId,
        sessionKey: signer.toSuiAddress(),
        identity: Array.from(root.identity),
        generation: fields.generation,
        expiresAtMs: fields.expiresAtMs,
        limits: fields.limits.map((l) => ({
          coinType: l.coinType,
          perTx: l.perTx.toString(),
          total: l.total.toString(),
        })),
        allowed: fields.allowed,
        cryptoKey: signer.cryptoKey,
      });
    } else {
      console.warn(
        "[siws-session] software key is not persistable; session will not survive reload",
      );
    }
  }

  return handle;
}

// --- Solana (ed25519) sign-in ---

export async function createSession(opts: CreateSessionOptions): Promise<SessionHandle> {
  const { client, network, packageId, registryId, solanaWallet } = opts;

  const signer = await makeSessionSigner();
  const sessionKey = signer.toSuiAddress();
  const solanaPk = await solanaWallet.getPublicKey();
  const nonce = randomNonce();
  const expiresAtMs = Date.now() + opts.ttlMs;
  const generation = await fetchGeneration(client, registryId, solanaPk);
  const limits = canonicalizeLimits(opts.limits);

  const message = serializeSessionMessage({
    domain: registryId,
    network,
    solanaPk,
    sessionKey,
    generation,
    nonce,
    expiresAtMs,
    limits,
  });
  const { signature } = await solanaWallet.signMessage(message);

  const tx = new Transaction();
  tx.moveCall({
    target: `${packageId}::session::verify_and_open_session`,
    arguments: [
      tx.object(registryId),
      tx.object(SUI_CLOCK_OBJECT_ID),
      tx.pure.vector("u8", Array.from(solanaPk)),
      tx.pure.vector("u8", Array.from(signature)),
      tx.pure.address(sessionKey),
      tx.pure.u64(BigInt(generation)),
      tx.pure.vector("u8", Array.from(nonce)),
      tx.pure.u64(BigInt(expiresAtMs)),
      ...limitArgs(tx, limits),
      tx.pure.vector("vector<u8>", toBytes(opts.allowed)),
    ],
  });

  return finalizeOpen(
    opts,
    signer,
    { scheme: "solana", wallet: solanaWallet, identity: solanaPk },
    tx,
    {
      generation,
      expiresAtMs,
      limits,
      allowed: opts.allowed,
      persist: opts.persist,
    },
  );
}

// --- Ethereum (EIP-4361 / SIWE) sign-in ---

export interface CreateSessionEthOptions extends SessionConfig {
  ethereumWallet: EthereumSignMessage;
  /** EVM chain id rendered in the SIWE message (e.g. 1, 11155111). */
  chainId: number;
  limits: SpendLimit[];
  ttlMs: number;
  allowed: string[];
  persist?: boolean;
}

export async function createSessionEth(
  opts: CreateSessionEthOptions,
): Promise<SessionHandle> {
  const { client, packageId, registryId, ethereumWallet, chainId } = opts;

  const signer = await makeSessionSigner();
  const sessionKey = signer.toSuiAddress();
  const ethAddress = ethAddressToBytes(await ethereumWallet.getAddress());
  const nonce = randomNonce();
  const expiresAtMs = Date.now() + opts.ttlMs;
  const issuedAt = new Date().toISOString();
  const generation = await fetchGeneration(client, registryId, ethAddress);
  const limits = canonicalizeLimits(opts.limits);

  const message = buildSiweSessionMessage({
    registryDomain: registryId,
    ethAddress,
    sessionKey,
    generation,
    nonce,
    expiresAtMs,
    chainId,
    issuedAt,
    limits,
  });
  const sig = ethSignatureToSui(await ethereumWallet.personalSign(message));

  const tx = new Transaction();
  tx.moveCall({
    target: `${packageId}::session::verify_and_open_session_eth`,
    arguments: [
      tx.object(registryId),
      tx.object(SUI_CLOCK_OBJECT_ID),
      tx.pure.vector("u8", Array.from(ethAddress)),
      tx.pure.vector("u8", Array.from(sig)),
      tx.pure.address(sessionKey),
      tx.pure.u64(BigInt(generation)),
      tx.pure.vector("u8", Array.from(nonce)),
      tx.pure.u64(BigInt(expiresAtMs)),
      tx.pure.u64(BigInt(chainId)),
      tx.pure.vector("u8", Array.from(enc.encode(issuedAt))),
      ...limitArgs(tx, limits),
      tx.pure.vector("vector<u8>", toBytes(opts.allowed)),
    ],
  });

  return finalizeOpen(
    opts,
    signer,
    { scheme: "ethereum", wallet: ethereumWallet, identity: ethAddress, chainId },
    tx,
    {
      generation,
      expiresAtMs,
      limits,
      allowed: opts.allowed,
      persist: opts.persist,
    },
  );
}

/**
 * Rebuild a SessionHandle from a persisted non-extractable session, or null if
 * none / expired / revoked / software-key. Pass the matching root wallet (or
 * attach it later via `attachRootWallet`) if you want `revoke()` to work.
 */
export async function restoreSession(
  config: SessionConfig & {
    solanaWallet?: SolanaSignMessage;
    ethereumWallet?: EthereumSignMessage;
  },
): Promise<SessionHandle | null> {
  const p = await loadSession();
  if (!p || !p.cryptoKey) return null;
  if (Date.now() > p.expiresAtMs) {
    await clearSession();
    return null;
  }

  const signer = await sessionSignerFromCryptoKey(p.cryptoKey);
  if (signer.toSuiAddress() !== p.sessionKey) {
    await clearSession();
    return null;
  }

  // Confirm the cap hasn't been revoked out from under us.
  const accountGeneration = await readGeneration(config.client, p.accountId);
  if (accountGeneration !== p.generation) {
    await clearSession();
    return null;
  }

  const identity = Uint8Array.from(p.identity);
  const root: RootState =
    p.scheme === "ethereum"
      ? {
          scheme: "ethereum",
          wallet: config.ethereumWallet,
          identity,
          chainId: p.chainId ?? 1,
        }
      : { scheme: "solana", wallet: config.solanaWallet, identity };

  return new SessionHandle({
    client: config.client,
    network: config.network as Network,
    packageId: config.packageId,
    registryId: config.registryId,
    sponsor: config.sponsor,
    signer,
    root,
    capId: p.capId,
    accountId: p.accountId,
    generation: p.generation,
    expiresAtMs: p.expiresAtMs,
    limits: p.limits.map((l) => ({
      coinType: l.coinType,
      perTx: BigInt(l.perTx),
      total: BigInt(l.total),
    })),
    allowed: p.allowed,
  });
}
