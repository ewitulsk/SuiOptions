//! DeepBook-Margin hedge venue (doc 04 §3c): the desk's short lives in a
//! shared `MarginManager` owned by a 2-of-2 multisig (curator member key
//! = this bot's signer; service member key = hedge-signer). Reads are
//! plain RPC/dev-inspect against the shared objects; every state change
//! is a multisig tx co-signed through hedge-signer's `/sign` policy
//! endpoint.
//!
//! Position convention: SHORT = base debt. `borrow_base` → market-sell
//! extends the short; market-buy → `repay_base` reduces it. Carry is the
//! base margin pool's borrow APR, reported NEGATIVE through
//! `funding_rate_annual` (the short always pays).

use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use serde::Deserialize;
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponseOptions};
use sui_types::base_types::{ObjectID, SequenceNumber, SuiAddress};
use sui_types::crypto::{EncodeDecodeBase64, PublicKey, Signature};
use sui_types::multisig::{MultiSig, MultiSigPublicKey};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::signature::GenericSignature;
use sui_types::transaction::{
    Argument, Command, ObjectArg, ProgrammableTransaction, SharedObjectMutability, Transaction,
    TransactionData, TransactionKind,
};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION};

use sui_tx::sui_client::SuiClientWrapper;

use super::hedge::HedgeVenue;

/// Gas budget for multisig-sent DBM txs (matches the bot's other flows).
const GAS_BUDGET: u64 = 100_000_000;

/// DBM interest rates / risk ratios are 9-decimal fixed point
/// (`margin_constants::float_scaling`).
const FLOAT_SCALING: f64 = 1e9;

/// SUI/DBUSDC risk params (doc 04 Phase 0, mainnet-identical): headroom
/// maps risk ratio 1.1 (liquidation) → 0.0 and 2.0 (withdraw-free) → 1.0.
const LIQUIDATION_RISK_RATIO: f64 = 1.1;
const FREE_RISK_RATIO: f64 = 2.0;

/// Parsed `[[desk.hedge.venues]]` entry for kind = "deepbook_margin".
#[derive(Clone, Debug, PartialEq)]
pub struct DbmVenueConfig {
    pub margin_package: ObjectID,
    pub margin_registry_id: ObjectID,
    pub margin_manager_id: ObjectID,
    pub deepbook_pool_id: ObjectID,
    pub base_margin_pool_id: ObjectID,
    pub quote_margin_pool_id: ObjectID,
    /// Canonical base/quote coin types of the manager's pool.
    pub base_type: String,
    pub quote_type: String,
    /// Pyth `PriceInfoObject`s (DBM entry calls take them by ref).
    pub base_price_info_id: ObjectID,
    pub quote_price_info_id: ObjectID,
    /// hedge-signer base URL + multisig address: both present ⇒ live
    /// submission enabled; otherwise the venue is read-only.
    pub signer_url: Option<String>,
    pub multisig_address: Option<SuiAddress>,
}

impl DbmVenueConfig {
    pub fn from_toml(v: &super::hedge::HedgeVenueToml) -> Result<Self> {
        let id = |field: &str, val: &Option<String>| -> Result<ObjectID> {
            let s = val.as_deref().ok_or_else(|| {
                anyhow!("[[desk.hedge.venues]] deepbook_margin entry missing {field}")
            })?;
            ObjectID::from_hex_literal(s.trim())
                .with_context(|| format!("[[desk.hedge.venues]] bad {field} {s:?}"))
        };
        let ty = |field: &str, val: &Option<String>| -> Result<String> {
            let s = val.as_deref().ok_or_else(|| {
                anyhow!("[[desk.hedge.venues]] deepbook_margin entry missing {field}")
            })?;
            Ok(protocol_types::asset::canonicalize_move_type(s.trim()))
        };
        let multisig_address = v
            .multisig_address
            .as_deref()
            .map(|s| {
                SuiAddress::from_str(s.trim())
                    .map_err(|e| anyhow!("[[desk.hedge.venues]] bad multisig_address {s:?}: {e}"))
            })
            .transpose()?;
        Ok(Self {
            margin_package: id("margin_package", &v.margin_package)?,
            margin_registry_id: id("margin_registry_id", &v.margin_registry_id)?,
            margin_manager_id: id("margin_manager_id", &v.margin_manager_id)?,
            deepbook_pool_id: id("deepbook_pool_id", &v.deepbook_pool_id)?,
            base_margin_pool_id: id("base_margin_pool_id", &v.base_margin_pool_id)?,
            quote_margin_pool_id: id("quote_margin_pool_id", &v.quote_margin_pool_id)?,
            base_type: ty("base_type", &v.base_type)?,
            quote_type: ty("quote_type", &v.quote_type)?,
            base_price_info_id: id("base_price_info_id", &v.base_price_info_id)?,
            quote_price_info_id: id("quote_price_info_id", &v.quote_price_info_id)?,
            signer_url: v.signer_url.as_deref().map(|s| s.trim_end_matches('/').to_string()),
            multisig_address,
        })
    }
}

