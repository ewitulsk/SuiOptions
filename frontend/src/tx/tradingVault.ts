// Programmable Transaction Block builders for the curated trading vault's
// wallet-facing flows (SO-288, v2 overhaul SO-418).
//
// Shapes mirror the Move signatures in
// `contracts/trading-vault-v2/sources/vault.move` + `vault_position.move`.
// v2 deltas: `deposit<T>` takes a `tranche_code` and RETURNS a transferable
// `VaultPosition` NFT the PTB must place (`transferObjects` to the sender);
// `request_withdraw<P>` CONSUMES a whole position object (partial exit =
// split first); the settlement pool replaces `enqueue_closed_stake`; and
// `create_vault` carries the immutable capital-structure terms + the Clock.

import { bcs } from "@mysten/sui/bcs";
import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import { fromHex } from "@mysten/sui/utils";

import {
  DEEPBOOK_ADAPTER_PACKAGE_ID,
  ENV,
  EQUITY_ORACLE_PACKAGE_ID,
  TRADING_VAULT_OBJECTS,
  TRADING_VAULT_PACKAGE_ID,
  TREASURY_ID,
  WHITELIST_ID,
} from "../config";

const CLOCK_ID = "0x6";

function requirePackage(): string {
  if (!TRADING_VAULT_PACKAGE_ID) {
    throw new Error(
      `No trading-vault deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no tradingVault packageId) — cannot build trading-vault PTBs`,
    );
  }
  return TRADING_VAULT_PACKAGE_ID;
}

/** The shared `whitelist::Whitelist` — `create_vault` / `deposit`'s ingress
 * gate (SO-383). */
function requireWhitelist(): string {
  if (!WHITELIST_ID) {
    throw new Error(
      `No whitelistId for VITE_ENVIRONMENT="${ENV}" — cannot build trading-vault PTBs`,
    );
  }
  return WHITELIST_ID;
}

/** The shared core `Treasury` — settlement redemptions deposit the protocol
 * fee cut into it. */
function requireTreasury(): string {
  if (!TREASURY_ID) {
    throw new Error(
      `No treasuryId for VITE_ENVIRONMENT="${ENV}" — cannot build settlement PTBs`,
    );
  }
  return TREASURY_ID;
}

// ═══════════════════════════════ creation ═══════════════════════════════

/** Immutable capital-structure terms for `create_vault` (SO-418). An
 * untranched vault passes structure code 0 and all six tranche params 0. */
export type CreateVaultCapitalParams = {
  /** 0 = Untranched, 1 = SeniorJunior. */
  structureCode: number;
  seniorHurdleBpsAnnual: number;
  targetJuniorBps: number;
  maintenanceJuniorBps: number;
  /** 0 = PreferredOnly, 1 = CappedParticipating, 2 = UncappedParticipating. */
  upsideCode: number;
  residualParticipationBps: number;
  totalReturnCapBps: number;
};

export const UNTRANCHED_CAPITAL: CreateVaultCapitalParams = {
  structureCode: 0,
  seniorHurdleBpsAnnual: 0,
  targetJuniorBps: 0,
  maintenanceJuniorBps: 0,
  upsideCode: 0,
  residualParticipationBps: 0,
  totalReturnCapBps: 0,
};

export type CreateTradingVaultParams = {
  /** Shared `VaultProtocolConfig` object id (resolved from the publish tx). */
  protocolConfigId: string;
  /** The vault's accounting-asset coin type — the `T` type arg. */
  depositCoinType: string;
  lockupMs: number;
  curatorFeeBps: number;
  unwindGraceMs: number;
  capital: CreateVaultCapitalParams;
  /** §9.2 terms binding: the spec version + content hash governing issuance. */
  termsVersion: number;
  /** Hex spec hash (with or without 0x); empty = no hash recorded. */
  specHashHex: string;
};

/**
 * `vault::create_vault<T>` — permissionless vault creation. The creator is
 * the initial curator: the `CuratorCap` is transferred to the sender inside
 * the call; the vault is shared. The capital structure is immutable at
 * creation (SO-418) and validated on-chain against the protocol floors/caps.
 */
