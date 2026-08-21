// Client for dakota-service.
//
// Every call carries the session JWT; the service reads `role` and `scope`
// from it and scopes the answer server-side. Nothing here passes a customer
// id as a claim of authority — the token is the authority, and asking about a
// customer outside your scope returns 404.

import { DAKOTA_API } from "../config";

export type Asset = {
  id: number;
  symbol: string;
  network_id: string;
  onramp_enabled: boolean;
  offramp_enabled: boolean;
  swap_enabled: boolean;
  sort_order: number;
};

export type Catalog = { assets: Asset[]; networks: string[] };

export type Customer = {
  dakota_customer_id: string;
  customer_type: "business" | "individual";
  is_sub_client: boolean;
  sub_client_id: string | null;
  external_ref: string | null;
  application_id: string | null;
  kyb_status: string | null;
  kyc_status: string | null;
  application_status: string | null;
  created_at: string;
  updated_at: string;
};

export type Account = {
  dakota_account_id: string;
  dakota_customer_id: string;
  account_type: "onramp" | "offramp" | "swap";
  source_asset: string | null;
  source_network_id: string | null;
  destination_asset: string | null;
  destination_network_id: string | null;
  rail: string | null;
  created_at: string;
};

export type LedgerEvent = {
  event_id: string;
  event_type: string;
  resource_id: string | null;
  dakota_customer_id: string | null;
  direction: string | null;
  amount_minor: number | null;
  asset: string | null;
  exchange_rate: string | null;
  fee_minor: number | null;
  status: string | null;
  occurred_at: string | null;
};

export type CustomerFlow = {
  dakota_customer_id: string;
  customer_type: string;
  sub_client_id: string | null;
  asset: string | null;
  events: number;
  inbound_minor: number | null;
  outbound_minor: number | null;
};

export type AssetTotal = {
  asset: string;
  inbound_minor: number;
  outbound_minor: number;
  events: number;
};

export type Flows = { by_customer: CustomerFlow[]; totals: AssetTotal[] };

export type FeeSchedule = {
  id: number;
  source: string;
  transfer_fee_bps: number | null;
  ach_fee_cents: number | null;
  wire_fee_cents: number | null;
  sepa_fee_cents: number | null;
  swift_fee_cents: number | null;
  kyc_fee_cents: number | null;
  kyb_fee_cents: number | null;
  effective_from: string;
  note: string | null;
};

export type Rates = {
  schedule: FeeSchedule | null;
  realised: Array<{
    asset: string | null;
    exchange_rate: string | null;
    fee_minor: number | null;
    amount_minor: number | null;
    occurred_at: string | null;
  }>;
};

export type Invite = { invite_id: string; role: string; expires_at: string };

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly dakotaRequestId?: string,
    readonly fields?: Array<{ field?: string; message?: string }>,
  ) {
    super(message);
  }
}

async function call<T>(token: string, path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${DAKOTA_API}${path}`, {
    ...init,
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${token}`,
      ...(init?.headers ?? {}),
    },
  });
  const text = await res.text();
  if (!res.ok) {
    // dakota-service relays Dakota's RFC 9457 detail verbatim, because those
    // messages are specific and actionable ("capabilities are required",
    // "amount 5 exceeds sandbox cap of 2").
    try {
      const body = JSON.parse(text) as {
        error?: string;
        dakota_request_id?: string;
        fields?: Array<{ field?: string; message?: string }>;
      };
      throw new ApiError(
        body.error ?? text ?? `request failed (${res.status})`,
        res.status,
        body.dakota_request_id,
        body.fields,
      );
    } catch (e) {
      if (e instanceof ApiError) throw e;
      throw new ApiError(text || `request failed (${res.status})`, res.status);
    }
  }
  return text ? (JSON.parse(text) as T) : (undefined as T);
}

export const getCatalog = (t: string) => call<Catalog>(t, "/catalog");
export const getRates = (t: string) => call<Rates>(t, "/rates");
export const listCustomers = (t: string) => call<Customer[]>(t, "/customers");
export const listAccounts = (t: string) => call<Account[]>(t, "/accounts");
export const getFlows = (t: string) => call<Flows>(t, "/flows");
export const getFeed = (t: string, limit = 100) =>
  call<LedgerEvent[]>(t, `/flows/feed?limit=${limit}`);
export const getCustomerFeed = (t: string, id: string) =>
  call<LedgerEvent[]>(t, `/flows/${id}`);

/** Dakota's live record, including the name we never store ourselves. */
export const getCustomer = (t: string, id: string) =>
  call<Record<string, unknown>>(t, `/customers/${id}`);