// ── pure PTB builders ──────────────────────────────────────────────────
//
// Shared-object identities are resolved once at boot (`initial_shared_
// version` never changes), so the builders are pure and shape-testable.

/// Resolved shared-object identities: (id, initial_shared_version).
#[derive(Clone, Copy, Debug)]
pub struct DbmRefs {
    pub manager: (ObjectID, SequenceNumber),
    pub registry: (ObjectID, SequenceNumber),
    pub pool: (ObjectID, SequenceNumber),
    pub base_margin_pool: (ObjectID, SequenceNumber),
    pub quote_margin_pool: (ObjectID, SequenceNumber),
    pub base_oracle: (ObjectID, SequenceNumber),
    pub quote_oracle: (ObjectID, SequenceNumber),
}

fn shared(
    pt: &mut ProgrammableTransactionBuilder,
    (id, ver): (ObjectID, SequenceNumber),
    mutable: bool,
) -> Result<Argument> {
    Ok(pt.obj(ObjectArg::SharedObject {
        id,
        initial_shared_version: ver,
        mutability: if mutable {
            SharedObjectMutability::Mutable
        } else {
            SharedObjectMutability::Immutable
        },
    })?)
}

fn clock(pt: &mut ProgrammableTransactionBuilder) -> Result<Argument> {
    Ok(pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?)
}

fn call(
    pt: &mut ProgrammableTransactionBuilder,
    pkg: ObjectID,
    module: &str,
    function: &str,
    type_args: Vec<TypeTag>,
    args: Vec<Argument>,
) -> Argument {
    pt.programmable_move_call(
        pkg,
        Identifier::new(module).expect("static module name"),
        Identifier::new(function).expect("static function name"),
        type_args,
        args,
    )
}

fn type_args(cfg: &DbmVenueConfig) -> Result<(TypeTag, TypeTag)> {
    Ok((
        TypeTag::from_str(&cfg.base_type).context("parsing base_type")?,
        TypeTag::from_str(&cfg.quote_type).context("parsing quote_type")?,
    ))
}

/// `pool_proxy::place_market_order_v2<Base, Quote>` — appended to both
/// adjust directions.
fn market_order(
    pt: &mut ProgrammableTransactionBuilder,
    cfg: &DbmVenueConfig,
    refs: &DbmRefs,
    quantity: u64,
    is_bid: bool,
    client_order_id: u64,
) -> Result<()> {
    let (base, quote) = type_args(cfg)?;
    let registry = shared(pt, refs.registry, false)?;
    let manager = shared(pt, refs.manager, true)?;
    let pool = shared(pt, refs.pool, true)?;
    let bmp = shared(pt, refs.base_margin_pool, false)?;
    let qmp = shared(pt, refs.quote_margin_pool, false)?;
    let b_oracle = shared(pt, refs.base_oracle, false)?;
    let q_oracle = shared(pt, refs.quote_oracle, false)?;
    let order_id = pt.pure(client_order_id)?;
    let self_matching = pt.pure(0u8)?;
    let qty = pt.pure(quantity)?;
    let bid = pt.pure(is_bid)?;
    let pay_with_deep = pt.pure(false)?;
    let clk = clock(pt)?;
    call(
        pt,
        cfg.margin_package,
        "pool_proxy",
        "place_market_order_v2",
        vec![base, quote],
        vec![
            registry, manager, pool, bmp, qmp, b_oracle, q_oracle, order_id, self_matching, qty,
            bid, pay_with_deep, clk,
        ],
    );
    Ok(())
}

/// Extend the short: `margin_manager::borrow_base` then market-sell the
/// borrowed base.
pub fn borrow_and_sell_ptb(
    cfg: &DbmVenueConfig,
    refs: &DbmRefs,
    amount: u64,
    client_order_id: u64,
) -> Result<ProgrammableTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let (base, quote) = type_args(cfg)?;
    let manager = shared(&mut pt, refs.manager, true)?;
    let registry = shared(&mut pt, refs.registry, false)?;
    let bmp = shared(&mut pt, refs.base_margin_pool, true)?;
    let b_oracle = shared(&mut pt, refs.base_oracle, false)?;
    let q_oracle = shared(&mut pt, refs.quote_oracle, false)?;
    let pool = shared(&mut pt, refs.pool, false)?;
    let loan = pt.pure(amount)?;
    let clk = clock(&mut pt)?;
    call(
        &mut pt,
        cfg.margin_package,
        "margin_manager",
        "borrow_base",
        vec![base, quote],
        vec![manager, registry, bmp, b_oracle, q_oracle, pool, loan, clk],
    );
    market_order(&mut pt, cfg, refs, amount, false, client_order_id)?;
    Ok(pt.finish())
}

