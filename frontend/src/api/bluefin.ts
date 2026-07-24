// Bluefin Pro REST client (SO-305), routed through the hedge-signer's
// allowlisted /bluefin relay — Bluefin only serves CORS to its own origins,
// so the browser never calls their hosts directly (see
// rust-backend/services/hedge-signer/src/bluefin_proxy.rs for the decision).
//
// Signing model (verified against fireflyprotocol/pro-sdk — rust
// signature.rs + ts request-signer.ts, and mirrored fail-closed by
// hedge-signer's payload policy):
// - every privileged request carries a detached Sui personal-message
//   signature over a canonical JSON rendering of its "signed fields";
// - the login payload signs COMPACT JSON and travels in a
//   `payloadSignature` header; all other payloads sign PRETTY (2-space)
//   JSON with an exact key order and a `type` discriminator, and travel in
//   the request body next to `signedFields`;
// - the curator's wallet signs orders (authorized-wallet trading); the
//   FROST parent signs login/authorize/withdraw via the ceremony.
//
// NOTE: shapes are implemented from the SDK sources but have NOT been
// exercised against a live Bluefin account yet (none exists for our vaults
// until the setup wizard runs on staging) — the staging validation spike
// drives these paths first.

import { bluefinFetch } from "./hedgeSigner";

// ── exchange info (contract ids + markets) ──────────────────────────────────

export type BluefinMarket = {
  symbol: string;
  baseAssetSymbol: string;
  status: string;
  tickSizeE9: string;
  stepSizeE9: string;
  minOrderQuantityE9: string;
  defaultLeverageE9: string;
  isolatedOnly: boolean;
};

export type BluefinAsset = {
  symbol: string;
  assetType: string;
  decimals: number;
};

export type BluefinExchangeInfo = {
  assets: BluefinAsset[];
  markets: BluefinMarket[];
  contractsConfig: {
    baseContractAddress: string;
    currentContractAddress: string;
    edsId: string;
    idsId: string;
    network: string;
  };
};

let exchangeInfo: Promise<BluefinExchangeInfo> | null = null;

async function readJson<T>(res: Response, what: string): Promise<T> {
  if (!res.ok) {
    let detail = "";
    try {
      detail = await res.text();
    } catch {
      /* keep status only */
    }
    throw new Error(`bluefin ${what}: ${res.status} ${detail || res.statusText}`);
  }
  return (await res.json()) as T;
}

/** `GET /v1/exchange/info` (public), cached for the session — contract ids
 * (eds/ids/package) and the tradable markets. */
export function fetchBluefinExchangeInfo(): Promise<BluefinExchangeInfo> {
  if (!exchangeInfo) {
    exchangeInfo = bluefinFetch("data", "/v1/exchange/info")
      .then((res) => readJson<BluefinExchangeInfo>(res, "exchange info"))
      .catch((e) => {
        exchangeInfo = null; // allow retry after a transient failure
        throw e;
      });
  }
  return exchangeInfo;
}

// ── account (public read) ───────────────────────────────────────────────────

export type BluefinPosition = {
  symbol: string;
  side: string;
  sizeE9: string;
  avgEntryPriceE9: string;
  markPriceE9: string;
  liquidationPriceE9: string;
  unrealizedPnlE9: string;
  leverageE9?: string;
  isIsolated: boolean;
  fundingRatePaymentSinceOpenedE9?: string;
};

export type BluefinAccount = {
  totalAccountValueE9: string;
  crossEffectiveBalanceE9?: string;
  marginAvailableE9?: string;
  crossMarginRequiredE9?: string;
  totalUnrealizedPnlE9?: string;
  crossLeverageE9?: string;
  canTrade?: boolean;
  assets?: Array<{ symbol: string; quantityE9: string; maxWithdrawQuantityE9?: string }>;
  positions?: BluefinPosition[];
  authorizedWallets?: Array<string | { address?: string; wallet?: string }>;
  updatedAtMillis?: number;
};

/** `GET /api/v1/account?accountAddress=…` (public, no auth). Returns null
 * for an account Bluefin has never seen (materializes on first deposit). */
export async function fetchBluefinAccount(
  accountAddress: string,
): Promise<BluefinAccount | null> {
  const res = await bluefinFetch(
    "data",
    `/api/v1/account?accountAddress=${encodeURIComponent(accountAddress)}`,
  );
  if (res.status === 404) return null;
  const body = await readJson<BluefinAccount & { message?: string }>(res, "account");
  // The endpoint answers 200 {"message":"Account not found"} for unknowns.
  if (body.totalAccountValueE9 == null) return null;
  return body;
}

/** Normalize the authorizedWallets entries (string or object per SDK docs). */
export function authorizedWalletAddresses(account: BluefinAccount): string[] {
  return (account.authorizedWallets ?? []).map((w) =>
    typeof w === "string" ? w : (w.address ?? w.wallet ?? ""),
  );
}

// ── signable payloads (exact SDK shapes) ────────────────────────────────────

const utf8 = (s: string) => new TextEncoder().encode(s);

