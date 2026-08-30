// Single source of truth for which on-chain deployment the app targets.
//
// The package / protocol-config / treasury ids and the supported-token
// catalog are fetched at startup from the **token-info service** (the only
// service that reads deployments.json). `initConfig()` must be awaited before
// the app renders — see `main.tsx`. This is a hard cutover: if token-info is
// unreachable the app fails to boot (there is no deployments.json fallback).
//
// Ids are `string | undefined`: an environment with no published deployment
// yields `undefined`, and the screens fall back to their existing "no
// deployment configured" / empty states instead of crashing.

export type SuiEnvironment = "mainnet" | "testnet" | "devnet";

// Default to testnet so local dev runs with no env file. Selects the Sui
// network for dapp-kit; the on-chain ids come from token-info.
export const ENV: SuiEnvironment =
  (import.meta.env.VITE_ENVIRONMENT as SuiEnvironment | undefined) ?? "testnet";

// token-info public base URL. Local dev hits the service directly; deployed
// builds set VITE_TOKEN_INFO_URL to the env's public route
// (e.g. https://<host>/<env>/token-info).
export const TOKEN_INFO_URL: string =
  (import.meta.env.VITE_TOKEN_INFO_URL as string | undefined) ??
  "http://127.0.0.1:9005";

// auth-service public base URL. Local dev hits the service directly; deployed
// builds set VITE_AUTH_URL to the env's public route
// (e.g. https://<host>/<env>/auth). Used by the admin token-manager to obtain
// the JWT that gates token-info's mutate endpoints.
export const AUTH_URL: string =
  (import.meta.env.VITE_AUTH_URL as string | undefined) ?? "http://127.0.0.1:9007";

// gas-station public base URL. Sponsors user transactions (pays their gas).
// Local dev hits the service directly; deployed builds set VITE_GAS_STATION_URL
// to the env's public route (e.g. https://<host>/<env>/gas-station).
export const GAS_STATION_URL: string =
  (import.meta.env.VITE_GAS_STATION_URL as string | undefined) ?? "http://127.0.0.1:9009";

// price-charting public base URL (SO-157). Serves OHLC bars (REST) and live
// bar updates (WS at `<base>/ws`, http(s) scheme auto-swapped to ws(s)).
// Deployed builds set VITE_CHARTS_URL to the env's public route
// (e.g. https://<host>/<env>/charts).
export const CHARTS_URL: string =
  (import.meta.env.VITE_CHARTS_URL as string | undefined) ?? "http://127.0.0.1:9011";

// oracle-service public base URL (SO-335). Serves `GET /oracle/descriptor`
// — the live oracle provider plus its on-chain adapter ids — which
// `tx/appraisal.ts` reads instead of hardcoding an adapter package. That
// is what lets a provider switch (a backend config field) take effect in
// an ALREADY-DEPLOYED frontend, with no rebuild.
//
// Deployed builds set VITE_ORACLE_SERVICE_URL to the env's public route.
export const ORACLE_SERVICE_URL: string =
  (import.meta.env.VITE_ORACLE_SERVICE_URL as string | undefined) ?? "http://127.0.0.1:9013";

// cctp-relay public base URL. Tracks CCTP bridge transfers, auto-relays the
// destination-chain mint, and serves the CCTP constants (`GET /config`, see
// api/cctpConfig.ts) — those are NOT derived from ENV here, because the bridge
// runs on its own network independent of the protocol's. Deployed builds set
// VITE_CCTP_URL to the env's public route (e.g. https://<host>/<env>/cctp).
export const CCTP_URL: string =
  (import.meta.env.VITE_CCTP_URL as string | undefined) ?? "http://127.0.0.1:9015";

// orderbook public base URL (SO-416). REST + WS gateway of the in-house
// hybrid exchange (rust-backend/services/orderbook): markets, books, signed
// order intake, cancels, and taker route quotes. Deployed builds set
// VITE_ORDERBOOK_URL to the env's public route
// (e.g. https://<host>/<env>/orderbook).
export const ORDERBOOK_URL: string =
  (import.meta.env.VITE_ORDERBOOK_URL as string | undefined) ?? "http://127.0.0.1:9014";