/// Reduce the short: market-buy the base back then `repay_base`.
/// `repay_amount = None` repays min(balance, debt) — the flat-target case.
pub fn buy_and_repay_ptb(
    cfg: &DbmVenueConfig,
    refs: &DbmRefs,
    quantity: u64,
    repay_amount: Option<u64>,
    client_order_id: u64,
) -> Result<ProgrammableTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    market_order(&mut pt, cfg, refs, quantity, true, client_order_id)?;
    let (base, quote) = type_args(cfg)?;
    let manager = shared(&mut pt, refs.manager, true)?;
    let registry = shared(&mut pt, refs.registry, false)?;
    let bmp = shared(&mut pt, refs.base_margin_pool, true)?;
    let amount = pt.pure(repay_amount)?;
    let clk = clock(&mut pt)?;
    call(
        &mut pt,
        cfg.margin_package,
        "margin_manager",
        "repay_base",
        vec![base, quote],
        vec![manager, registry, bmp, amount, clk],
    );
    Ok(pt.finish())
}

/// Emergency margin top-up (doc 04 §4): split `amount` off an owned
/// quote coin and `margin_manager::deposit` it. Deposit-only PTBs land in
/// hedge-signer's Emergency tier (auto-approved risk-raising).
pub fn deposit_quote_ptb(
    cfg: &DbmVenueConfig,
    refs: &DbmRefs,
    coin: sui_types::base_types::ObjectRef,
    amount: u64,
) -> Result<ProgrammableTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let (base, quote) = type_args(cfg)?;
    let coin_arg = pt.obj(ObjectArg::ImmOrOwnedObject(coin))?;
    let amt = pt.pure(amount)?;
    pt.command(Command::SplitCoins(coin_arg, vec![amt]));
    let split = Argument::Result(0);
    let manager = shared(&mut pt, refs.manager, true)?;
    let registry = shared(&mut pt, refs.registry, false)?;
    let b_oracle = shared(&mut pt, refs.base_oracle, false)?;
    let q_oracle = shared(&mut pt, refs.quote_oracle, false)?;
    let clk = clock(&mut pt)?;
    call(
        &mut pt,
        cfg.margin_package,
        "margin_manager",
        "deposit",
        vec![base, quote.clone(), quote],
        vec![manager, registry, b_oracle, q_oracle, split, clk],
    );
    Ok(pt.finish())
}

/// Read PTB: `margin_manager::borrowed_shares` → (base_shares, quote_shares).
pub fn borrowed_shares_ptb(cfg: &DbmVenueConfig, refs: &DbmRefs) -> Result<ProgrammableTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let (base, quote) = type_args(cfg)?;
    let manager = shared(&mut pt, refs.manager, false)?;
    call(
        &mut pt,
        cfg.margin_package,
        "margin_manager",
        "borrowed_shares",
        vec![base, quote],
        vec![manager],
    );
    Ok(pt.finish())
}

/// Read PTB: `margin_manager::calculate_debts<Base, Quote, Base>` against
/// the BASE margin pool → (base_debt, quote_debt). Only valid when the
/// manager's debt side is base (the short convention).
pub fn base_debts_ptb(cfg: &DbmVenueConfig, refs: &DbmRefs) -> Result<ProgrammableTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let (base, quote) = type_args(cfg)?;
    let manager = shared(&mut pt, refs.manager, false)?;
    let bmp = shared(&mut pt, refs.base_margin_pool, false)?;
    let clk = clock(&mut pt)?;
    call(
        &mut pt,
        cfg.margin_package,
        "margin_manager",
        "calculate_debts",
        vec![base.clone(), quote, base],
        vec![manager, bmp, clk],
    );
    Ok(pt.finish())
}

/// Read PTB: `margin_pool::interest_rate<Base>` (9-dec borrow APR).
pub fn interest_rate_ptb(cfg: &DbmVenueConfig, refs: &DbmRefs) -> Result<ProgrammableTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let (base, _) = type_args(cfg)?;
    let bmp = shared(&mut pt, refs.base_margin_pool, false)?;
    call(
        &mut pt,
        cfg.margin_package,
        "margin_pool",
        "interest_rate",
        vec![base],
        vec![bmp],
    );
    Ok(pt.finish())
}