export function buildCreateTradingVaultTx(p: CreateTradingVaultParams): Transaction {
  const pkg = requirePackage();
  const c = p.capital;
  const specHash = p.specHashHex.replace(/^0x/, "");
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::create_vault`,
    typeArguments: [p.depositCoinType],
    arguments: [
      tx.object(p.protocolConfigId),
      tx.object(requireWhitelist()),
      tx.pure.u64(p.lockupMs),
      tx.pure.u64(p.curatorFeeBps),
      tx.pure.u64(p.unwindGraceMs),
      tx.pure.u8(c.structureCode),
      tx.pure.u64(c.seniorHurdleBpsAnnual),
      tx.pure.u64(c.targetJuniorBps),
      tx.pure.u64(c.maintenanceJuniorBps),
      tx.pure.u8(c.upsideCode),
      tx.pure.u64(c.residualParticipationBps),
      tx.pure.u64(c.totalReturnCapBps),
      tx.pure.u64(p.termsVersion),
      tx.pure(bcs.vector(bcs.u8()).serialize(specHash ? fromHex(specHash) : new Uint8Array())),
      tx.object(CLOCK_ID),
    ],
  });
  return tx;
}

// ═══════════════════════════════ deposit ═══════════════════════════════

export type TradingVaultDepositParams = {
  vaultId: string;
  /** Shared `VaultProtocolConfig` object id. */
  protocolConfigId: string;
  /** The vault's ACCOUNTING asset — this plain builder passes `none` for the
   * attestation, which only the accounting asset may do. */
  depositCoinType: string;
  /** Deposit amount in smallest units. */
  amountRaw: bigint;
  /** 0 untranched / 1 senior / 2 junior. */
  trancheCode: number;
  /** The sender — the minted `VaultPosition` NFT is transferred here. */
  sender: string;
};

/**
 * `vault::begin_appraisal<T>` piped into `vault::deposit<T>` — mint a
 * `VaultPosition` NFT at NAV and transfer it to the sender (v2: `deposit`
 * RETURNS the position; the PTB decides where it goes). Accounting-asset
 * deposits only (`att` is `none`). This PTB only succeeds while the vault
 * holds nothing but its accounting asset (no positions, no other
 * balances); otherwise the appraisal is incomplete and `deposit` aborts.
 * Non-accounting deposits go through `buildAppraisedDepositTx`
 * (tx/appraisal.ts), which composes the attestation the option must carry.
 */
export function buildTradingVaultDepositTx(p: TradingVaultDepositParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const appraisal = tx.moveCall({
    target: `${pkg}::vault::begin_appraisal`,
    typeArguments: [p.depositCoinType],
    arguments: [tx.object(p.vaultId)],
  });
  const funds = tx.add(
    coinWithBalance({ balance: p.amountRaw, type: p.depositCoinType }),
  );
  const noAtt = tx.moveCall({
    target: "0x1::option::none",
    typeArguments: [`${pkg}::price::PriceAttestation`],
    arguments: [],
  });
  const position = tx.moveCall({
    target: `${pkg}::vault::deposit`,
    typeArguments: [p.depositCoinType],
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.protocolConfigId),
      tx.object(requireWhitelist()),
      appraisal,
      funds,
      noAtt,
      tx.pure.u8(p.trancheCode),
      tx.object(CLOCK_ID),
    ],
  });
  tx.transferObjects([position], p.sender);
  return tx;
}

// ═══════════════════════════ withdrawal queue ═══════════════════════════

export type TradingVaultWithdrawParams = {
  vaultId: string;
  /** The wallet-held `VaultPosition` object to CONSUME (v2 — whole
   * positions only; partial exit = `buildSplitThenWithdrawTx`). */
  positionId: string;
  /** Requested payout asset — the `P` type arg; must be on the vault's
   * `deposit_assets` allowlist. */
  payoutCoinType: string;
};

/**
 * `vault::request_withdraw<P>` — queue a lane-FIFO withdrawal by consuming
 * the whole position object, payable in `P`.
 */
export function buildTradingVaultWithdrawTx(p: TradingVaultWithdrawParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::request_withdraw`,
    typeArguments: [p.payoutCoinType],
    arguments: [tx.object(p.vaultId), tx.object(p.positionId), tx.object(CLOCK_ID)],
  });
  return tx;
}