// leaderboard public base URL (go-backend). Read-only points/ranking API
// feeding the Leaderboard tab. Deployed builds set VITE_LEADERBOARD_URL to
// the env's public route (e.g. https://<host>/<env>/leaderboard).
export const LEADERBOARD_URL: string =
  (import.meta.env.VITE_LEADERBOARD_URL as string | undefined) ?? "http://127.0.0.1:9021";

// event-ingestor admin base URL (go-backend). JWT-gated config plane for
// the event→points rules driving the leaderboard (see api/ingestorAdmin.ts).
// Deployed builds set VITE_INGESTOR_URL to the env's public route
// (e.g. https://<host>/<env>/ingestor).
export const INGESTOR_URL: string =
  (import.meta.env.VITE_INGESTOR_URL as string | undefined) ?? "http://127.0.0.1:9023";

// hedge-signer public base URL (SO-305). Drives the curator dashboard's
// FROST ceremonies (/frost/*) and the allowlisted Bluefin REST relay
// (/bluefin/*; Bluefin CORS-blocks third-party origins, so the browser
// never calls their API directly). Deployed builds set
// VITE_HEDGE_SIGNER_URL to the env's public route
// (e.g. https://<host>/<env>/hedge-signer).
export const HEDGE_SIGNER_URL: string =
  (import.meta.env.VITE_HEDGE_SIGNER_URL as string | undefined) ?? "http://127.0.0.1:9017";

// Populated by `initConfig()`. Exported as live bindings — consumers that
// `import { PACKAGE_ID }` see the value once initialization completes (which
// happens before the first render).
export let PACKAGE_ID: string | undefined;
export let PROTOCOL_CONFIG_ID: string | undefined;
export let TREASURY_ID: string | undefined;

// The options_vault package of the four-package contracts tree.
// `PACKAGE_ID` above is options_core (buckets/accounts/quotes); vault PTBs
// and vault object types resolve against this one. `undefined` on records
// predating the split.
export let VAULT_PACKAGE_ID: string | undefined;

// The trading_vault package (curated trading vaults, SO-288). Both `undefined`
// on records predating the trading-vault deployment — the /vaults screens
// render an "unavailable on this network" empty state. The publish digest is
// used client-side to resolve the shared `VaultProtocolConfig` object id
// (token-info doesn't serve it).
export let TRADING_VAULT_PACKAGE_ID: string | undefined;
export let TRADING_VAULT_PUBLISH_DIGEST: string | undefined;

// Trading-vault adapter packages (SO-289). All `undefined` on deployments
// predating the adapter publishes — the appraisal composer surfaces a clear
// error instead of building an unresolvable PTB.
export let ORACLE_PYTH_PACKAGE_ID: string | undefined;
export let DEEPBOOK_ADAPTER_PACKAGE_ID: string | undefined;
export let OPTIONS_ADAPTER_PACKAGE_ID: string | undefined;
/** Hybrid-exchange maker adapter for the trading vault (SO-370). */
export let EXCHANGE_ADAPTER_PACKAGE_ID: string | undefined;

/** Hybrid-exchange settlement package. `undefined` where no exchange is
 * deployed. Per-market pause admin PTBs target this package with its own
 * `exchange::admin::AdminCap` (separate from the core cap). */
export let EXCHANGE_PACKAGE_ID: string | undefined;

/** The ONE shared ingress `Whitelist` of the standalone whitelist package
 * (guarded launch): the gate arg every ingress entry across core /
 * trading-vault / exchange takes. `undefined` on records predating the
 * standalone package. */
export let WHITELIST_ID: string | undefined;

/** Shared `BucketRegistry` for any-strike bucket creation (SO-395).
 * `undefined` on deployments predating the overhaul — the custom-strike UI
 * then stays hidden. */
export let BUCKET_REGISTRY_ID: string | undefined;

/** Standalone whitelist package id + its AdminCap (admin PTBs). */
export let WHITELIST_PACKAGE_ID: string | undefined;
export let WHITELIST_ADMIN_CAP_ID: string | undefined;

/** One created exchange market: the shared `SettlementRegistry` id plus the
 * `Base`/`Quote` coin types its `registry::set_paused<Base, Quote>` call
 * needs (the big-red-button pauses every market). */
export type ExchangeMarket = {
  registryId: string;
  base: string;
  quote: string;
};
export let EXCHANGE_MARKETS: ExchangeMarket[] = [];