/// Read PTB: `margin_manager::risk_ratio_unsafe` (9-dec; unsafe variant so
/// a stale Pyth object can't abort a monitoring read).
pub fn risk_ratio_ptb(cfg: &DbmVenueConfig, refs: &DbmRefs) -> Result<ProgrammableTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let (base, quote) = type_args(cfg)?;
    let manager = shared(&mut pt, refs.manager, false)?;
    let registry = shared(&mut pt, refs.registry, false)?;
    let b_oracle = shared(&mut pt, refs.base_oracle, false)?;
    let q_oracle = shared(&mut pt, refs.quote_oracle, false)?;
    let pool = shared(&mut pt, refs.pool, false)?;
    let bmp = shared(&mut pt, refs.base_margin_pool, false)?;
    let qmp = shared(&mut pt, refs.quote_margin_pool, false)?;
    let clk = clock(&mut pt)?;
    call(
        &mut pt,
        cfg.margin_package,
        "margin_manager",
        "risk_ratio_unsafe",
        vec![base, quote],
        vec![manager, registry, b_oracle, q_oracle, pool, bmp, qmp, clk],
    );
    Ok(pt.finish())
}

// ── multisig assembly ──────────────────────────────────────────────────

/// Build the 2-of-2 `GenericSignature` from the service + curator member
/// signatures. Committee order is [service, curator] (mirrors the
/// hedge-signer bring-up ceremony); the derived multisig address must
/// match the configured one — a mismatch means wrong member keys.
pub fn assemble_multisig(
    service_pk: PublicKey,
    curator_pk: PublicKey,
    service_sig: Signature,
    curator_sig: Signature,
    expected_address: SuiAddress,
) -> Result<GenericSignature> {
    let ms_pk = MultiSigPublicKey::new(vec![service_pk, curator_pk], vec![1, 1], 2)
        .map_err(|e| anyhow!("building 2-of-2 multisig pubkey: {e}"))?;
    let derived = SuiAddress::from(&ms_pk);
    if derived != expected_address {
        bail!(
            "multisig committee derives {derived}, config says {expected_address} — \
             wrong member keys?"
        );
    }
    let ms = MultiSig::combine(
        vec![
            GenericSignature::Signature(service_sig),
            GenericSignature::Signature(curator_sig),
        ],
        ms_pk,
    )
    .map_err(|e| anyhow!("combining multisig signatures: {e}"))?;
    Ok(GenericSignature::MultiSig(ms))
}

// ── the venue ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PubkeyResp {
    public_key_b64: String,
}

#[derive(Deserialize)]
struct SignResp {
    signature_b64: String,
    tier: String,
}

pub struct DeepbookMarginVenue {
    name: String,
    cfg: DbmVenueConfig,
    /// The desk's vault id (hex literal) — hedge-signer's policy key.
    vault_id: String,
    wrap: SuiClientWrapper,
    http: reqwest::Client,
    refs: DbmRefs,
}

impl DeepbookMarginVenue {
    /// Resolve the shared-object identities once and build the venue.
    pub async fn connect(
        name: impl Into<String>,
        cfg: DbmVenueConfig,
        vault_id: String,
        wrap: SuiClientWrapper,
    ) -> Result<Self> {
        let resolve = |id: ObjectID| {
            let client = &wrap.client;
            async move {
                match sui_tx::tx::shared_object_arg(client, id, false).await? {
                    ObjectArg::SharedObject {
                        id,
                        initial_shared_version,
                        ..
                    } => Ok::<_, anyhow::Error>((id, initial_shared_version)),
                    other => bail!("{id} resolved to a non-shared arg: {other:?}"),
                }
            }
        };
        let refs = DbmRefs {
            manager: resolve(cfg.margin_manager_id).await?,
            registry: resolve(cfg.margin_registry_id).await?,
            pool: resolve(cfg.deepbook_pool_id).await?,
            base_margin_pool: resolve(cfg.base_margin_pool_id).await?,
            quote_margin_pool: resolve(cfg.quote_margin_pool_id).await?,
            base_oracle: resolve(cfg.base_price_info_id).await?,
            quote_oracle: resolve(cfg.quote_price_info_id).await?,
        };
        Ok(Self {
            name: name.into(),
            cfg,
            vault_id,
            wrap,
            http: reqwest::Client::new(),
            refs,
        })
    }

