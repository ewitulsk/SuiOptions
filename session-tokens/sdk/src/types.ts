// Shared types for the SDK surface.

import type { SuiRpcClient } from "./client.js";
import type { SponsorClient } from "./sponsor.js";

export type Network = "mainnet" | "testnet" | "devnet" | "localnet";

/**
 * The "Sign In With" root wallet (Phantom, etc.). Only ed25519 roots are
 * supported by the current Move verify path; a secp256k1 root would dispatch to
 * `ecdsa_k1` instead (spec §4.5).
 */
export interface SolanaSignMessage {
  /** Raw 32-byte ed25519 public key. */
  getPublicKey(): Uint8Array | Promise<Uint8Array>;
  /** Sign raw message bytes; returns the raw 64-byte signature. */
  signMessage(message: Uint8Array): Promise<{ signature: Uint8Array }>;
}

/**
 * The "Sign In With Ethereum" root wallet (MetaMask, etc.). The root identity
 * is a 20-byte Ethereum address; signing is EIP-191 `personal_sign` over the
 * canonical EIP-4361 message (secp256k1, verified on-chain via ecrecover).
 */
export interface EthereumSignMessage {
  /** Checksummed or lowercase "0x…" address. */
  getAddress(): string | Promise<string>;
  /** personal_sign a message string; returns 0x + 65-byte signature. */
  personalSign(message: string): Promise<string>;
}

export type RootScheme = "solana" | "ethereum";

/** Deployment + policy config shared by every SDK entry point. */
export interface SessionConfig {
  client: SuiRpcClient;
  network: Network;
  /** Published Move package id. */
  packageId: string;
  /** Shared Registry object id (also the message `domain`). */
  registryId: string;
  /** Fully-qualified coin type backing the Account, e.g. "0x2::sui::SUI". */
  coinType: string;
  /** Pays gas + co-signs. Cannot move funds. */
  sponsor: SponsorClient;
}

export interface CreateSessionOptions extends SessionConfig {
  solanaWallet: SolanaSignMessage;
  /** Max cumulative spend over the cap's life. */
  spendCap: bigint;
  /** Per-transaction max. */
  perTxCap: bigint;
  /** Cap lifetime in milliseconds. Keep short. */
  ttlMs: number;
  /** Full `pkg::module::function` selectors this cap may call. */
  allowed: string[];
  /** Persist the (non-extractable) handle so the session survives reloads. */
  persist?: boolean;
}

export interface SessionStatus {
  capId: string;
  accountId: string;
  expiresAt: number;
  spent: bigint;
  remaining: bigint;
  /** Cap generation; compare with `accountGeneration` to detect revocation. */
  generation: number;
  accountGeneration: number;
  active: boolean;
}