/** Permissionless option-market listing package (SO-415/416): anyone can
 * list an exchange market for an existing bucket via
 * `exchange_listing::create_call_market` / `create_put_market`. Both ids
 * `undefined` when token-info doesn't serve the block yet (or the env has
 * no listing deployment) — the "List market" UI then stays hidden.
 * VITE_ overrides let a build point at a listing deploy token-info doesn't
 * know about yet. */
export let EXCHANGE_LISTING_PACKAGE_ID: string | undefined;
export let EXCHANGE_LISTING_AUTHORITY_ID: string | undefined;

// Keeper-attested equity oracle for trading-vault external accounts
// (SO-299). The publish digest resolves the shared `EquityBook` client-side
// (token-info doesn't serve it), mirroring the publish-digest fallback below.
export let EQUITY_ORACLE_PACKAGE_ID: string | undefined;
export let EQUITY_ORACLE_PUBLISH_DIGEST: string | undefined;

/** The Pyth state's `price_info` `Table<PriceIdentifier, ID>` id (feed id →
 * `PriceInfoObject`), pinned so feed resolution can derive per-feed
 * dynamic-field ids directly. Never re-created for a deployment. Not served
 * by token-info (it isn't our deployment); both staging and prod run on Sui
 * testnet, so only the testnet slot is populated. */
export const PYTH_PRICE_INFO_TABLE_IDS: Partial<Record<SuiEnvironment, string>> = {
  testnet: "0xcb858b77d8068c6c8c0d8a4ddfba95053268e4a31f8ecc49adccc4ec1570d3a7",
};

/** Shared governance objects created by the trading-vault family's inits,
 * recorded at deploy time (SO-292). Absent on older deployments — consumers
 * fall back to publish-digest discovery where they can. */
export type TradingVaultObjects = {
  vaultProtocolConfigId: string;
  integrationRegistryId: string;
  oracleRegistryId: string;
  pythFeedRegistryId: string;
  poolAllowlistId: string;
  /** Options-adapter VolBook (premium marks) — absent on older deploys. */
  volBookId?: string | null;
  /** §9.2 terms binding (SO-418): the spec version + content hash new
   * vaults are created under — absent on records predating v2. */
  termsVersion?: number | null;
  /** Hex spec hash (with 0x). */
  specHash?: string | null;
};
export let TRADING_VAULT_OBJECTS: TradingVaultObjects | undefined;

// Canonical DEEP coin type of the DeepBook deployment (SO-151), served by
// token-info. Options secondary trading moved to the in-house exchange
// (SO-416), but trading-vault custody appraisals still value locked-balance
// legs in DEEP, so this one id survives the cutover. `undefined` on networks
// without a DeepBook deployment (devnet).
export let DEEP_COIN_TYPE: string | undefined;

// Testnet faucet tokens (SO-93). Each is a shared `Faucet` with a public
// `mint_to_sender`. Only the testnet/dev deployment publishes these; on
// mainnet this stays `[]` and the faucet page shows a "testnet only" state.
export type TestToken = {
  symbol: string;
  /** Full Move coin type, e.g. `0x…::tbtc::TBTC`. */
  coinType: string;
  /** Shared `Faucet` object id. */
  faucetId: string;
  decimals: number;
  /** Module name from the coin type's middle segment, e.g. `tbtc`. */
  module: string;
  /** Package that published the test tokens (≠ the protocol `PACKAGE_ID`). */
  packageId: string;
};

export let TEST_TOKENS: TestToken[] = [];

/** One supported-token catalog entry from token-info's `GET /tokens`. */
export type SupportedToken = {
  coinType: string;
  ticker: string;
  name: string;
  logoUri: string | null;
  decimals: number;
  pythFeedId: string | null;
  enabled: boolean;
};

export let SUPPORTED_TOKENS: SupportedToken[] = [];

/**
 * Resolve a supported-token catalog entry from a market symbol. Call sites pass
 * either the on-chain ticker (`TBTC`) or the stripped display alias (`BTC`, the
 * form `displayAsset()` produces), so match the ticker directly first, then the
 * ticker with a leading test-token `T` removed. Returns `null` for symbols that
 * aren't catalog tokens (e.g. native `SUI`).
 */
export function findToken(symbol: string | null | undefined): SupportedToken | null {
  if (!symbol) return null;
  const upper = symbol.trim().toUpperCase();
  if (!upper) return null;
  return (
    SUPPORTED_TOKENS.find((t) => t.ticker.toUpperCase() === upper) ??
    SUPPORTED_TOKENS.find((t) => t.ticker.toUpperCase().replace(/^T/, "") === upper) ??
    null
  );
}