export const getCapabilities = (t: string, id: string) =>
  call<Record<string, unknown>>(t, `/customers/${id}/capabilities`);

export type CreateCustomerBody = {
  name: string;
  customer_type: "business" | "individual";
  external_ref?: string;
  is_sub_client?: boolean;
  sub_client_id?: string;
  with_invite?: boolean;
};

export type CreateCustomerResult = {
  customer: Customer;
  application_url: string;
  invite?: Invite;
};

export const createCustomer = (t: string, body: CreateCustomerBody) =>
  call<CreateCustomerResult>(t, "/customers", {
    method: "POST",
    body: JSON.stringify(body),
  });

export const createInvite = (t: string, customerId: string) =>
  call<Invite>(t, `/customers/${customerId}/invite`, { method: "POST" });

export const createRecipient = (
  t: string,
  customerId: string,
  body: { name: string; address?: unknown },
) =>
  call<{ id: string }>(t, `/customers/${customerId}/recipients`, {
    method: "POST",
    body: JSON.stringify(body),
  });

export const createDestination = (
  t: string,
  recipientId: string,
  body: Record<string, unknown>,
) =>
  call<{ id: string }>(t, `/recipients/${recipientId}/destinations`, {
    method: "POST",
    body: JSON.stringify(body),
  });

export type CreateAccountBody = {
  customer_id: string;
  account_type: "onramp" | "offramp" | "swap";
  crypto_destination_id?: string;
  fiat_destination_id?: string;
  source_asset?: string;
  destination_asset?: string;
  source_network_id?: string;
  destination_network_id?: string;
};

/** Returns Dakota's raw account body — deposit details live in there. */
export const createAccount = (t: string, body: CreateAccountBody) =>
  call<Record<string, any>>(t, "/accounts", {
    method: "POST",
    body: JSON.stringify(body),
  });

export const getAccount = (t: string, id: string) =>
  call<Record<string, any>>(t, `/accounts/${id}`);

// --- admin -------------------------------------------------------------------

export const upsertAsset = (t: string, a: Omit<Asset, "id">) =>
  call<Asset>(t, "/admin/assets", { method: "PUT", body: JSON.stringify(a) });

export const deleteAsset = (t: string, id: number) =>
  call<void>(t, `/admin/assets/${id}`, { method: "DELETE" });

export const setRates = (t: string, body: Partial<FeeSchedule> & { note?: string }) =>
  call<FeeSchedule>(t, "/admin/rates", { method: "POST", body: JSON.stringify(body) });

export const listSubClients = (t: string) =>
  call<{ sub_clients: Customer[]; summary: any }>(t, "/admin/sub-clients");

export const simulateOnboarding = (t: string, customerId: string, type?: string) =>
  call<{ previous_state?: string; new_state?: string }>(t, "/admin/sandbox/onboarding", {
    method: "POST",
    body: JSON.stringify({ customer_id: customerId, type }),
  });

export type SimulateInboundBody = {
  type: string;
  amount: string;
  currency?: string;
  account_id?: string;
  wallet_address?: string;
};

export const simulateInbound = (t: string, body: SimulateInboundBody) =>
  call<Record<string, unknown>>(t, "/admin/sandbox/inbound", {
    method: "POST",
    body: JSON.stringify(body),
  });

export const resync = (t: string) =>
  call<{ scanned: number; inserted: number }>(t, "/admin/resync", { method: "POST" });

export const registerWebhook = (t: string) =>
  call<{ url: string }>(t, "/admin/webhooks/register", { method: "POST" });

export const listWebhooks = (t: string) => call<any>(t, "/admin/webhooks");

export const getTreasury = (t: string) => call<{ treasury: any[] }>(t, "/admin/treasury");

export const setupTreasury = (t: string, label = "treasury", family = "evm") =>
  call<any>(t, "/admin/treasury/setup", {
    method: "POST",
    body: JSON.stringify({ label, family }),
  });

export const treasurySend = (
  t: string,
  walletId: string,
  body: { to: string; amount: string; asset_id: string; network_id: string },
) =>
  call<Record<string, unknown>>(t, `/admin/treasury/${walletId}/send`, {
    method: "POST",
    body: JSON.stringify(body),
  });

// --- formatting --------------------------------------------------------------

/** Minor units (cents) → a display string. Amounts are integers end to end. */
export function formatMinor(minor: number | null | undefined): string {
  if (minor == null) return "—";
  const sign = minor < 0 ? "-" : "";
  const abs = Math.abs(minor);
  return `${sign}${Math.floor(abs / 100)}.${String(abs % 100).padStart(2, "0")}`;
}