    /// Dev-inspect a single-command read PTB; returns the command's raw
    /// return values.
    async fn inspect(&self, pt: ProgrammableTransaction, label: &str) -> Result<Vec<Vec<u8>>> {
        let resp = self
            .wrap
            .client
            .read_api()
            .dev_inspect_transaction_block(
                self.wrap.signer.address,
                TransactionKind::ProgrammableTransaction(pt),
                None,
                None,
                None,
            )
            .await
            .with_context(|| format!("devInspect {label}"))?;
        let results = resp
            .results
            .ok_or_else(|| anyhow!("devInspect {label} returned no results: {:?}", resp.error))?;
        let first = results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("devInspect {label}: empty results"))?;
        Ok(first.return_values.into_iter().map(|(bytes, _)| bytes).collect())
    }

    async fn borrowed_shares(&self) -> Result<(u64, u64)> {
        let vals = self
            .inspect(borrowed_shares_ptb(&self.cfg, &self.refs)?, "borrowed_shares")
            .await?;
        let get = |i: usize| -> Result<u64> {
            vals.get(i)
                .map(|b| bcs::from_bytes(b))
                .transpose()?
                .ok_or_else(|| anyhow!("borrowed_shares: missing return value {i}"))
        };
        Ok((get(0)?, get(1)?))
    }

    /// The live-submission ceremony: gas-select on the multisig, sign
    /// via hedge-signer + own member key, aggregate, submit, assert
    /// success. Errors bubble to the caller (the rebalancer fires the
    /// `tx-failed-mm-bot-desk` alert at the service layer).
    async fn submit_multisig(&self, ptb: ProgrammableTransaction, label: &str) -> Result<()> {
        let (Some(signer_url), Some(ms_addr)) =
            (self.cfg.signer_url.as_deref(), self.cfg.multisig_address)
        else {
            bail!(
                "venue {}: live submission disabled — configure signer_url AND \
                 multisig_address on the [[desk.hedge.venues]] entry",
                self.name
            );
        };
        let client = &self.wrap.client;

        // Gas: the multisig pays its own gas (funded at bring-up).
        let gas_coin = client
            .coin_read_api()
            .get_coins(ms_addr, None, None, Some(10))
            .await
            .context("listing multisig gas coins")?
            .data
            .into_iter()
            .max_by_key(|c| c.balance)
            .ok_or_else(|| anyhow!("multisig {ms_addr} has no SUI to pay gas"))?;
        let gas_price = client
            .read_api()
            .get_reference_gas_price()
            .await
            .context("fetching reference gas price")?;
        let tx_data = TransactionData::new_programmable(
            ms_addr,
            vec![gas_coin.object_ref()],
            ptb,
            GAS_BUDGET,
            gas_price,
        );

        // Service member: pubkey + policy-gated signature.
        let pk_resp: PubkeyResp = self
            .http
            .get(format!("{signer_url}/pubkey"))
            .send()
            .await
            .context("GET hedge-signer /pubkey")?
            .error_for_status()
            .context("hedge-signer /pubkey")?
            .json()
            .await
            .context("decoding /pubkey response")?;
        let service_pk = PublicKey::decode_base64(&pk_resp.public_key_b64)
            .map_err(|e| anyhow!("decoding hedge-signer pubkey: {e}"))?;
        let tx_bytes_b64 =
            base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&tx_data)?);
        let sign_resp = self
            .http
            .post(format!("{signer_url}/sign"))
            .json(&serde_json::json!({
                "vault_id": self.vault_id,
                "tx_bytes_b64": tx_bytes_b64,
            }))
            .send()
            .await
            .context("POST hedge-signer /sign")?;
        if !sign_resp.status().is_success() {
            let status = sign_resp.status();
            let body = sign_resp.text().await.unwrap_or_default();
            bail!("hedge-signer refused to co-sign {label} ({status}): {body}");
        }
        let sign_resp: SignResp = sign_resp.json().await.context("decoding /sign response")?;
        let service_sig = Signature::decode_base64(&sign_resp.signature_b64)
            .map_err(|e| anyhow!("decoding hedge-signer signature: {e}"))?;

        // Curator member: the bot's own key.
        let curator_pk = self.wrap.signer.keypair.public();
        let curator_sig = Transaction::signature_from_signer(
            tx_data.clone(),
            Intent::sui_transaction(),
            &self.wrap.signer.keypair,
        );

        let generic =
            assemble_multisig(service_pk, curator_pk, service_sig, curator_sig, ms_addr)?;
        let tx = Transaction::from_generic_sig_data(tx_data, vec![generic]);
        let resp = client
            .quorum_driver_api()
            .execute_transaction_block(
                tx,
                SuiTransactionBlockResponseOptions::new().with_effects(),
                Some(ExecuteTransactionRequestType::WaitForLocalExecution),
            )
            .await
            .with_context(|| format!("submitting {label} tx"))?;
        let effects = resp.effects.as_ref().context("response missing effects")?;
        if effects.status().is_err() {
            bail!("{label} reverted: {:?}", effects.status());
        }
        tracing::info!(
            venue = %self.name,
            digest = %resp.digest,
            tier = %sign_resp.tier,
            label,
            "multisig tx succeeded"
        );
        Ok(())
    }

    /// Emergency top-up (doc 04 §4): deposit `amount` of the QUOTE asset
    /// from the multisig's wallet into the manager, raising the risk
    /// ratio. Deposit-only PTBs are hedge-signer's fast-tracked
    /// Emergency tier.
    pub async fn top_up(&self, amount: u64) -> Result<()> {
        let Some(ms_addr) = self.cfg.multisig_address else {
            bail!(
                "venue {}: live submission disabled — configure signer_url AND \
                 multisig_address on the [[desk.hedge.venues]] entry",
                self.name
            );
        };
        let coin = self
            .wrap
            .client
            .coin_read_api()
            .get_coins(ms_addr, Some(self.cfg.quote_type.clone()), None, Some(50))
            .await
            .context("listing multisig quote coins")?
            .data
            .into_iter()
            .filter(|c| c.balance >= amount)
            .max_by_key(|c| c.balance)
            .ok_or_else(|| {
                anyhow!(
                    "multisig {ms_addr} holds no single {} coin ≥ {amount} for the top-up",
                    self.cfg.quote_type
                )
            })?;
        let pt = deposit_quote_ptb(&self.cfg, &self.refs, coin.object_ref(), amount)?;
        self.submit_multisig(pt, "margin top-up").await
    }
}

