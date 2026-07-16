// Solana→Sui leg of the CCTP bridge: builds the deposit_for_burn tx directly
// against Circle's TokenMessengerMinter and sends it via Phantom's
// signAndSendTransaction.
//
// We call Circle directly rather than through a wrapper program: the wrapper
// existed to own the message and emit its own event, neither of which anything
// consumes. Account order below mirrors Circle's `DepositForBurnContext`
// (Anchor 0.28), with the event-CPI pair (event_authority, program) appended.
// `owner` appears twice — once read-only as the burn authority, once writable
// as `event_rent_payer`; the runtime unions the privileges.

import { Buffer } from "buffer";

import {
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
} from "@solana/web3.js";

import type { CctpConfig } from "../api/cctpConfig";

const TOKEN_PROGRAM = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROGRAM = new PublicKey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

// sha256("global:deposit_for_burn")[..8] — Circle's Anchor 0.28 discriminator.
const DEPOSIT_FOR_BURN_DISCRIMINATOR = Uint8Array.from([
  215, 60, 61, 46, 114, 55, 128, 176,
]);

// --- Phantom (transaction-signing surface; the message-signing surface used
// for session login lives in ../session/wallets.ts) ---

interface PhantomTxProvider {
  isPhantom?: boolean;
  publicKey: { toBytes(): Uint8Array } | null;
  connect(): Promise<{ publicKey: { toBytes(): Uint8Array } }>;
  signAndSendTransaction(tx: Transaction): Promise<{ signature: string }>;
}

function phantomProvider(): PhantomTxProvider | null {
  const p = (window as unknown as { solana?: PhantomTxProvider }).solana;
  return p?.isPhantom ? p : null;
}

export function hasPhantom(): boolean {
  return phantomProvider() !== null;
}

/** Connect Phantom and return the wallet's base58 address. */
export async function connectPhantomWallet(): Promise<string> {
  const provider = phantomProvider();
  if (!provider) {
    throw new Error("Phantom wallet not found — install it from phantom.app");
  }
  const { publicKey } = await provider.connect();
  return new PublicKey(publicKey.toBytes()).toBase58();
}

/** The wallet's associated USDC token account. */
export function deriveUsdcAta(owner: PublicKey, usdcMint: string): PublicKey {
  return PublicKey.findProgramAddressSync(
    [owner.toBytes(), TOKEN_PROGRAM.toBytes(), new PublicKey(usdcMint).toBytes()],
    ATA_PROGRAM,
  )[0];
}

export type SolanaDepositForBurnParams = {
  /** USDC to bridge, in base units (6 decimals). */
  amountRaw: bigint;
  /** Destination Sui address (0x-prefixed 32-byte hex). */
  suiRecipientHex: string;
};

/**
 * Build the deposit_for_burn tx, sign it with Phantom (plus the fresh
 * message-sent-event-data keypair Circle requires), send it, and return the
 * signature.
 */
export async function sendSolanaDepositForBurn(
  cctp: CctpConfig,
  p: SolanaDepositForBurnParams,
): Promise<{ signature: string; wallet: string }> {
  const provider = phantomProvider();
  if (!provider) {
    throw new Error("Phantom wallet not found — install it from phantom.app");
  }
  const { publicKey } = await provider.connect();
  const owner = new PublicKey(publicKey.toBytes());

  const tmm = new PublicKey(cctp.solana.tokenMessengerProgram);
  const mt = new PublicKey(cctp.solana.messageTransmitterProgram);
  const usdcMint = new PublicKey(cctp.solana.usdcMint);

  const pda = (seeds: (Uint8Array | Buffer)[], program: PublicKey) =>
    PublicKey.findProgramAddressSync(seeds, program)[0];
  const utf8 = (s: string) => new TextEncoder().encode(s);

  const senderAuthority = pda([utf8("sender_authority")], tmm);
  const messageTransmitter = pda([utf8("message_transmitter")], mt);
  const tokenMessenger = pda([utf8("token_messenger")], tmm);
  const remoteTokenMessenger = pda(
    [utf8("remote_token_messenger"), utf8(String(cctp.domainSui))],
    tmm,
  );
  const tokenMinter = pda([utf8("token_minter")], tmm);
  const localToken = pda([utf8("local_token"), usdcMint.toBytes()], tmm);
  const tmmEventAuthority = pda([utf8("__event_authority")], tmm);
  const burnTokenAccount = deriveUsdcAta(owner, cctp.solana.usdcMint);

  // Fresh throwaway account Circle stores the MessageSent event data in;
  // must co-sign the tx.
  const messageSentEventData = Keypair.generate();

  // Borsh args: amount u64 LE | destination_domain u32 LE | mint_recipient 32B.
  const suiRecipient = hexToBytes32(p.suiRecipientHex);
  const data = new Uint8Array(8 + 8 + 4 + 32);
  data.set(DEPOSIT_FOR_BURN_DISCRIMINATOR, 0);
  new DataView(data.buffer).setBigUint64(8, p.amountRaw, true);
  new DataView(data.buffer).setUint32(16, cctp.domainSui, true);
  data.set(suiRecipient, 20);

  const ix = new TransactionInstruction({
    programId: tmm,
    keys: [
      { pubkey: owner, isSigner: true, isWritable: false },
      { pubkey: owner, isSigner: true, isWritable: true }, // event_rent_payer
      { pubkey: senderAuthority, isSigner: false, isWritable: false },
      { pubkey: burnTokenAccount, isSigner: false, isWritable: true },
      { pubkey: messageTransmitter, isSigner: false, isWritable: true },
      { pubkey: tokenMessenger, isSigner: false, isWritable: false },
      { pubkey: remoteTokenMessenger, isSigner: false, isWritable: false },
      { pubkey: tokenMinter, isSigner: false, isWritable: false },
      { pubkey: localToken, isSigner: false, isWritable: true },
      { pubkey: usdcMint, isSigner: false, isWritable: true },
      { pubkey: messageSentEventData.publicKey, isSigner: true, isWritable: true },
      { pubkey: mt, isSigner: false, isWritable: false },
      { pubkey: tmm, isSigner: false, isWritable: false },
      { pubkey: TOKEN_PROGRAM, isSigner: false, isWritable: false },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      { pubkey: tmmEventAuthority, isSigner: false, isWritable: false },
      { pubkey: tmm, isSigner: false, isWritable: false },
    ],
    data: Buffer.from(data),
  });

  const connection = new Connection(cctp.solana.rpcUrl, "confirmed");
  const { blockhash } = await connection.getLatestBlockhash("confirmed");

  const tx = new Transaction({ feePayer: owner, recentBlockhash: blockhash });
  tx.add(ix);
  tx.partialSign(messageSentEventData);

  const { signature } = await provider.signAndSendTransaction(tx);
  return { signature, wallet: owner.toBase58() };
}

/** 0x-prefixed (or bare) hex → exactly 32 bytes. */
function hexToBytes32(hex: string): Uint8Array {
  const clean = hex.replace(/^0x/, "").padStart(64, "0");
  if (clean.length !== 64 || /[^0-9a-fA-F]/.test(clean)) {
    throw new Error(`bad 32-byte hex: ${hex}`);
  }
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  return out;
}