export type SplitThenWithdrawParams = TradingVaultWithdrawParams & {
  /** Shares to split OFF and queue, in atomic share units (u128); must be
   * strictly less than the position's shares. */
  sharesRaw: bigint;
};

/**
 * Partial exit (v2): `vault_position::split` the requested shares out of the
 * position, then `vault::request_withdraw<P>` the CHILD. The parent stays in
 * the wallet with the remaining shares and pro-rata basis.
 */
export function buildSplitThenWithdrawTx(p: SplitThenWithdrawParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const child = tx.moveCall({
    target: `${pkg}::vault_position::split`,
    arguments: [tx.object(p.positionId), tx.pure.u128(p.sharesRaw)],
  });
  tx.moveCall({
    target: `${pkg}::vault::request_withdraw`,
    typeArguments: [p.payoutCoinType],
    arguments: [tx.object(p.vaultId), child, tx.object(CLOCK_ID)],
  });
  return tx;
}

export type AmendPayoutAssetParams = {
  vaultId: string;
  /** GLOBAL sequence number of the pending request (v2). */
  seq: bigint;
  /** New payout asset — must be on the vault's allowlist. */
  payoutCoinType: string;
};

/**
 * `vault::amend_payout_asset<P>` (SO-370) — the recipient re-points a pending
 * request's payout asset; the unwedge lever when the vault cannot source the
 * originally requested asset.
 */
export function buildAmendPayoutAssetTx(p: AmendPayoutAssetParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::amend_payout_asset`,
    typeArguments: [p.payoutCoinType],
    arguments: [tx.object(p.vaultId), tx.pure.u64(p.seq)],
  });
  return tx;
}

// ═══════════════════════ position split / merge / transfer ═══════════════════════

export type SplitPositionParams = {
  /** The wallet-held `VaultPosition` to split. */
  positionId: string;
  /** Shares for the NEW child position (u128, atomic share units). */
  sharesRaw: bigint;
  /** Recipient of the child — usually the sender. */
  recipient: string;
};

/** `vault_position::split` — basis allocates pro rata; both objects keep the
 * same vault, tranche, generation, and lock expiry. */
export function buildSplitPositionTx(p: SplitPositionParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const child = tx.moveCall({
    target: `${pkg}::vault_position::split`,
    arguments: [tx.object(p.positionId), tx.pure.u128(p.sharesRaw)],
  });
  tx.transferObjects([child], p.recipient);
  return tx;
}

export type MergePositionsParams = {
  /** The surviving position. */
  intoPositionId: string;
  /** The position merged in (consumed). */
  fromPositionId: string;
};

/** `vault_position::merge` — identical vault/tranche/generation only; shares
 * and basis ADD, the lock takes the max. */
export function buildMergePositionsTx(p: MergePositionsParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault_position::merge`,
    arguments: [tx.object(p.intoPositionId), tx.object(p.fromPositionId)],
  });
  return tx;
}

export type TransferPositionParams = {
  positionId: string;
  recipient: string;
};

/** Plain object transfer of a `VaultPosition` (key + store — never
 * module-gated). The UI interposes the value-vs-basis disclosure first. */
export function buildTransferPositionTx(p: TransferPositionParams): Transaction {
  const tx = new Transaction();
  tx.transferObjects([tx.object(p.positionId)], p.recipient);
  return tx;
}

export type BurnWipedPositionParams = {
  vaultId: string;
  positionId: string;
};

/** `vault::burn_wiped_position` — cleanup for a wiped-generation junior
 * position (permanently zero value, §8.5). */
