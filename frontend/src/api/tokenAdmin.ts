// Token catalog admin client — the authenticated mutate side of token-info.
//
// Reads go to the open `GET /tokens`; writes (`POST` / `DELETE`) carry the
// admin JWT and hit token-info's public mutate routes, which delegate auth to
// auth-service. Reuses TOKEN_INFO_URL (same base as `initConfig`).

import { TOKEN_INFO_URL } from "../config";

const base = TOKEN_INFO_URL.replace(/\/$/, "");

/** Wire shape of a token as token-info serializes it (snake_case). */
export type TokenDto = {
  coin_type: string;
  ticker: string;
  name: string;
  logo_uri?: string | null;
  decimals: number;
  pyth_feed_id?: string | null;
  enabled: boolean;
};

/** Fields accepted by `POST /tokens`. */
export type TokenInput = {
  coin_type: string;
  ticker: string;
  name: string;
  logo_uri?: string | null;
  decimals: number;
  pyth_feed_id?: string | null;
  enabled: boolean;
};

/** Thrown when the JWT is missing/expired/invalid (HTTP 401/403). */
export class AuthExpiredError extends Error {}

/** Full catalog (enabled and disabled), as served by `GET /tokens`. */
export async function listTokens(): Promise<TokenDto[]> {
  const res = await fetch(`${base}/tokens`);
  if (!res.ok) throw new Error(`token-info /tokens → ${res.status}`);
  return (await res.json()) as TokenDto[];
}

async function mutate(
  token: string,
  path: string,
  method: "POST" | "DELETE",
  body?: unknown,
): Promise<Response> {
  const res = await fetch(`${base}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${token}`,
      ...(body ? { "content-type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (res.status === 401 || res.status === 403) {
    throw new AuthExpiredError(await res.text());
  }
  if (!res.ok) {
    throw new Error(`${method} ${path} → ${res.status}: ${await res.text()}`);
  }
  return res;
}

/** Add or replace a supported token. */
export async function createToken(token: string, input: TokenInput): Promise<TokenDto> {
  const res = await mutate(token, "/tokens", "POST", input);
  return (await res.json()) as TokenDto;
}

/** Remove a supported token by coin type. */
export async function deleteToken(token: string, coinType: string): Promise<void> {
  await mutate(token, `/tokens/${encodeURIComponent(coinType)}`, "DELETE");
}