// --- multichain spoke vault (docs/multichain-vault-plan.md §9) ---------------
//
// Per-network EVM spoke bundle, selected by the same `VITE_ENVIRONMENT`
// switch as everything else: the testnet-set pairs the SpokeVault with the
// faucet-mintable TUSDG; the mainnet-set uses real USDG. Promotion is a
// config flip, never a code edit.

export type SpokeConfig = {
  /** EVM chain id of the spoke chain. */
  chainId: number;
  chainName: string;
  rpcUrl: string;
  explorerUrl: string;
  /** `SpokeVault` contract address. */
  spokeVaultAddress: `0x${string}`;
  /** Deposit-asset ERC-20: TUSDG on the testnet-set, real USDG on mainnet. */
  usdgAddress: `0x${string}`;
  usdgDecimals: number;
  /** Spoke-local asset code the vault registers USDG under. */
  assetCode: number;
};

/** Placeholder until the spoke deploy publishes real addresses. */
export const SPOKE_ZERO_ADDRESS =
  "0x0000000000000000000000000000000000000000" as const;

const SPOKE_CONFIGS: Partial<Record<SuiEnvironment, SpokeConfig>> = {
  // testnet-set. Chain values are the working assumption for the first spoke
  // (an Arbitrum-stack testnet); the deploy config owns the final choice.
  testnet: {
    chainId: 421614,
    chainName: "Arbitrum Sepolia",
    rpcUrl: "https://sepolia-rollup.arbitrum.io/rpc",
    explorerUrl: "https://sepolia.arbiscan.io",
    // 0x0 placeholders: filled by the spoke deployment (SpokeVault + TUSDG).
    spokeVaultAddress: SPOKE_ZERO_ADDRESS,
    usdgAddress: SPOKE_ZERO_ADDRESS,
    usdgDecimals: 6,
    assetCode: 1,
  },
  // mainnet-set (real USDG; no TUSDG faucet).
  mainnet: {
    chainId: 42161,
    chainName: "Arbitrum One",
    rpcUrl: "https://arb1.arbitrum.io/rpc",
    explorerUrl: "https://arbiscan.io",
    // 0x0 placeholders: filled by the mainnet spoke deployment.
    spokeVaultAddress: SPOKE_ZERO_ADDRESS,
    usdgAddress: SPOKE_ZERO_ADDRESS,
    usdgDecimals: 6,
    assetCode: 1,
  },
  // devnet: no spoke deployment — /spoke renders its unavailable state.
};

const spokeBase = SPOKE_CONFIGS[ENV];

/** Chain METADATA (rpc/explorer) by EVM chain id — the only spoke values
 * that are runtime config rather than deployment facts. Addresses come
 * from token-info's `multichain` block (one place to write:
 * deployments.json); this map only says how to reach a chain the record
 * names. Unknown chain ids fall back to the env's `SPOKE_CONFIGS` entry. */
const SPOKE_CHAIN_METADATA: Record<
  number,
  { chainName: string; rpcUrl: string; explorerUrl: string }
> = {
  421614: {
    chainName: "Arbitrum Sepolia",
    rpcUrl: "https://sepolia-rollup.arbitrum.io/rpc",
    explorerUrl: "https://sepolia.arbiscan.io",
  },
  42161: {
    chainName: "Arbitrum One",
    rpcUrl: "https://arb1.arbitrum.io/rpc",
    explorerUrl: "https://arbiscan.io",
  },
};

function applySpokeOverrides(base: SpokeConfig): SpokeConfig {
  return {
    ...base,
    spokeVaultAddress:
      (import.meta.env.VITE_SPOKE_VAULT_ADDRESS as `0x${string}` | undefined) ??
      base.spokeVaultAddress,
    usdgAddress:
      (import.meta.env.VITE_SPOKE_USDG_ADDRESS as `0x${string}` | undefined) ??
      base.usdgAddress,
  };
}

/** The active spoke bundle, or `undefined` when this network has none.
 * Boot value carries the static map's placeholders; `initConfig()`
 * replaces it with the deployment record token-info serves. The
 * `VITE_SPOKE_*` overrides stay break-glass and win over both. */
