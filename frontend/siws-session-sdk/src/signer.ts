// Ephemeral Sui signer for the session key (spec §2.2).
//
// Preferred: a NON-EXTRACTABLE WebCrypto Ed25519 key — the private key never
// appears in JS memory, so it can't be exfiltrated by a malicious script. We
// wrap it in a Sui `Signer` whose `sign(digest)` delegates to
// `crypto.subtle.sign`. Sui signs the blake2b intent digest with raw Ed25519,
// and WebCrypto Ed25519 signs the given bytes directly — so this is a drop-in.
//
// Fallback: an in-memory software `Ed25519Keypair` (never persisted as a raw
// secret — see store.ts).

import { Signer } from "@mysten/sui/cryptography";
import { Ed25519Keypair, Ed25519PublicKey } from "@mysten/sui/keypairs/ed25519";

/** A session signer that can report whether its key material is exfiltratable. */
export interface SessionSigner extends Signer {
  /** True when backed by a non-extractable handle (WebCrypto). */
  readonly nonExtractable: boolean;
  /** The underlying CryptoKeyPair, if any — used by the persistence store. */
  readonly cryptoKey?: CryptoKeyPair;
}

class WebCryptoEd25519Signer extends Signer implements SessionSigner {
  readonly nonExtractable = true;
  readonly cryptoKey: CryptoKeyPair;
  #publicKey: Ed25519PublicKey;

  constructor(cryptoKey: CryptoKeyPair, publicKey: Ed25519PublicKey) {
    super();
    this.cryptoKey = cryptoKey;
    this.#publicKey = publicKey;
  }

  static async fromKeyPair(cryptoKey: CryptoKeyPair): Promise<WebCryptoEd25519Signer> {
    const raw = new Uint8Array(await crypto.subtle.exportKey("raw", cryptoKey.publicKey));
    return new WebCryptoEd25519Signer(cryptoKey, new Ed25519PublicKey(raw));
  }

  getKeyScheme() {
    return "ED25519" as const;
  }

  getPublicKey() {
    return this.#publicKey;
  }

  async sign(bytes: Uint8Array): Promise<Uint8Array<ArrayBuffer>> {
    const sig = await crypto.subtle.sign(
      "Ed25519",
      this.cryptoKey.privateKey,
      bytes as BufferSource,
    );
    return new Uint8Array(sig);
  }
}

class SoftwareSessionSigner extends Ed25519Keypair implements SessionSigner {
  readonly nonExtractable = false;
}

/** True if this runtime can mint non-extractable Ed25519 keys via WebCrypto. */
export async function webCryptoEd25519Available(): Promise<boolean> {
  try {
    if (typeof crypto === "undefined" || !crypto.subtle) return false;
    await crypto.subtle.generateKey({ name: "Ed25519" }, false, ["sign", "verify"]);
    return true;
  } catch {
    return false;
  }
}

/**
 * Generate the session signer, preferring the non-extractable WebCrypto path.
 * Falls back to a software in-memory keypair where WebCrypto Ed25519 is
 * unavailable (older browsers, Node without the flag).
 */
export async function makeSessionSigner(): Promise<SessionSigner> {
  if (await webCryptoEd25519Available()) {
    const kp = (await crypto.subtle.generateKey({ name: "Ed25519" }, false, [
      "sign",
      "verify",
    ])) as CryptoKeyPair;
    return WebCryptoEd25519Signer.fromKeyPair(kp);
  }
  return new SoftwareSessionSigner();
}

/** Rebuild a signer from a persisted non-extractable CryptoKeyPair (restore). */
export async function sessionSignerFromCryptoKey(
  cryptoKey: CryptoKeyPair,
): Promise<SessionSigner> {
  return WebCryptoEd25519Signer.fromKeyPair(cryptoKey);
}