#[async_trait]
impl HedgeVenue for DeepbookMarginVenue {
    fn name(&self) -> &str {
        &self.name
    }

    /// SHORT = base debt units (raw). Quote-side debt would mean the
    /// manager is levered LONG — not this desk's trade — and reports 0
    /// with a warning.
    async fn position_units(&self) -> Result<f64> {
        let (base_shares, quote_shares) = self.borrowed_shares().await?;
        if base_shares == 0 {
            if quote_shares > 0 {
                tracing::warn!(
                    venue = %self.name,
                    quote_shares,
                    "manager has QUOTE debt (levered long?) — not a short; reporting 0"
                );
            }
            return Ok(0.0);
        }
        let vals = self
            .inspect(base_debts_ptb(&self.cfg, &self.refs)?, "calculate_debts")
            .await?;
        let base_debt: u64 = vals
            .first()
            .map(|b| bcs::from_bytes(b))
            .transpose()?
            .ok_or_else(|| anyhow!("calculate_debts: missing return value"))?;
        Ok(base_debt as f64)
    }

    async fn adjust_to(&self, target_short_units: f64, _spot: f64) -> Result<()> {
        let current = self.position_units().await?;
        let target = target_short_units.max(0.0);
        let delta = target - current;
        let amount = delta.abs().round() as u64;
        if amount == 0 {
            return Ok(());
        }
        let client_order_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let pt = if delta > 0.0 {
            borrow_and_sell_ptb(&self.cfg, &self.refs, amount, client_order_id)?
        } else {
            // Flat target: repay everything the buy-back covers (None =
            // min(balance, debt)); partial: repay exactly the reduction.
            let repay = if target <= 0.0 { None } else { Some(amount) };
            buy_and_repay_ptb(&self.cfg, &self.refs, amount, repay, client_order_id)?
        };
        self.submit_multisig(pt, "hedge adjust").await?;
        tracing::info!(
            venue = %self.name,
            target = target_short_units,
            previous = current,
            delta_units = delta,
            "hedge adjusted"
        );
        Ok(())
    }

    /// NEGATIVE borrow APR: the short always PAYS the base pool's
    /// interest rate (9-dec fixed point on-chain).
    async fn funding_rate_annual(&self) -> Result<f64> {
        let vals = self
            .inspect(interest_rate_ptb(&self.cfg, &self.refs)?, "interest_rate")
            .await?;
        let rate: u64 = vals
            .first()
            .map(|b| bcs::from_bytes(b))
            .transpose()?
            .ok_or_else(|| anyhow!("interest_rate: missing return value"))?;
        Ok(-(rate as f64) / FLOAT_SCALING)
    }

