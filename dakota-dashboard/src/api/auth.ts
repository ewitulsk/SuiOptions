// Client for auth-service.
//
// One account can be reached by several login methods — a username+password
// and a Sui wallet both resolve to the same `user_id`, and either can be added
// to an account that started with the other. The JWT that comes back carries
// `role` and `scope`, which is what every screen in this app keys off.

import { AUTH_API } from "../config";

export type Role = "admin" | "business" | "individual";

export type Session = {
  token: string;
  user_id: string;
  role: Role;
  scope?: string;
  address?: string;
  expires_in: number;
};

export type Identity = {
  id: string;
  kind: "password" | "sui_wallet";
  identifier: string;
  created_at: string;
  last_used_at?: string;
};

export type Me = {
  user_id: string;
  role: Role;
  scope?: string;
  identities: Identity[];
};

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${AUTH_API}${path}`, {
    ...init,
    headers: { "content-type": "application/json", ...(init?.headers ?? {}) },
  });
  if (!res.ok) {
    // auth-service returns a bare string body on error; it is written to be
    // shown to a person, so pass it straight through.
    throw new Error((await res.text()) || `auth ${path} → ${res.status}`);
  }
  return res.status === 204 ? (undefined as T) : ((await res.json()) as T);
}

export const loginWithPassword = (username: string, password: string) =>
  call<Session>("/login/password", {
    method: "POST",
    body: JSON.stringify({ username, password }),
  });

export const fetchChallenge = () =>
  call<{ message: string }>("/challenge").then((r) => r.message);

/** `signature` and `bytes` come straight from dapp-kit's signPersonalMessage. */
export const loginWithWallet = (signature: string, bytes: string) =>
  call<Session>("/login", {
    method: "POST",
    body: JSON.stringify({ signature, bytes }),
  });

export type RegisterMethod =
  | { username: string; password: string }
  | { signature: string; bytes: string };

export const register = (invite: string, method: RegisterMethod) =>
  call<Session>("/register", {
    method: "POST",
    body: JSON.stringify({ invite, ...method }),
  });

export const previewInvite = (invite: string) =>
  call<{ role: Role; label?: string; valid: boolean; reason: string | null }>(
    `/invites/preview?invite=${encodeURIComponent(invite)}`,
  );

export const me = (token: string) =>
  call<Me>("/me", { headers: bearer(token) });

export const addIdentity = (token: string, method: RegisterMethod) =>
  call<Identity>("/identities", {
    method: "POST",
    headers: bearer(token),
    body: JSON.stringify(method),
  });

export const removeIdentity = (token: string, id: string) =>
  call<void>(`/identities/${id}`, { method: "DELETE", headers: bearer(token) });

export const refresh = (token: string) =>
  call<Session>("/refresh", { method: "POST", headers: bearer(token) });

const bearer = (token: string) => ({ authorization: `Bearer ${token}` });

// --- persistence -------------------------------------------------------------

const KEY = "dakota-session";

export function storeSession(s: Session) {
  try {
    localStorage.setItem(KEY, JSON.stringify(s));
  } catch {
    /* private browsing — the session just won't survive a reload */
  }
}

export function loadSession(): Session | null {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return null;
    const s = JSON.parse(raw) as Session;
    // A token past its expiry is worse than none: it produces confusing 401s
    // on every screen instead of a clean redirect to login.
    return jwtExp(s.token) > Date.now() / 1000 ? s : null;
  } catch {
    return null;
  }
}

export function clearSession() {
  try {
    localStorage.removeItem(KEY);
  } catch {
    /* ignore */
  }
}

export function jwtExp(token: string): number {
  try {
    const payload = token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/");
    return (JSON.parse(atob(payload)) as { exp?: number }).exp ?? 0;
  } catch {
    return 0;
  }
}