export function buildBurnWipedPositionTx(p: BurnWipedPositionParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::burn_wiped_position`,
    arguments: [tx.object(p.vaultId), tx.object(p.positionId)],
  });
  return tx;
}

// ═══════════════════ terminal settlement pool (§8.7) ═══════════════════

export type RedeemSettledPositionParams = {
  vaultId: string;
  /** Shared `VaultProtocolConfig` object id. */
  protocolConfigId: string;
  /** The wallet-held position to redeem (consumed). */
  positionId: string;
  /** The vault's accounting asset — the `T` type arg (settlement pays it). */
  accountingCoinType: string;
};

/** `vault::redeem_settled_position<T>` — redeem directly against the frozen
 * pool: no queue, no appraisal, no keeper. */
export function buildRedeemSettledPositionTx(p: RedeemSettledPositionParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::redeem_settled_position`,
    typeArguments: [p.accountingCoinType],
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.protocolConfigId),
      tx.object(requireTreasury()),
      tx.object(p.positionId),
    ],
  });
  return tx;
}

export type SettleQueuedRequestParams = {
  vaultId: string;
  /** Shared `VaultProtocolConfig` object id. */
  protocolConfigId: string;
  /** GLOBAL sequence of the outstanding queued request. */
  globalSeq: bigint;
  accountingCoinType: string;
};

/** `vault::settle_queued_request<T>` — permissionless: settle an outstanding
 * queued request from the pool at the snapshot entitlement. */
export function buildSettleQueuedRequestTx(p: SettleQueuedRequestParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::settle_queued_request`,
    typeArguments: [p.accountingCoinType],
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.protocolConfigId),
      tx.object(requireTreasury()),
      tx.pure.u64(p.globalSeq),
    ],
  });
  return tx;
}

export type ClaimSettlementCuratorFeesParams = {
  vaultId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
  accountingCoinType: string;
};

/** `vault::claim_settlement_curator_fees<T>` — pay out the curator fees
 * accrued from settlement redemptions. Current-cap-gated, wallet-paid. */
export function buildClaimSettlementCuratorFeesTx(
  p: ClaimSettlementCuratorFeesParams,
): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::claim_settlement_curator_fees`,
    typeArguments: [p.accountingCoinType],
    arguments: [tx.object(p.vaultId), tx.object(p.curatorCapId)],
  });
  return tx;
}

// ═══════════════════ curator asset management (SO-370) ═══════════════════

export type DepositAssetParams = {
  vaultId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
  /** Shared `VaultProtocolConfig` object id (add only — the cap on
   * `max_deposit_assets` reads it). */
  protocolConfigId: string;
  /** The asset to (dis)allow — the `T` type arg. */
  coinType: string;
};

/**
 * `vault::add_deposit_asset<T>` (SO-370) — curator-gated: allow deposits and
 * payout requests in `T`. Curator ops are NOT gas-sponsored — submit
 * wallet-paid.
 */
export function buildAddDepositAssetTx(p: DepositAssetParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::add_deposit_asset`,
    typeArguments: [p.coinType],
    arguments: [tx.object(p.vaultId), tx.object(p.curatorCapId), tx.object(p.protocolConfigId)],
  });
  return tx;
}

/** `vault::remove_deposit_asset<T>` (SO-370) — curator-gated delist; never
 * the accounting asset. Wallet-paid. */
export function buildRemoveDepositAssetTx(
  p: Omit<DepositAssetParams, "protocolConfigId">,
): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::remove_deposit_asset`,
    typeArguments: [p.coinType],
    arguments: [tx.object(p.vaultId), tx.object(p.curatorCapId)],
  });
  return tx;
}

export type SetHaircutsParams = {
  vaultId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
  entryBps: number;
  exitBps: number;
};

/** `vault::set_haircuts` (SO-370) — curator-gated oracle-arb dampers on
 * non-accounting deposits/payouts (both capped on-chain). Wallet-paid. */