    /// Risk-ratio distance above liquidation: 1.1 → 0.0 (liquidatable),
    /// ≥ 2.0 (withdraw-free) → 1.0. Debt-free managers read the on-chain
    /// max ratio and report fully free.
    async fn margin_headroom(&self) -> Result<f64> {
        let vals = self
            .inspect(risk_ratio_ptb(&self.cfg, &self.refs)?, "risk_ratio")
            .await?;
        let ratio: u64 = vals
            .first()
            .map(|b| bcs::from_bytes(b))
            .transpose()?
            .ok_or_else(|| anyhow!("risk_ratio: missing return value"))?;
        let r = ratio as f64 / FLOAT_SCALING;
        Ok(((r - LIQUIDATION_RISK_RATIO) / (FREE_RISK_RATIO - LIQUIDATION_RISK_RATIO))
            .clamp(0.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_types::crypto::{get_key_pair, AccountKeyPair, SuiKeyPair};

    fn cfg() -> DbmVenueConfig {
        DbmVenueConfig {
            margin_package: ObjectID::from_hex_literal("0xd6").unwrap(),
            margin_registry_id: ObjectID::from_hex_literal("0x48").unwrap(),
            margin_manager_id: ObjectID::from_hex_literal("0x11").unwrap(),
            deepbook_pool_id: ObjectID::from_hex_literal("0x1c").unwrap(),
            base_margin_pool_id: ObjectID::from_hex_literal("0xcd").unwrap(),
            quote_margin_pool_id: ObjectID::from_hex_literal("0xf0").unwrap(),
            base_type: "0x2::sui::SUI".into(),
            quote_type:
                "0x0000000000000000000000000000000000000000000000000000000000000abc::dbusdc::DBUSDC"
                    .into(),
            base_price_info_id: ObjectID::from_hex_literal("0x50").unwrap(),
            quote_price_info_id: ObjectID::from_hex_literal("0x51").unwrap(),
            signer_url: None,
            multisig_address: None,
        }
    }

    fn refs() -> DbmRefs {
        let r = |id: &ObjectID| (*id, SequenceNumber::from_u64(1));
        let c = cfg();
        DbmRefs {
            manager: r(&c.margin_manager_id),
            registry: r(&c.margin_registry_id),
            pool: r(&c.deepbook_pool_id),
            base_margin_pool: r(&c.base_margin_pool_id),
            quote_margin_pool: r(&c.quote_margin_pool_id),
            base_oracle: r(&c.base_price_info_id),
            quote_oracle: r(&c.quote_price_info_id),
        }
    }

    /// (module, function, n_type_args) of every MoveCall, in order.
    fn move_calls(pt: &ProgrammableTransaction) -> Vec<(String, String, usize)> {
        pt.commands
            .iter()
            .filter_map(|c| match c {
                Command::MoveCall(m) => Some((
                    m.module.to_string(),
                    m.function.to_string(),
                    m.type_arguments.len(),
                )),
                _ => None,
            })
            .collect()
    }

    fn shared_input(pt: &ProgrammableTransaction, id: ObjectID) -> SharedObjectMutability {
        for input in &pt.inputs {
            if let sui_types::transaction::CallArg::Object(ObjectArg::SharedObject {
                id: got,
                mutability,
                ..
            }) = input
            {
                if *got == id {
                    return *mutability;
                }
            }
        }
        panic!("shared input {id} not found");
    }

    #[test]
    fn borrow_and_sell_shape() {
        let pt = borrow_and_sell_ptb(&cfg(), &refs(), 5_000_000_000, 42).unwrap();
        assert_eq!(
            move_calls(&pt),
            vec![
                ("margin_manager".into(), "borrow_base".into(), 2),
                ("pool_proxy".into(), "place_market_order_v2".into(), 2),
            ]
        );
        // The manager, pool and base margin pool must be mutable (the
        // builder merges the borrow's &mut with the order's & ref).
        assert_eq!(shared_input(&pt, cfg().margin_manager_id), SharedObjectMutability::Mutable);
        assert_eq!(shared_input(&pt, cfg().deepbook_pool_id), SharedObjectMutability::Mutable);
        assert_eq!(
            shared_input(&pt, cfg().base_margin_pool_id),
            SharedObjectMutability::Mutable
        );
        assert_eq!(
            shared_input(&pt, cfg().margin_registry_id),
            SharedObjectMutability::Immutable
        );
        // Every call targets the margin package.
        for c in &pt.commands {
            if let Command::MoveCall(m) = c {
                assert_eq!(m.package, cfg().margin_package);
            }
        }
    }

    #[test]
    fn buy_and_repay_shape() {
        let pt = buy_and_repay_ptb(&cfg(), &refs(), 3_000_000_000, Some(3_000_000_000), 7).unwrap();
        assert_eq!(
            move_calls(&pt),
            vec![
                ("pool_proxy".into(), "place_market_order_v2".into(), 2),
                ("margin_manager".into(), "repay_base".into(), 2),
            ]
        );
        // Flat-target variant passes a None repay amount (repay-all).
        let flat = buy_and_repay_ptb(&cfg(), &refs(), 3_000_000_000, None, 7).unwrap();
        assert_eq!(move_calls(&flat).len(), 2);
        // The None encodes as an empty Move option (single 0 byte).
        let none_input = flat
            .inputs
            .iter()
            .filter_map(|i| match i {
                sui_types::transaction::CallArg::Pure(b) => Some(b.clone()),
                _ => None,
            })
            .find(|b| b == &vec![0u8]);
        assert!(none_input.is_some(), "expected an option::none pure input");
    }

    #[test]
    fn deposit_shape_is_emergency_compatible() {
        let coin = (
            ObjectID::from_hex_literal("0x99").unwrap(),
            SequenceNumber::from_u64(3),
            sui_types::digests::ObjectDigest::random(),
        );
        let pt = deposit_quote_ptb(&cfg(), &refs(), coin, 1_000_000).unwrap();
        // SplitCoins (neutral) then deposit — the ONLY MoveCall, so the
        // hedge-signer policy classifies it Emergency.
        assert_eq!(
            move_calls(&pt),
            vec![("margin_manager".into(), "deposit".into(), 3)]
        );
        assert!(matches!(pt.commands[0], Command::SplitCoins(..)));
        assert_eq!(pt.commands.len(), 2);
    }

    #[test]
    fn read_ptbs_are_single_call() {
        for (pt, module, function) in [
            (borrowed_shares_ptb(&cfg(), &refs()).unwrap(), "margin_manager", "borrowed_shares"),
            (base_debts_ptb(&cfg(), &refs()).unwrap(), "margin_manager", "calculate_debts"),
            (interest_rate_ptb(&cfg(), &refs()).unwrap(), "margin_pool", "interest_rate"),
            (risk_ratio_ptb(&cfg(), &refs()).unwrap(), "margin_manager", "risk_ratio_unsafe"),
        ] {
            let calls = move_calls(&pt);
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, module);
            assert_eq!(calls[0].1, function);
        }
        // calculate_debts pins DebtAsset = Base (3 type args).
        assert_eq!(move_calls(&base_debts_ptb(&cfg(), &refs()).unwrap())[0].2, 3);
    }

    #[test]
    fn multisig_assembly_derives_the_committee_address() {
        let (_, service): (_, AccountKeyPair) = get_key_pair();
        let (_, curator): (_, AccountKeyPair) = get_key_pair();
        let service = SuiKeyPair::Ed25519(service);
        let curator = SuiKeyPair::Ed25519(curator);

        let ms_pk =
            MultiSigPublicKey::new(vec![service.public(), curator.public()], vec![1, 1], 2)
                .unwrap();
        let expected = SuiAddress::from(&ms_pk);
        // The multisig address is its own thing — neither member's.
        assert_ne!(expected, SuiAddress::from(&service.public()));
        assert_ne!(expected, SuiAddress::from(&curator.public()));

        // Sign an (arbitrary) TransactionData with both members.
        let tx_data = TransactionData::new_programmable(
            expected,
            vec![(
                ObjectID::from_hex_literal("0x9").unwrap(),
                SequenceNumber::from_u64(1),
                sui_types::digests::ObjectDigest::random(),
            )],
            ProgrammableTransactionBuilder::new().finish(),
            GAS_BUDGET,
            1000,
        );
        let s_sig =
            Transaction::signature_from_signer(tx_data.clone(), Intent::sui_transaction(), &service);
        let c_sig =
            Transaction::signature_from_signer(tx_data.clone(), Intent::sui_transaction(), &curator);

        let generic = assemble_multisig(
            service.public(),
            curator.public(),
            s_sig.clone(),
            c_sig.clone(),
            expected,
        )
        .unwrap();
        let GenericSignature::MultiSig(ms) = generic else {
            panic!("expected a MultiSig")
        };
        assert_eq!(ms.get_bitmap(), 0b11, "both member signatures present");
        assert_eq!(SuiAddress::from(ms.get_pk()), expected);

        // A committee that doesn't derive the configured address is refused.
        let wrong = SuiAddress::from(&curator.public());
        let err = assemble_multisig(service.public(), curator.public(), s_sig, c_sig, wrong)
            .unwrap_err();
        assert!(err.to_string().contains("wrong member keys"), "{err}");
    }
}