export function newSalt(): string {
  // SDK convention: epoch millis + randomness, as a decimal string.
  return String(Date.now() * 1000 + Math.floor(Math.random() * 1000));
}

/** Login payload: COMPACT JSON, signature goes in the payloadSignature
 * header, the same JSON is the POST body. */
export function loginPayload(accountAddress: string): { json: string; bytes: Uint8Array } {
  const json = JSON.stringify({
    accountAddress,
    signedAtMillis: Date.now(),
    audience: "api",
  });
  return { json, bytes: utf8(json) };
}

/** Authorize/deauthorize payload: PRETTY JSON, exact key order. */
export function authorizePayload(p: {
  idsId: string;
  parentAddress: string;
  userAddress: string;
  authorize: boolean;
}): { json: string; bytes: Uint8Array; salt: string; signedAtMillis: number } {
  const salt = newSalt();
  const signedAtMillis = Date.now();
  const json = JSON.stringify(
    {
      type: "Bluefin Pro Authorize Account",
      ids: p.idsId,
      account: p.parentAddress,
      user: p.userAddress,
      status: p.authorize,
      salt,
      signedAt: String(signedAtMillis),
    },
    null,
    2,
  );
  return { json, bytes: utf8(json), salt, signedAtMillis };
}

/** Withdraw payload: PRETTY JSON, exact key order. Amount is E9. */
export function withdrawPayload(p: {
  edsId: string;
  assetSymbol: string;
  parentAddress: string;
  amountE9: string;
}): { json: string; bytes: Uint8Array; salt: string; signedAtMillis: number } {
  const salt = newSalt();
  const signedAtMillis = Date.now();
  const json = JSON.stringify(
    {
      type: "Bluefin Pro Withdrawal",
      eds: p.edsId,
      assetSymbol: p.assetSymbol,
      account: p.parentAddress,
      amount: p.amountE9,
      salt,
      signedAt: String(signedAtMillis),
    },
    null,
    2,
  );
  return { json, bytes: utf8(json), salt, signedAtMillis };
}

export type OrderTicket = {
  symbol: string;
  /** Parent account the order trades for (the curator wallet signs). */
  accountAddress: string;
  side: "LONG" | "SHORT";
  type: "LIMIT" | "MARKET";
  /** E9 decimal string; "0" for MARKET orders. */
  priceE9: string;
  quantityE9: string;
  leverageE9: string;
  isIsolated: boolean;
  reduceOnly: boolean;
  postOnly?: boolean;
  timeInForce?: "GTT" | "IOC" | "FOK";
  expiresAtMillis: number;
};

/** Order payload: PRETTY JSON, exact key order (SDK `conversion::signable`).
 * `type`/`reduceOnly`/`postOnly`/`timeInForce` live OUTSIDE the signed
 * fields — they ride only in the REST body. */
export function orderPayload(
  idsId: string,
  t: OrderTicket,
): { json: string; bytes: Uint8Array; salt: string; signedAtMillis: number } {
  const salt = newSalt();
  const signedAtMillis = Date.now();
  const json = JSON.stringify(
    {
      type: "Bluefin Pro Order",
      ids: idsId,
      account: t.accountAddress,
      market: t.symbol,
      price: t.priceE9,
      quantity: t.quantityE9,
      leverage: t.leverageE9,
      side: t.side,
      positionType: t.isIsolated ? "ISOLATED" : "CROSS",
      expiration: String(t.expiresAtMillis),
      salt,
      signedAt: String(signedAtMillis),
    },
    null,
    2,
  );
  return { json, bytes: utf8(json), salt, signedAtMillis };
}

// ── authenticated REST ops (JWT via the relay) ──────────────────────────────

export type BluefinTokens = {
  accessToken: string;
  accessTokenValidForSeconds: number;
  refreshToken?: string;
};

/** `POST /auth/v2/token` — exchange a signed login payload for a JWT. The
 * signature is the base64 Sui serialized personal-message signature (wallet
 * `signPersonalMessage` output, or the FROST ceremony's serialized form). */
export async function bluefinLogin(
  payloadJson: string,
  signatureB64: string,
): Promise<BluefinTokens> {
  const res = await bluefinFetch("auth", "/auth/v2/token", {
    method: "POST",
    headers: {
      "content-type": "application/json",
      payloadSignature: signatureB64,
    },
    body: payloadJson,
  });
  return readJson<BluefinTokens>(res, "login");
}

const bearer = (jwt: string) => ({ authorization: `Bearer ${jwt}` });

export type BluefinOpenOrder = {
  orderHash: string;
  clientOrderId?: string;
  symbol: string;
  side: "LONG" | "SHORT";
  type?: string;
  priceE9: string;
  quantityE9: string;
  filledQuantityE9?: string;
  status?: string;
  reduceOnly?: boolean;
  createdAtMillis?: number;
};

