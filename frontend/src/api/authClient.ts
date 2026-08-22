// Client for the auth-service (challenge-response admin login → JWT).
//
// Flow: GET /challenge returns a message; the wallet signs it with
// `signPersonalMessage`; POST /login returns a JWT bound to the caller's IP.
// POST /refresh swaps a still-in-window token for a fresh one from the same IP
// (no re-signing). The JWT then authorizes token-info's mutate endpoints.

import { AUTH_URL } from "../config";

const base = AUTH_URL.replace(/\/$/, "");

export type TokenResp = {
  token: string;
  address: string;
  expires_in: number;
};

/** Mint a single-use challenge message to sign. */
export async function fetchChallenge(): Promise<string> {
  const res = await fetch(`${base}/challenge`);
  if (!res.ok) throw new Error(`auth /challenge → ${res.status}`);
  const body = (await res.json()) as { message: string };
  return body.message;
}

/** Exchange a signed challenge for a JWT. `signature` and `bytes` come
 *  straight from dapp-kit's `signPersonalMessage` result (both base64).
 *  `address` is the connected wallet — REQUIRED for zkLogin signatures
 *  (their address is not recoverable server-side); a cross-check for
 *  every other scheme. */
export async function login(
  signature: string,
  bytes: string,
  address?: string,
): Promise<TokenResp> {
  const res = await fetch(`${base}/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ signature, bytes, address }),
  });
  if (!res.ok) {
    throw new Error(`login failed (${res.status}): ${await res.text()}`);
  }
  return (await res.json()) as TokenResp;
}

/** Swap a token for a fresh one (same IP, no re-signing). */
export async function refresh(token: string): Promise<TokenResp> {
  const res = await fetch(`${base}/refresh`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}` },
  });
  if (!res.ok) {
    throw new Error(`refresh failed (${res.status}): ${await res.text()}`);
  }
  return (await res.json()) as TokenResp;
}

// --- token persistence -------------------------------------------------------

const STORAGE_KEY = "so-admin-jwt";

export function getStoredToken(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

export function setStoredToken(token: string): void {
  try {
    localStorage.setItem(STORAGE_KEY, token);
  } catch {
    /* ignore */
  }
}

export function clearStoredToken(): void {
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch {
    /* ignore */
  }
}

/** Unix-seconds expiry from a JWT payload, or 0 if unparseable. */
export function jwtExp(token: string): number {
  try {
    const payload = token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/");
    const json = JSON.parse(atob(payload)) as { exp?: number };
    return json.exp ?? 0;
  } catch {
    return 0;
  }
}

/** `0x…` subject (admin address) from a JWT, or null. */
export function jwtSubject(token: string): string | null {
  try {
    const payload = token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/");
    const json = JSON.parse(atob(payload)) as { sub?: string };
    return json.sub ?? null;
  } catch {
    return null;
  }
}