export let SPOKE_CONFIG: SpokeConfig | undefined = spokeBase
  ? applySpokeOverrides(spokeBase)
  : undefined;

/** False while the config still carries 0x0 placeholders — the screen then
 * shows an "awaiting deployment" state instead of firing doomed reads. */
export function spokeDeployed(cfg: SpokeConfig): boolean {
  return (
    cfg.spokeVaultAddress !== SPOKE_ZERO_ADDRESS &&
    cfg.usdgAddress !== SPOKE_ZERO_ADDRESS
  );
}

// --- wire shapes returned by token-info -------------------------------------

type PackageInfoDto = {
  packageId: string;
  protocolConfigId: string;
  treasuryId?: string | null;
  network?: string;
  testTokens?: {
    packageId: string;
    tokens: Record<string, { coinType: string; faucetId: string; decimals: number }>;
  } | null;
  deepbook?: {
    packageId: string;
    originalPackageId: string;
    registryId: string;
    deepCoinType: string;
    poolCreationFee: string;
  } | null;
  vault?: { packageId: string } | null;
  tradingVault?: { packageId: string; publishDigest: string } | null;
  oraclePyth?: { packageId: string } | null;
  deepbookAdapter?: { packageId: string } | null;
  optionsAdapter?: { packageId: string } | null;
  exchangeAdapter?: { packageId: string } | null;
  exchange?: {
    packageId: string;
    /** Created markets keyed by symbol (e.g. `TBTC/TUSDC`). */
    markets?: Record<string, { registryId: string; base: string; quote: string }>;
  } | null;
  /** Permissionless option-market listing (SO-415/416). Absent until the
   * backend serves the block — code defensively. */
  exchangeListing?: { packageId: string; listingAuthorityId: string } | null;
  /** Standalone ingress whitelist package (guarded launch). Absent on
   * records predating the standalone package. */
  whitelist?: {
    packageId: string;
    whitelistId: string;
    adminCapId: string;
  } | null;
  /** Shared `bucket_registry::BucketRegistry` (any-strike derived bucket
   * UIDs, SO-393). Absent on records predating the overhaul. */
  bucketRegistryId?: string | null;
  equityOracle?: { packageId: string; publishDigest: string } | null;
  /** Multichain hub wiring + EVM spoke deployments (multichain-vault-plan
   * §9). Absent until the multichain deploy pass has run for this env. */
  multichain?: {
    endpointRegistryId: string;
    hubChainId: number;
    spokes?: Record<
      string,
      {
        spokeId: number;
        protocolChainId: number;
        evmChainId: number;
        spokeVault: string;
        relayerEndpoint?: string | null;
        layerzeroEndpoint?: string | null;
        ccipEndpoint?: string | null;
        usdg: { address: string; decimals: number; assetCode: number };
        deployBlock: number;
      }
    > | null;
  } | null;
  tradingVaultObjects?: {
    vaultProtocolConfigId: string;
    integrationRegistryId: string;
    oracleRegistryId: string;
    pythFeedRegistryId: string;
    poolAllowlistId: string;
    volBookId?: string | null;
  } | null;
};

type SupportedTokenDto = {
  coin_type: string;
  ticker: string;
  name: string;
  logo_uri?: string | null;
  decimals: number;
  pyth_feed_id?: string | null;
  enabled: boolean;
};

/**
 * Fetch `package_info` + the supported-token catalog from token-info and
 * populate the exported bindings. Throws if token-info is unreachable or
 * returns non-2xx — the caller (main.tsx) surfaces a boot error.
 */
