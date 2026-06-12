// Reload persistence (spec §2.2, §2.6).
//
// We persist ONLY a non-extractable CryptoKeyPair handle plus public session
// metadata. A `CryptoKeyPair` whose private key is non-extractable can be
// structured-cloned into IndexedDB without ever exposing the secret to JS — the
// classic theft path (raw secrets in localStorage) is avoided entirely.
//
// Software (extractable) keypairs are deliberately NOT persisted; restoring
// them would require storing the raw secret, so those sessions die on reload.

export interface PersistedSpendLimit {
  /** Canonical coin type. */
  coinType: string;
  perTx: string;
  total: string;
}

export interface PersistedSession {
  packageId: string;
  registryId: string;
  network: string;
  /** Root scheme — determines which revoke path to use. */
  scheme: "solana" | "ethereum";
  /** Ethereum chain id (ethereum scheme only). */
  chainId?: number;
  capId: string;
  accountId: string;
  sessionKey: string;
  /** Root identity bytes: 32-byte ed25519 pubkey or 20-byte eth address. */
  identity: number[];
  generation: number;
  expiresAtMs: number;
  limits: PersistedSpendLimit[];
  allowed: string[];
  /** Present only for non-extractable WebCrypto sessions. */
  cryptoKey?: CryptoKeyPair;
}

const DB_NAME = "siws-session";
const STORE = "sessions";
const KEY = "current";

function hasIndexedDb(): boolean {
  return typeof indexedDB !== "undefined";
}

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function tx<T>(mode: IDBTransactionMode, run: (s: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  return openDb().then(
    (db) =>
      new Promise<T>((resolve, reject) => {
        const req = run(db.transaction(STORE, mode).objectStore(STORE));
        req.onsuccess = () => resolve(req.result);
        req.onerror = () => reject(req.error);
      }),
  );
}

export async function saveSession(session: PersistedSession): Promise<void> {
  if (!hasIndexedDb()) return;
  await tx("readwrite", (s) => s.put(session, KEY));
}

export async function loadSession(): Promise<PersistedSession | null> {
  if (!hasIndexedDb()) return null;
  try {
    return (await tx<PersistedSession | undefined>("readonly", (s) => s.get(KEY))) ?? null;
  } catch {
    return null;
  }
}

export async function clearSession(): Promise<void> {
  if (!hasIndexedDb()) return;
  await tx("readwrite", (s) => s.delete(KEY));
}