export function buildSetHaircutsTx(p: SetHaircutsParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::set_haircuts`,
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.curatorCapId),
      tx.pure.u64(p.entryBps),
      tx.pure.u64(p.exitBps),
    ],
  });
  return tx;
}

export type SetExternalAccountAttestedParams = {
  vaultId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
  /** Shared `VaultProtocolConfig` object id. */
  protocolConfigId: string;
  /** The FROST parent address being registered as the external account. */
  account: string;
  budgetBps: number;
  dailyReleaseBps: number;
  /** Raw 64-byte ed25519 registrar signature, hex (hedge-signer). */
  attestationHex: string;
};

/**
 * `vault::set_external_account_attested` (SO-308) — the curator registers
 * their own FROST parent address against a registrar attestation, instead
 * of handing an admin a `set_external_account` invocation. Budgets are
 * capped on-chain (2000 / 1000 bps); above that it stays an admin act.
 *
 * `equity_oracle` is a `TypeName` — NOT an allowed pure-input struct (the
 * runtime answers InvalidUsageOfPureArg), so it is constructed on-chain with
 * `0x1::type_name::with_defining_ids<T>()` and passed as a result argument,
 * the same shape the deployment-manager's allowlist PTB uses.
 *
 * Curator ops are NOT gas-sponsored — submit wallet-paid.
 */
export function buildSetExternalAccountAttestedTx(
  p: SetExternalAccountAttestedParams,
): Transaction {
  const pkg = requirePackage();
  const gov = TRADING_VAULT_OBJECTS;
  if (!gov) throw new Error("trading-vault governance objects not served by token-info");
  if (!EQUITY_ORACLE_PACKAGE_ID) {
    throw new Error("equity-oracle package not deployed on this network");
  }
  const tx = new Transaction();
  const witness = tx.moveCall({
    target: "0x1::type_name::with_defining_ids",
    typeArguments: [`${EQUITY_ORACLE_PACKAGE_ID}::equity_oracle::EquityOracle`],
    arguments: [],
  });
  tx.moveCall({
    target: `${pkg}::vault::set_external_account_attested`,
    arguments: [
      tx.object(p.curatorCapId),
      tx.object(p.vaultId),
      tx.object(p.protocolConfigId),
      tx.object(gov.oracleRegistryId),
      tx.pure.address(p.account),
      witness,
      tx.pure.u64(p.budgetBps),
      tx.pure.u64(p.dailyReleaseBps),
      tx.pure(bcs.vector(bcs.u8()).serialize(fromHex(p.attestationHex))),
    ],
  });
  return tx;
}

export type CuratorTakerSwapParams = {
  vaultId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
  /** Allowlisted DeepBook `Pool<B, Q>` object id + its type args. */
  poolId: string;
  baseType: string;
  quoteType: string;
  /** true = sell Base for Quote, false = buy Base with Quote. */
  baseForQuote: boolean;
  /** Input amount in the input asset's smallest units. */
  amountRaw: bigint;
  /** Minimum acceptable output in the output asset's smallest units. */
  minOutRaw: bigint;
};

/**
 * `deepbook_adapter::taker_swap_base_for_quote` / `taker_swap_quote_for_base`
 * (SO-299 spot tab): a curator taker swap of vault FREE balances against an
 * allowlisted pool — a single session-gated call, no custody or appraisal
 * needed. The custody surface (`init_custody` → BM deposits → resting
 * limit/market orders) is deferred: it needs custody discovery, order
 * management, and per-pool book UX well beyond a first spot tab.
 *
 * Curator ops are NOT gas-sponsored (sui-tx template.rs) — submit wallet-paid.
 */
export function buildCuratorTakerSwapTx(p: CuratorTakerSwapParams): Transaction {
  const pkg = DEEPBOOK_ADAPTER_PACKAGE_ID;
  if (!pkg) throw new Error("deepbook-adapter package not deployed on this network");
  const gov = TRADING_VAULT_OBJECTS;
  if (!gov) throw new Error("trading-vault governance objects not served by token-info");
  const fn = p.baseForQuote ? "taker_swap_base_for_quote" : "taker_swap_quote_for_base";
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::deepbook_adapter::${fn}`,
    typeArguments: [p.baseType, p.quoteType],
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.curatorCapId),
      tx.object(gov.integrationRegistryId),
      tx.object(gov.poolAllowlistId),
      tx.object(p.poolId),
      tx.pure.u64(p.amountRaw),
      tx.pure.u64(p.minOutRaw),
      tx.object(CLOCK_ID),
    ],
  });
  return tx;
}