export async function initConfig(): Promise<void> {
  const base = TOKEN_INFO_URL.replace(/\/$/, "");
  const [piRes, tokRes] = await Promise.all([
    fetch(`${base}/package-info`),
    fetch(`${base}/tokens`),
  ]);
  if (!piRes.ok) throw new Error(`token-info /package-info → ${piRes.status}`);
  if (!tokRes.ok) throw new Error(`token-info /tokens → ${tokRes.status}`);

  const info = (await piRes.json()) as PackageInfoDto;
  const tokens = (await tokRes.json()) as SupportedTokenDto[];

  PACKAGE_ID = info.packageId;
  PROTOCOL_CONFIG_ID = info.protocolConfigId;
  TREASURY_ID = info.treasuryId ?? undefined;
  VAULT_PACKAGE_ID = info.vault?.packageId;
  TRADING_VAULT_PACKAGE_ID = info.tradingVault?.packageId;
  TRADING_VAULT_PUBLISH_DIGEST = info.tradingVault?.publishDigest;
  ORACLE_PYTH_PACKAGE_ID = info.oraclePyth?.packageId;
  DEEPBOOK_ADAPTER_PACKAGE_ID = info.deepbookAdapter?.packageId;
  OPTIONS_ADAPTER_PACKAGE_ID = info.optionsAdapter?.packageId;
  EXCHANGE_ADAPTER_PACKAGE_ID = info.exchangeAdapter?.packageId;
  EXCHANGE_PACKAGE_ID = info.exchange?.packageId;
  EXCHANGE_MARKETS = Object.values(info.exchange?.markets ?? {}).map((m) => ({
    registryId: m.registryId,
    base: m.base,
    quote: m.quote,
  }));
  EXCHANGE_LISTING_PACKAGE_ID =
    (import.meta.env.VITE_EXCHANGE_LISTING_PACKAGE_ID as string | undefined) ??
    info.exchangeListing?.packageId;
  EXCHANGE_LISTING_AUTHORITY_ID =
    (import.meta.env.VITE_EXCHANGE_LISTING_AUTHORITY_ID as string | undefined) ??
    info.exchangeListing?.listingAuthorityId;
  WHITELIST_ID = info.whitelist?.whitelistId ?? undefined;
  BUCKET_REGISTRY_ID = info.bucketRegistryId ?? undefined;
  WHITELIST_PACKAGE_ID = info.whitelist?.packageId;
  WHITELIST_ADMIN_CAP_ID = info.whitelist?.adminCapId;
  EQUITY_ORACLE_PACKAGE_ID = info.equityOracle?.packageId;
  EQUITY_ORACLE_PUBLISH_DIGEST = info.equityOracle?.publishDigest;
  TRADING_VAULT_OBJECTS = info.tradingVaultObjects ?? undefined;

  // Spoke addresses come from the SAME deployment record as everything
  // else (multichain-vault-plan §9: one place to write). The first spoke
  // in the record is the active one (MVP: exactly one); chain metadata
  // (rpc/explorer) resolves via SPOKE_CHAIN_METADATA with the static
  // env entry as fallback for unknown chain ids.
  const spokeEntry = Object.values(info.multichain?.spokes ?? {})[0];
  if (spokeEntry) {
    const meta =
      SPOKE_CHAIN_METADATA[spokeEntry.evmChainId] ??
      (spokeBase
        ? {
            chainName: spokeBase.chainName,
            rpcUrl: spokeBase.rpcUrl,
            explorerUrl: spokeBase.explorerUrl,
          }
        : undefined);
    if (meta) {
      SPOKE_CONFIG = applySpokeOverrides({
        chainId: spokeEntry.evmChainId,
        ...meta,
        spokeVaultAddress: spokeEntry.spokeVault as `0x${string}`,
        usdgAddress: spokeEntry.usdg.address as `0x${string}`,
        usdgDecimals: spokeEntry.usdg.decimals,
        assetCode: spokeEntry.usdg.assetCode,
      });
    } else {
      console.warn(
        `token-info names spoke chain ${spokeEntry.evmChainId} but no rpc metadata is configured — keeping static spoke config`,
      );
    }
  }

  DEEP_COIN_TYPE = info.deepbook?.deepCoinType;

  const tt = info.testTokens;
  TEST_TOKENS = tt
    ? Object.entries(tt.tokens).map(([symbol, t]) => ({
        symbol,
        coinType: t.coinType,
        faucetId: t.faucetId,
        decimals: t.decimals,
        module: t.coinType.split("::")[1] ?? symbol.toLowerCase(),
        // Derive the package from the token's own coinType, not the shared
        // testTokens.packageId — a token (e.g. prod TSUI) can live in a
        // different package than the rest, and the faucet must target the
        // package that actually contains its module.
        packageId: t.coinType.split("::")[0] ?? tt.packageId,
      }))
    : [];

  SUPPORTED_TOKENS = tokens.map((t) => ({
    coinType: t.coin_type,
    ticker: t.ticker,
    name: t.name,
    logoUri: t.logo_uri ?? null,
    decimals: t.decimals,
    pythFeedId: t.pyth_feed_id ?? null,
    enabled: t.enabled,
  }));
}
