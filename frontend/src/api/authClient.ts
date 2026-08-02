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
  /** Only wallet-opened sessions carry an address, which is all this client
   *  opens. Password sessions (the Dakota dashboard) leave it unset. */
  address?: string;
  user_id: string;
  role: string;
  scope?: string;
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
 *  straight from dapp-kit's `signPersonalMessage` result (both base64). */
export async function login(signature: string, bytes: string): Promise<TokenResp> {
  const res = await fetch(`${base}/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ signature, bytes }),
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

/** `0x…` wallet address from a JWT, or null.
 *
 *  Reads the `address` claim. `sub` used to hold the address but now holds the
 *  account uuid — an account can be reached by wallet OR password, so the
 *  address is one identity among several rather than the subject itself. */
export function jwtSubject(token: string): string | null {
  return jwtClaims(token)?.address ?? null;
}

/** Role from a JWT (`admin` | `business` | `individual`), or null. */
export function jwtRole(token: string): string | null {
  return jwtClaims(token)?.role ?? null;
}

type Claims = { sub?: string; role?: string; scope?: string; address?: string; exp?: number };

function jwtClaims(token: string): Claims | null {
  try {
    const payload = token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/");
    return JSON.parse(atob(payload)) as Claims;
  } catch {
    return null;
  }
}