export async function fetchOpenOrders(
  jwt: string,
  symbol?: string,
): Promise<BluefinOpenOrder[]> {
  const q = symbol ? `?symbol=${encodeURIComponent(symbol)}` : "";
  const res = await bluefinFetch("trade", `/api/v1/trade/openOrders${q}`, {
    headers: bearer(jwt),
  });
  return readJson<BluefinOpenOrder[]>(res, "open orders");
}

export type BluefinTrade = {
  symbol: string;
  side?: string;
  positionSide?: string;
  priceE9: string;
  quantityE9: string;
  tradingFeeE9?: string;
  isMaker?: boolean;
  executedAtMillis?: number;
  orderHash?: string;
};

export async function fetchAccountTrades(
  jwt: string,
  limit = 25,
): Promise<BluefinTrade[]> {
  const res = await bluefinFetch("data", `/api/v1/account/trades?limit=${limit}`, {
    headers: bearer(jwt),
  });
  return readJson<BluefinTrade[]>(res, "account trades");
}

/** `POST /api/v1/trade/orders` — relay a curator-wallet-signed order. */
export async function placeOrder(
  jwt: string,
  t: OrderTicket,
  idsId: string,
  salt: string,
  signedAtMillis: number,
  signatureB64: string,
): Promise<{ orderHash?: string }> {
  const body = {
    signedFields: {
      symbol: t.symbol,
      accountAddress: t.accountAddress,
      priceE9: t.priceE9,
      quantityE9: t.quantityE9,
      side: t.side,
      leverageE9: t.leverageE9,
      isIsolated: t.isIsolated,
      salt,
      idsId,
      expiresAtMillis: t.expiresAtMillis,
      signedAtMillis,
    },
    signature: signatureB64,
    type: t.type,
    reduceOnly: t.reduceOnly,
    ...(t.postOnly != null ? { postOnly: t.postOnly } : {}),
    ...(t.timeInForce != null ? { timeInForce: t.timeInForce } : {}),
  };
  const res = await bluefinFetch("trade", "/api/v1/trade/orders", {
    method: "POST",
    headers: { "content-type": "application/json", ...bearer(jwt) },
    body: JSON.stringify(body),
  });
  return readJson<{ orderHash?: string }>(res, "place order");
}

/** `PUT /api/v1/trade/orders/cancel` — JWT-only per Bluefin's API (no
 * wallet signature needed to cancel; max 10 hashes per call). */
export async function cancelOrders(
  jwt: string,
  symbol: string,
  orderHashes: string[],
): Promise<void> {
  const res = await bluefinFetch("trade", "/api/v1/trade/orders/cancel", {
    method: "PUT",
    headers: { "content-type": "application/json", ...bearer(jwt) },
    body: JSON.stringify({ symbol, orderHashes }),
  });
  if (!res.ok) await readJson(res, "cancel orders");
}

/** `POST /api/v1/trade/withdraw` — relay a ceremony-signed withdraw. Funds
 * can only land at the parent address (no destination field exists). */
export async function submitWithdraw(
  jwt: string,
  p: {
    assetSymbol: string;
    accountAddress: string;
    amountE9: string;
    edsId: string;
    salt: string;
    signedAtMillis: number;
    signatureB64: string;
  },
): Promise<void> {
  const body = {
    signedFields: {
      assetSymbol: p.assetSymbol,
      accountAddress: p.accountAddress,
      amountE9: p.amountE9,
      salt: p.salt,
      edsId: p.edsId,
      signedAtMillis: p.signedAtMillis,
    },
    signature: p.signatureB64,
  };
  const res = await bluefinFetch("trade", "/api/v1/trade/withdraw", {
    method: "POST",
    headers: { "content-type": "application/json", ...bearer(jwt) },
    body: JSON.stringify(body),
  });
  if (!res.ok) await readJson(res, "withdraw");
}

/** `PUT /api/v1/trade/accounts/authorize` — relay a ceremony-signed
 * authorize of the curator's trading wallet. */
export async function submitAuthorize(
  jwt: string,
  p: {
    accountAddress: string;
    authorizedAccountAddress: string;
    idsId: string;
    salt: string;
    signedAtMillis: number;
    signatureB64: string;
  },
): Promise<void> {
  const body = {
    signedFields: {
      accountAddress: p.accountAddress,
      authorizedAccountAddress: p.authorizedAccountAddress,
      salt: p.salt,
      idsId: p.idsId,
      signedAtMillis: p.signedAtMillis,
    },
    signature: p.signatureB64,
  };
  const res = await bluefinFetch("trade", "/api/v1/trade/accounts/authorize", {
    method: "PUT",
    headers: { "content-type": "application/json", ...bearer(jwt) },
    body: JSON.stringify(body),
  });
  if (!res.ok) await readJson(res, "authorize account");
}

// ── display helpers ─────────────────────────────────────────────────────────

/** E9 fixed-point decimal string → display number. */
export function fromE9(v: string | null | undefined): number | null {
  if (v == null) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n / 1e9 : null;
}

/** Display number → E9 decimal string (rounded to integer E9). */
export function toE9(v: number): string {
  return String(Math.round(v * 1e9));
}
