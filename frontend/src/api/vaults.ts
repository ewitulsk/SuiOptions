// HTTP client for the api-service covered-call vault endpoints.
//
// Authoritative response shapes live next to the handler:
//   rust-backend/services/api-service/src/handlers/vaults.rs
// Keep these types in sync with `VaultDto` / `VaultRoundDto` /
// `VaultReceiptDto`.
//
// Numeric fields ship in two flavors, same convention as `client.ts`:
//   - **scaled** (`number | null`) — display-ready, divided by the relevant
//     token's decimals / PPS_SCALE. `null` when decimals are unknown.
//   - **raw** (`string`) — on-chain integer in atomic units, kept as a string
//     to preserve u64/u128 precision. Use these when building a tx.

const API_BASE_URL: string =
  (import.meta.env.VITE_API_BASE_URL as string | undefined) ?? "http://127.0.0.1:9003";

/** One covered-call vault. Mirrors `VaultDto`. */
export type Vault = {
  vault_id: string;
  underlying_symbol: string;
  underlying_decimals: number | null;
  /** Fully-qualified coin type — the `U` (Underlying) type arg for PTBs. */
  underlying_coin_type: string;
  settlement_symbol: string;
  settlement_decimals: number | null;
  /** The `S` (Settlement) type arg for PTBs. */
  settlement_coin_type: string;
  /** The `V` (VShare) type arg for PTBs, and the share `Coin<V>` type. */
  share_type: string;
  /** Current round (last finalized + 1; 0 = pre-genesis). */
  round: number;
  current_bucket: string | null;
  /** Underlying-per-share at the last finalize (PPS_SCALE-adjusted). */
  pps: number | null;
  pps_raw: string | null;
  /** shares × pps + queued deposits, in whole underlying units. */
  tvl: number | null;
  tvl_raw: string | null;
  total_shares_raw: string;
  pending_deposits_raw: string;
  /** Ribbon-formula APY over the last two finalized rounds; null until two
   *  finalized rounds exist. */
  apy: number | null;
  deposits_paused: boolean;
  /** Live round phase, derived server-side from indexed state: "active"
   *  (selling/holding) | "settling" (between rounds / past expiry). */
  phase: string;
  /** Active on-chain VaultConfig (consumer-facing subset), in display units.
   *  `null` until the config-carrying events are indexed for this vault. */
  mgmt_fee_pct: number | null;
  perf_fee_pct: number | null;
  min_strike_over_spot_pct: number | null;
  max_strike_over_spot_pct: number | null;
  round_ms: number | null;
  selling_window_ms: number | null;
};

export type VaultsResponse = { vaults: Vault[] };

export async function fetchVaults(): Promise<Vault[]> {
  const res = await fetch(`${API_BASE_URL}/vaults`);
  if (!res.ok) throw new Error(`GET /vaults failed: ${res.status} ${res.statusText}`);
  const body: VaultsResponse = await res.json();
  return body.vaults;
}

export async function fetchVault(vaultId: string): Promise<Vault> {
  const res = await fetch(`${API_BASE_URL}/vaults/${encodeURIComponent(vaultId)}`);
  if (!res.ok) throw new Error(`GET /vaults/:id failed: ${res.status} ${res.statusText}`);
  return (await res.json()) as Vault;
}

/** One finalized-or-in-progress round. Mirrors `VaultRoundDto`. */
export type VaultRound = {
  round: number;
  bucket_id: string | null;
  /** Strike in USD-cross whole units (null when decimals are unknown). */
  strike: number | null;
  strike_raw: string | null;
  strike_scale: number | null;
  expiry_ms: number | null;
  /** Underlying-per-share locked at finalize; null until the round finalizes. */
  pps: number | null;
  pps_raw: string | null;
  aum_raw: string | null;
  shares_raw: string | null;
  premium_collected_raw: string | null;
  mgmt_fee_raw: string | null;
  perf_fee_raw: string | null;
  finalized_at_ms: number | null;
};

export type VaultRoundsResponse = { vault_id: string; rounds: VaultRound[] };

export async function fetchVaultRounds(vaultId: string): Promise<VaultRound[]> {
  const res = await fetch(`${API_BASE_URL}/vaults/${encodeURIComponent(vaultId)}/rounds`);
  if (!res.ok) {
    throw new Error(`GET /vaults/:id/rounds failed: ${res.status} ${res.statusText}`);
  }
  const body: VaultRoundsResponse = await res.json();
  return body.rounds;
}

/**
 * A user's aggregated deposit/withdraw receipt with derived claimability.
 * Mirrors `VaultReceiptDto`. Note: this aggregate carries no receipt
 * **object id** — to build a claim/withdraw PTB the frontend reads the
 * actual `DepositReceipt` / `WithdrawReceipt` objects from the wallet (see
 * `useOwnedVaultReceipts`).
 */
export type VaultReceipt = {
  owner: string;
  round: number;
  /** deposit | withdraw. */
  kind: string;
  /** Queued underlying (deposits) / escrowed shares (withdrawals), raw. */
  amount_raw: string;
  settled_raw: string;
  /** pending | claimable | settled. */
  status: string;
};

export type VaultReceiptsResponse = { vault_id: string; receipts: VaultReceipt[] };

export async function fetchVaultReceipts(
  vaultId: string,
  owner: string,
): Promise<VaultReceipt[]> {
  const res = await fetch(
    `${API_BASE_URL}/vaults/${encodeURIComponent(vaultId)}/receipts?owner=${encodeURIComponent(owner)}`,
  );
  if (!res.ok) {
    throw new Error(`GET /vaults/:id/receipts failed: ${res.status} ${res.statusText}`);
  }
  const body: VaultReceiptsResponse = await res.json();
  return body.receipts;
}

/**
 * One point on the vault's APY-over-time curve. `kind`/`confidence` are
 * present on predicted points only (realized points omit them).
 */
export type VaultApyPoint = {
  t_ms: number;
  apy: number;
  /** "current" (Tier 1) | "forecast" (Tier 2) — predicted points only. */
  kind?: string;
  confidence?: number;
};

/**
 * `GET /vaults/:id/apy` — realized (annualized pps growth per finalized round,
 * from the indexer) plus predicted (forward premium-yield, from
 * derived-metric-worker). `predicted` is empty when the worker isn't deployed.
 * Mirrors `api-service::handlers::vaults::VaultApyResponse`.
 */
export type VaultApySeries = {
  vault_id: string;
  realized: VaultApyPoint[];
  predicted: VaultApyPoint[];
};

export async function fetchVaultApy(vaultId: string): Promise<VaultApySeries> {
  const res = await fetch(`${API_BASE_URL}/vaults/${encodeURIComponent(vaultId)}/apy`);
  if (!res.ok) {
    throw new Error(`GET /vaults/:id/apy failed: ${res.status} ${res.statusText}`);
  }
  return (await res.json()) as VaultApySeries;
}
