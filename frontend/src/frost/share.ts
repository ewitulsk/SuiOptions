// Curator FROST key-share custody (SO-305).
//
// The curator's DKG share (`key_package_b64`) is the ONLY counterpart of the
// hedge-signer's share: a Bluefin parent account cannot rotate keys, so a
// lost curator share permanently strands whatever the parent holds
// (doc 03 §3b key-loss posture). The share therefore NEVER touches disk in
// the clear: it is AES-GCM-encrypted under a passphrase-derived key
// (WebCrypto PBKDF2-SHA256), downloaded as a mandatory backup file before
// the keygen ceremony is allowed to complete, and optionally cached (still
// encrypted) in localStorage for passphrase-per-session unlock.

import { base64ToBytes, bytesToBase64 } from "./frost";

const KDF_ITERATIONS = 600_000;
const CACHE_PREFIX = "frost-share:";

export type ShareBackup = {
  v: 1;
  kind: "pismo-frost-curator-share";
  vaultId: string;
  /** The FROST group's derived Sui address (the Bluefin parent account). */
  parentAddress: string;
  groupPublicKeyHex: string;
  /** Public half — needed to aggregate/verify; not secret. */
  publicKeyPackageB64: string;
  createdAtMs: number;
  kdf: { name: "PBKDF2"; hash: "SHA-256"; iterations: number; saltB64: string };
  /** ciphertext = AES-GCM(key = KDF(passphrase), plaintext = key_package_b64). */
  cipher: { name: "AES-GCM"; ivB64: string; ciphertextB64: string };
};

async function deriveKey(
  passphrase: string,
  salt: Uint8Array,
  iterations: number,
): Promise<CryptoKey> {
  const material = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(passphrase),
    "PBKDF2",
    false,
    ["deriveKey"],
  );
  return crypto.subtle.deriveKey(
    { name: "PBKDF2", hash: "SHA-256", salt: salt as BufferSource, iterations },
    material,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
}

export async function encryptShare(
  keyPackageB64: string,
  passphrase: string,
  meta: {
    vaultId: string;
    parentAddress: string;
    groupPublicKeyHex: string;
    publicKeyPackageB64: string;
  },
): Promise<ShareBackup> {
  const salt = crypto.getRandomValues(new Uint8Array(16));
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const key = await deriveKey(passphrase, salt, KDF_ITERATIONS);
  const ciphertext = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv: iv as BufferSource },
    key,
    new TextEncoder().encode(keyPackageB64),
  );
  return {
    v: 1,
    kind: "pismo-frost-curator-share",
    ...meta,
    createdAtMs: Date.now(),
    kdf: { name: "PBKDF2", hash: "SHA-256", iterations: KDF_ITERATIONS, saltB64: bytesToBase64(salt) },
    cipher: { name: "AES-GCM", ivB64: bytesToBase64(iv), ciphertextB64: bytesToBase64(new Uint8Array(ciphertext)) },
  };
}

/** Decrypt a backup with the passphrase → the curator `key_package_b64`.
 * Throws on a wrong passphrase (AES-GCM authentication failure). */
export async function decryptShare(backup: ShareBackup, passphrase: string): Promise<string> {
  const key = await deriveKey(
    passphrase,
    base64ToBytes(backup.kdf.saltB64),
    backup.kdf.iterations,
  );
  let plaintext: ArrayBuffer;
  try {
    plaintext = await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: base64ToBytes(backup.cipher.ivB64) as BufferSource },
      key,
      base64ToBytes(backup.cipher.ciphertextB64) as BufferSource,
    );
  } catch {
    throw new Error("Wrong passphrase (or corrupted share file).");
  }
  return new TextDecoder().decode(plaintext);
}

/** Trigger a browser download of the encrypted backup file. */
export function downloadShareBackup(backup: ShareBackup): void {
  const blob = new Blob([JSON.stringify(backup, null, 2)], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = `frost-share-${backup.vaultId.slice(0, 10)}-${backup.parentAddress.slice(0, 10)}.json`;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

export function parseShareBackup(text: string): ShareBackup {
  const parsed = JSON.parse(text) as ShareBackup;
  if (parsed.kind !== "pismo-frost-curator-share" || parsed.v !== 1) {
    throw new Error("Not a curator share backup file.");
  }
  return parsed;
}

// ── encrypted localStorage cache (per vault) ────────────────────────────────

export function cacheShare(backup: ShareBackup): void {
  localStorage.setItem(CACHE_PREFIX + backup.vaultId, JSON.stringify(backup));
}

export function cachedShare(vaultId: string): ShareBackup | null {
  const raw = localStorage.getItem(CACHE_PREFIX + vaultId);
  if (!raw) return null;
  try {
    return parseShareBackup(raw);
  } catch {
    return null;
  }
}

export function clearCachedShare(vaultId: string): void {
  localStorage.removeItem(CACHE_PREFIX + vaultId);
}
