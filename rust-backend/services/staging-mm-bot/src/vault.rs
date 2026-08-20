//! Resolving the direct-escrow trading vault the bot quotes for — adopt,
//! or provision one (SO-372 runtime, provisioning modeled on mm-bot's desk,
//! SO-345).
//!
//! Vault-direct quoting needs four pieces of on-chain wiring: an open vault
//! in the right accounting asset, a direct `ExchangeCustody` (whose identity
//! BalanceManager the orders name), this wallet delegated as an approved
//! signer on that BM, and the funding pass's tokens on the vault's
//! deposit-asset allowlist. All of it used to be a hand-run ceremony
//! (`trading-vault-smoke --direct-escrow`) plus a config re-pin after every
//! contract redeploy; this module makes the bot do it itself so a redeploy
//! needs no manual step.
//!
//! ## Adoption rules (same provenance argument as mm-bot's desk)
//!
//! `trading_vault::create_vault` is permissionless and the `CuratorCap` is
//! freely transferable, so holding a cap proves nothing. `creator` is
//! `ctx.sender()` at creation, immutable, and indexed:
//!
//! - **Pinned** (`[vault_direct].vault_id` set): adopt that vault whatever
//!   its provenance — the operator asserted intent. Still verified against
//!   chain state, and its wiring is finished if this wallet holds the cap.
//! - **Auto** (`vault_id` empty + `[vault_direct.provision].enabled`): only
//!   ever adopt a vault this wallet created. Nothing else is a candidate.
//!
//! Wiring state is read from durable sources (the indexer's
//! `TvExchangeCustodyCreated` events and dev-inspected chain views), never
//! from RPC event history — the provider prunes it (SO-369).

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use move_core_types::identifier::Identifier;
use serde::Deserialize;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::TypeTag;
use tracing::{error, info, warn};

use indexer_graphql::{IndexerClient, TradingVault};
use protocol_types::asset::canonicalize_move_type;
use sui_tx::chain::{created_objects, decode_return_value, ChainClient};
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::trading_vault;
use sui_tx::tx::{owned_object_arg, shared_object_arg, submit_ptb_rebuilding};

/// `[vault_direct.provision]` — self-provisioning when discovery finds no
/// vault this wallet created.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProvisionConfig {
    /// Create + wire a vault when there is none of ours to adopt.
    pub enabled: bool,
    /// Depositor lockup. 0 = withdraw any time (subject to the queue).
    pub lockup_ms: u64,
    pub curator_fee_bps: u64,
    pub unwind_grace_ms: u64,
    pub gas_budget: u64,
    /// v2 capital structure (SO-418/SO-420), flattened — same keys as
    /// mm-bot's `[desk.provision]` (`structure_code`, the six tranche
    /// params, `terms_version`, `spec_hash`). Defaults = UNTRANCHED.
    #[serde(flatten)]
    pub tranche: trading_vault::TrancheParams,
}

impl Default for ProvisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lockup_ms: 0,
            curator_fee_bps: 0,
            unwind_grace_ms: 24 * 60 * 60 * 1_000,
            gas_budget: 200_000_000,
            tranche: trading_vault::TrancheParams::default(),
        }
    }
}

pub struct ResolveParams<'a> {
    pub wrap: &'a SuiClientWrapper,
    pub indexer: &'a IndexerClient,
    pub cfg: &'a ProvisionConfig,
    /// `[vault_direct].vault_id`, trimmed. Empty ⇒ auto path.
    pub pinned_vault_id: &'a str,
    pub trading_vault_package: ObjectID,
    pub exchange_adapter_package: ObjectID,
    /// The live exchange package (from the orderbook's `/v1/markets`) —
    /// `balance_manager::is_approved_signer` lives there.
    pub exchange_package: ObjectID,
    /// Shared trading-vault `IntegrationRegistry` (curator sessions).
    pub integration_registry: ObjectID,
    pub vault_protocol_config: ObjectID,
    /// Shared `whitelist::Whitelist` — the ingress gate on `create_vault`
    /// and `vault::deposit` (SO-383).
    pub whitelist: ObjectID,
    /// The bot's accounting asset: the unique quote coin type across the
    /// orderbook's markets. A vault denominated in anything else would put
    /// NAV and every share price against the wrong asset.
    pub accounting_coin_type: &'a str,
    /// Canonical coin types the funding pass deposits (`[funding.targets]`)
    /// — each non-accounting one must be on the vault's deposit-asset
    /// allowlist or `vault::deposit_asset` aborts.
    pub deposit_asset_types: &'a [String],
}

pub struct ResolvedDirectVault {
    pub vault_id: ObjectID,
    /// The vault's direct-custody identity BalanceManager: the manager the
    /// bot's signed orders name.
    pub manager_id: ObjectID,
    /// The vault's `CuratorCap`, when this wallet holds it (SO-418: the
    /// funding pass needs it to fund the escrowed curator commitment via
    /// `deposit_into_commitment`). `None` = commitment upkeep disabled.
    pub curator_cap: Option<ObjectID>,
    /// True when this call created the vault.
    pub provisioned: bool,
    /// Wire code the funding pass deposits into (SO-420): 0 on an
    /// untranched vault, junior on a tranched one — the vault rejects a
    /// mismatched code (abort 121). Fixed at resolution: the capital
    /// structure is immutable per vault.
    pub deposit_tranche: u8,
}

/// The direct custody's ids, from the indexer's durable event view.
#[derive(Debug, Clone, Copy)]
struct Custody {
    custody_id: ObjectID,
    bm_id: ObjectID,
}

/// Resolve the vault the bot will quote for, provisioning one if configured.
pub async fn resolve(p: ResolveParams<'_>) -> Result<ResolvedDirectVault> {
    let me = p.wrap.signer.address;

    if !p.pinned_vault_id.is_empty() {
        let vault_id = ObjectID::from_hex_literal(p.pinned_vault_id)
            .map_err(|e| anyhow!("bad [vault_direct].vault_id {}: {e}", p.pinned_vault_id))?;
        let view = fetch(p.indexer, vault_id).await?.ok_or_else(|| {
            anyhow!(
                "[vault_direct].vault_id {} is not a trading vault of this deployment — it is \
                 most likely pinned from a previous contract publish. Re-pin it, or clear it \
                 and set [vault_direct.provision].enabled = true.",
                vault_id.to_hex_literal()
            )
        })?;
        info!(vault = %vault_id.to_hex_literal(), creator = %view.creator, "adopting pinned vault");
        let (manager_id, curator_cap) = ensure_wired(&p, &view, me).await?;
        return Ok(ResolvedDirectVault {
            vault_id,
            manager_id,
            curator_cap,
            provisioned: false,
            deposit_tranche: trading_vault::deposit_tranche_code(view.structure_code),
        });
    }

    if !p.cfg.enabled {
        bail!(
            "[vault_direct] with no vault_id and [vault_direct.provision].enabled = false — \
             there is no vault to quote against"
        );
    }

    // Auto path. Only vaults this wallet created are candidates; a cap
    // someone else minted for us is deliberately ignored.
    let mine = self_created(p.indexer, me, p.accounting_coin_type).await?;
    let wired = wired_set(&p, &mine).await?;
    if let Some(view) = pick(mine, &wired) {
        let vault_id = ObjectID::new(*view.vault_id.as_bytes());
        info!(vault = %vault_id.to_hex_literal(), "adopting self-created vault");
        let (manager_id, curator_cap) = ensure_wired(&p, &view, me).await?;
        return Ok(ResolvedDirectVault {
            vault_id,
            manager_id,
            curator_cap,
            provisioned: false,
            deposit_tranche: trading_vault::deposit_tranche_code(view.structure_code),
        });
    }

    // Nothing to adopt. Before creating, insist the indexer is current: a
    // lagging view reads exactly like "no vault exists" and would make us
    // create a duplicate on every boot.
    let progress = p
        .indexer
        .progress()
        .await
        .context("reading indexer progress before provisioning a vault")?;
    if !progress.caught_up {
        bail!(
            "refusing to provision a vault while the indexer is behind the chain tip \
             (checkpoint {} of {:?}) — a lagging view is indistinguishable from 'no vault \
             exists' and would create a duplicate",
            progress.current_checkpoint,
            progress.tip_checkpoint
        );
    }

    provision(&p, me).await
}

/// Log the resolution failure the way an operator will find it. A bot
/// without a usable vault quotes nothing, so this is an alert, not a
/// warning to scroll past.
pub fn report_unusable(err: &anyhow::Error) {
    error!(
        alert_id = "staging-mm-bot-vault-unusable",
        error = %format!("{err:#}"),
        "cannot resolve a usable direct-escrow vault — the bot cannot quote"
    );
}

// -- Provisioning --------------------------------------------------------

/// Create and fully wire a fresh vault. No seed step: the SO-375 funding
/// pass runs synchronously right after resolution and its first attested
/// deposit is what gives the vault a NAV.
async fn provision(p: &ResolveParams<'_>, me: SuiAddress) -> Result<ResolvedDirectVault> {
    let created = trading_vault::create_vault(
        &p.wrap.client,
        &p.wrap.signer,
        p.trading_vault_package,
        p.vault_protocol_config,
        p.whitelist,
        p.accounting_coin_type,
        // v2 (SO-420): capital structure from config — untranched by
        // default, senior/junior when `[vault_direct.provision]` sets the
        // tranche params. The escrowed curator commitment is funded by
        // the funding pass right after resolution (it has the appraisal
        // composer; provisioning does not).
        &p.cfg
            .tranche
            .create_vault_spec(p.cfg.lockup_ms, p.cfg.curator_fee_bps, p.cfg.unwind_grace_ms)
            .context("[vault_direct.provision]")?,
        p.cfg.gas_budget,
    )
    .await
    .context("creating the direct-escrow vault")?;
    info!(
        vault = %created.vault_id.to_hex_literal(),
        curator_cap = %created.curator_cap_id.to_hex_literal(),
        digest = %created.digest,
        "provisioned a trading vault"
    );

    let custody = wire(p, created.vault_id, created.curator_cap_id).await?;
    add_signer(p, created.vault_id, created.curator_cap_id, &custody, me).await?;
    // A fresh vault's allowlist holds only the accounting asset.
    let missing = missing_deposit_assets(p.deposit_asset_types, &[], p.accounting_coin_type);
    add_deposit_assets(p, created.vault_id, created.curator_cap_id, &missing).await?;
    info!(vault = %created.vault_id.to_hex_literal(), "vault wired for direct quoting");

    Ok(ResolvedDirectVault {
        vault_id: created.vault_id,
        manager_id: custody.bm_id,
        curator_cap: Some(created.curator_cap_id),
        provisioned: true,
        deposit_tranche: trading_vault::deposit_tranche_code(p.cfg.tranche.structure_code),
    })
}

/// Verify an adopted vault against chain state and finish any wiring a
/// crashed provision (or an older ceremony) left undone. Returns the
/// identity BM the orders will name plus the CuratorCap when this wallet
/// holds it. Errors mean "unusable".
async fn ensure_wired(
    p: &ResolveParams<'_>,
    view: &TradingVault,
    me: SuiAddress,
) -> Result<(ObjectID, Option<ObjectID>)> {
    let vault_id = ObjectID::new(*view.vault_id.as_bytes());

    if view.state != "open" {
        bail!("vault {} is {}, not open", vault_id.to_hex_literal(), view.state);
    }
    if view.deposits_paused {
        // The funding pass IS the vault's liquidity; paused deposits mean
        // the float can never be topped up.
        bail!("vault {} has deposits paused", vault_id.to_hex_literal());
    }
    let want = canonicalize_move_type(p.accounting_coin_type);
    let got = canonicalize_move_type(&view.accounting_asset.to_string());
    if want != got {
        bail!(
            "vault {} is denominated in {got}, but the markets quote in {want}",
            vault_id.to_hex_literal()
        );
    }

    // Ownership comes from a chain read, never the indexer's `curator` —
    // a plain `public_transfer` of a CuratorCap emits no event.
    let cap_id = ObjectID::new(*view.curator_cap_id.as_bytes());
    let cap = match owner_of(&p.wrap.client, cap_id).await {
        Ok(owner) => (owner == Some(me)).then_some(cap_id),
        Err(e) => {
            warn!(error = %format!("{e:#}"), cap = %cap_id.to_hex_literal(), "reading CuratorCap owner failed");
            None
        }
    };

    let custody = match direct_custody(p.indexer, vault_id).await? {
        Some(c) => c,
        None => {
            let Some(cap_id) = cap else {
                bail!(
                    "vault {} has no direct exchange custody and this wallet does not hold \
                     its CuratorCap — cannot wire it for direct quoting",
                    vault_id.to_hex_literal()
                );
            };
            info!(
                vault = %vault_id.to_hex_literal(),
                "adopted vault has no direct custody — finishing its wiring"
            );
            wire(p, vault_id, cap_id).await?
        }
    };

    // The BM the orders will name must be the vault's identity BM: right
    // type, order-attribution owner = the vault's id-as-address.
    let owner = manager_owner(&p.wrap.client, custody.bm_id).await?;
    if owner.to_lowercase() != vault_id.to_string().to_lowercase() {
        bail!(
            "identity BM {} owned by {owner}, not vault {} — not its identity BM",
            custody.bm_id,
            vault_id.to_hex_literal()
        );
    }

    // Delegation: without it every order dies BAD_SIGNATURE at intake.
    if !is_approved_signer(p, custody.bm_id, me).await? {
        let Some(cap_id) = cap else {
            bail!(
                "this wallet is not an approved signer on identity BM {} and does not hold \
                 the CuratorCap to delegate itself — orders would be rejected BAD_SIGNATURE",
                custody.bm_id
            );
        };
        add_signer(p, vault_id, cap_id, &custody, me).await?;
    }

    // Deposit-asset allowlist for the funding pass. Missing entries only
    // degrade funding (that token's float can't be topped up), so without
    // the cap this warns instead of failing the boot.
    let present = deposit_assets(p, vault_id).await?;
    let missing = missing_deposit_assets(p.deposit_asset_types, &present, p.accounting_coin_type);
    if !missing.is_empty() {
        match cap {
            Some(cap_id) => add_deposit_assets(p, vault_id, cap_id, &missing).await?,
            None => warn!(
                vault = %vault_id.to_hex_literal(),
                ?missing,
                "deposit-asset allowlist incomplete and this wallet holds no CuratorCap — \
                 funding deposits of these assets will fail"
            ),
        }
    }

    Ok((custody.bm_id, cap))
}

/// `init_direct_custody` + `add_quote_adapter`, one PTB — atomic, so
/// "custody exists" implies "adapter enabled" and the resume check needs
/// only the custody event.
async fn wire(p: &ResolveParams<'_>, vault_id: ObjectID, cap_id: ObjectID) -> Result<Custody> {
    let client = &p.wrap.client;
    let witness = TypeTag::from_str(&format!(
        "{}::exchange_adapter::ExchangeAdapter",
        p.exchange_adapter_package
    ))?;
    let resp = submit_ptb_rebuilding(
        client,
        &p.wrap.signer,
        p.cfg.gas_budget,
        "exchange_adapter::init_direct_custody",
        || async {
            let mut pt = ProgrammableTransactionBuilder::new();
            let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
            let cap = pt.obj(owned_object_arg(client, cap_id).await?)?;
            let ireg = pt.obj(shared_object_arg(client, p.integration_registry, false).await?)?;
            pt.programmable_move_call(
                p.exchange_adapter_package,
                Identifier::new("exchange_adapter").unwrap(),
                Identifier::new("init_direct_custody").unwrap(),
                vec![],
                vec![vault, cap, ireg],
            );
            pt.programmable_move_call(
                p.trading_vault_package,
                Identifier::new("vault").unwrap(),
                Identifier::new("add_quote_adapter").unwrap(),
                vec![witness.clone()],
                vec![vault, cap],
            );
            Ok(pt.finish())
        },
    )
    .await?;
    let (mut custody_id, mut bm_id) = (None, None);
    for c in created_objects(&resp) {
        let Ok(tag) = sui_types::parse_sui_struct_tag(&c.object_type) else { continue };
        match tag.name.as_str() {
            "ExchangeCustody" => custody_id = Some(c.object_id),
            "BalanceManager" => bm_id = Some(c.object_id),
            _ => {}
        }
    }
    let custody = Custody {
        custody_id: custody_id.ok_or_else(|| anyhow!("no ExchangeCustody created"))?,
        bm_id: bm_id.ok_or_else(|| anyhow!("no identity BalanceManager created"))?,
    };
    client
        .await_object(custody.bm_id, 6)
        .await
        .context("waiting for the identity BM to be readable")?;
    info!(
        vault = %vault_id.to_hex_literal(),
        custody = %custody.custody_id,
        manager = %custody.bm_id,
        "direct custody initialized, quote adapter enabled"
    );
    Ok(custody)
}

/// Delegate `delegate` as an order-signing hot key on the identity BM.
async fn add_signer(
    p: &ResolveParams<'_>,
    vault_id: ObjectID,
    cap_id: ObjectID,
    custody: &Custody,
    delegate: SuiAddress,
) -> Result<()> {
    let client = &p.wrap.client;
    submit_ptb_rebuilding(
        client,
        &p.wrap.signer,
        p.cfg.gas_budget,
        "exchange_adapter::add_signer",
        || async {
            let mut pt = ProgrammableTransactionBuilder::new();
            let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
            let cap = pt.obj(owned_object_arg(client, cap_id).await?)?;
            let ireg = pt.obj(shared_object_arg(client, p.integration_registry, false).await?)?;
            let bm = pt.obj(shared_object_arg(client, custody.bm_id, true).await?)?;
            let custody_arg = pt.pure(custody.custody_id)?;
            let signer = pt.pure(delegate)?;
            pt.programmable_move_call(
                p.exchange_adapter_package,
                Identifier::new("exchange_adapter").unwrap(),
                Identifier::new("add_signer").unwrap(),
                vec![],
                vec![vault, cap, ireg, bm, custody_arg, signer],
            );
            Ok(pt.finish())
        },
    )
    .await
    .context("delegating this wallet on the identity BM")?;
    info!(vault = %vault_id.to_hex_literal(), %delegate, "delegated as approved order signer");
    Ok(())
}

/// Allowlist `types` for deposits (one PTB).
async fn add_deposit_assets(
    p: &ResolveParams<'_>,
    vault_id: ObjectID,
    cap_id: ObjectID,
    types: &[String],
) -> Result<()> {
    if types.is_empty() {
        return Ok(());
    }
    let client = &p.wrap.client;
    let tags = types
        .iter()
        .map(|t| TypeTag::from_str(t).with_context(|| format!("parsing coin type {t}")))
        .collect::<Result<Vec<_>>>()?;
    submit_ptb_rebuilding(
        client,
        &p.wrap.signer,
        p.cfg.gas_budget,
        "vault::add_deposit_asset",
        || async {
            let mut pt = ProgrammableTransactionBuilder::new();
            let vault = pt.obj(shared_object_arg(client, vault_id, true).await?)?;
            let cap = pt.obj(owned_object_arg(client, cap_id).await?)?;
            let cfg = pt.obj(shared_object_arg(client, p.vault_protocol_config, false).await?)?;
            for tag in &tags {
                pt.programmable_move_call(
                    p.trading_vault_package,
                    Identifier::new("vault").unwrap(),
                    Identifier::new("add_deposit_asset").unwrap(),
                    vec![tag.clone()],
                    vec![vault, cap, cfg],
                );
            }
            Ok(pt.finish())
        },
    )
    .await
    .context("allowlisting funding deposit assets")?;
    info!(vault = %vault_id.to_hex_literal(), ?types, "deposit assets allowlisted");
    Ok(())
}

// -- Chain / indexer reads -----------------------------------------------

/// The vault's direct custody, from the indexer's durable event store
/// (RPC event history prunes; the indexer's doesn't, and it is wiped with
/// every redeploy so the view is deployment-scoped for free).
async fn direct_custody(indexer: &IndexerClient, vault_id: ObjectID) -> Result<Option<Custody>> {
    let hex = protocol_types::ids::ObjectId::new(vault_id.into_bytes()).to_hex();
    let events = indexer
        .recent_events_with_payload(
            &["TvExchangeCustodyCreated"],
            serde_json::json!({ "vault_id": hex, "direct": true }),
            16,
        )
        .await
        .context("querying ExchangeCustodyCreated events")?;
    for ev in events.iter().rev() {
        if let protocol_types::events::ChainEvent::TvExchangeCustodyCreated(c) = &ev.event {
            if c.direct {
                return Ok(Some(Custody {
                    custody_id: ObjectID::new(*c.custody_id.as_bytes()),
                    bm_id: ObjectID::new(*c.balance_manager_id.as_bytes()),
                }));
            }
        }
    }
    Ok(None)
}

/// Which of `vaults` already have a direct custody — the adoption
/// preference (a wired vault beats an unwired husk of any stake).
async fn wired_set(
    p: &ResolveParams<'_>,
    vaults: &[TradingVault],
) -> Result<HashSet<protocol_types::ids::ObjectId>> {
    let mut wired = HashSet::new();
    for v in vaults {
        let id = ObjectID::new(*v.vault_id.as_bytes());
        if direct_custody(p.indexer, id).await?.is_some() {
            wired.insert(v.vault_id);
        }
    }
    Ok(wired)
}

/// Every open vault this wallet created in the right accounting asset.
/// `creator` is the tx sender at creation and cannot be spoofed, which is
/// the whole basis for trusting auto-discovery. SO-420: mm-bot's vol desk
/// signs with THIS wallet too, so creator alone cannot tell our vaults
/// from its — skip vaults with `mm_release_enabled` (the desk flips it on
/// as part of provisioning; this bot never does).
async fn self_created(
    indexer: &IndexerClient,
    me: SuiAddress,
    accounting_coin_type: &str,
) -> Result<Vec<TradingVault>> {
    let me = protocol_types::ids::SuiAddress::new(me.to_inner());
    let want = canonicalize_move_type(accounting_coin_type);
    Ok(indexer
        .trading_vaults()
        .await
        .context("listing trading vaults")?
        .into_iter()
        .filter(|v| {
            v.creator == me
                && v.state == "open"
                && !v.mm_release_enabled
                && canonicalize_move_type(&v.accounting_asset.to_string()) == want
        })
        .collect())
}

async fn fetch(indexer: &IndexerClient, vault_id: ObjectID) -> Result<Option<TradingVault>> {
    let hex = vault_id.to_hex_literal();
    Ok(indexer
        .trading_vaults()
        .await
        .context("listing trading vaults")?
        .into_iter()
        .find(|v| format!("0x{}", v.vault_id.to_hex()) == hex))
}

/// Deterministic choice when several self-created vaults exist, so a
/// restart lands on the same one. A vault already wired for direct escrow
/// beats any unwired one (the same wallet also runs smoke tooling), then
/// largest stake (mm-bot's husk lesson, 2026-08-04), then lowest id.
fn pick(
    mut vaults: Vec<TradingVault>,
    wired: &HashSet<protocol_types::ids::ObjectId>,
) -> Option<TradingVault> {
    vaults.sort_by(|a, b| {
        wired
            .contains(&b.vault_id)
            .cmp(&wired.contains(&a.vault_id))
            .then_with(|| b.total_shares.cmp(&a.total_shares))
            .then_with(|| a.vault_id.to_hex().cmp(&b.vault_id.to_hex()))
    });
    if vaults.len() > 1 {
        let others: Vec<String> =
            vaults[1..].iter().map(|v| format!("0x{}", v.vault_id.to_hex())).collect();
        warn!(
            adopted = %format!("0x{}", vaults[0].vault_id.to_hex()),
            ignored = ?others,
            "several self-created trading vaults — adopting the wired/largest one"
        );
    }
    vaults.into_iter().next()
}

/// Configured funding types not yet on the allowlist. The accounting asset
/// is allowlisted at creation and never missing.
fn missing_deposit_assets(
    configured: &[String],
    present: &[String],
    accounting_coin_type: &str,
) -> Vec<String> {
    let accounting = canonicalize_move_type(accounting_coin_type);
    let present: HashSet<String> =
        present.iter().map(|t| canonicalize_move_type(t)).collect();
    configured
        .iter()
        .map(|t| canonicalize_move_type(t))
        .filter(|t| *t != accounting && !present.contains(t))
        .collect()
}

/// The single quote coin type shared by every market — the vault's
/// accounting asset. Refuses a mixed set: one vault cannot denominate two
/// quote assets.
pub fn unique_accounting(quote_types: &[String]) -> Result<String> {
    let mut canon: Vec<String> =
        quote_types.iter().map(|t| canonicalize_move_type(t)).collect();
    canon.sort();
    canon.dedup();
    match canon.len() {
        0 => bail!("no markets, no accounting asset"),
        1 => Ok(canon.remove(0)),
        _ => bail!(
            "markets quote in {} different assets ({canon:?}) — vault-direct mode needs a \
             single accounting asset",
            canon.len()
        ),
    }
}

/// Dev-inspect `balance_manager::is_approved_signer` on the live exchange
/// package — delegation IS readable from chain, no BAD_SIGNATURE probing.
async fn is_approved_signer(
    p: &ResolveParams<'_>,
    bm_id: ObjectID,
    addr: SuiAddress,
) -> Result<bool> {
    let client = &p.wrap.client;
    let mut pt = ProgrammableTransactionBuilder::new();
    let bm = pt.obj(shared_object_arg(client, bm_id, false).await?)?;
    let a = pt.pure(addr)?;
    pt.programmable_move_call(
        p.exchange_package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("is_approved_signer").unwrap(),
        vec![],
        vec![bm, a],
    );
    let res = client
        .dev_inspect_ptb(p.wrap.signer.address, pt)
        .await
        .context("dev-inspecting is_approved_signer")?;
    decode_return_value::<bool>(&res, 0).context("decoding is_approved_signer")
}

/// Dev-inspect `vault::deposit_assets` — the current allowlist as
/// canonical coin-type strings.
async fn deposit_assets(p: &ResolveParams<'_>, vault_id: ObjectID) -> Result<Vec<String>> {
    // BCS of `VecSet<TypeName>`: a vector of TypeName, each an ascii string.
    #[derive(Deserialize)]
    struct TypeName {
        name: String,
    }
    #[derive(Deserialize)]
    struct VecSet {
        contents: Vec<TypeName>,
    }
    let client = &p.wrap.client;
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
    pt.programmable_move_call(
        p.trading_vault_package,
        Identifier::new("vault").unwrap(),
        Identifier::new("deposit_assets").unwrap(),
        vec![],
        vec![vault],
    );
    let res = client
        .dev_inspect_ptb(p.wrap.signer.address, pt)
        .await
        .context("dev-inspecting deposit_assets")?;
    let set = decode_return_value::<VecSet>(&res, 0).context("decoding deposit_assets")?;
    // TypeName strings carry no 0x prefix — canonicalize before comparing.
    Ok(set.contents.iter().map(|t| canonicalize_move_type(&t.name)).collect())
}

/// SO-418: is this vault "risk-off" for quoting? Mirrors the §8.4b gate
/// set the on-chain releases abort on (code 124) plus the terminal
/// states: capital risk state not Healthy, curator commitment breached,
/// lifecycle not open, or settled. The quoter hard-stops on this BEFORE
/// signing orders whose fills would abort at settlement.
pub fn risk_off(v: &TradingVault) -> bool {
    v.risk_state != 0 || v.curator_commitment_breached || v.state != "open" || v.settled
}

/// Mirror of the Move `SHARE_OFFSET` (shares are offset-scaled vs value).
const SHARE_OFFSET: u128 = 1_000_000;

/// SO-418: the junior/risk-bearing capital measure of a TRANCHED vault,
/// accounting-asset raw units — `junior_nav` from the latest capital
/// sync, falling back to the observed junior pps × junior shares. The
/// senior claim is not the bot's to quote against. `None` for untranched
/// vaults (the whole book is risk capital) or when nothing has priced
/// the junior side yet.
pub fn junior_capital(v: &TradingVault) -> Option<u64> {
    if v.structure_code == 0 {
        return None;
    }
    let nav = v.junior_nav.or_else(|| {
        v.latest_junior_pps_e12.map(|pps| {
            pps.saturating_mul(v.junior_shares) / 1_000_000_000_000u128 / SHARE_OFFSET
        })
    });
    nav.map(|n| u64::try_from(n).unwrap_or(u64::MAX))
}

/// Dev-inspect `vault::commitment_of(vault, cap_id).0` — does the
/// escrowed curator commitment slot exist for this cap (SO-418)? The
/// funding pass funds it when missing.
pub async fn has_commitment(
    client: &ChainClient,
    sender: SuiAddress,
    trading_vault_package: ObjectID,
    vault_id: ObjectID,
    cap_id: ObjectID,
) -> Result<bool> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, vault_id, false).await?)?;
    let cap = pt.pure(cap_id)?;
    pt.programmable_move_call(
        trading_vault_package,
        Identifier::new("vault").unwrap(),
        Identifier::new("commitment_of").unwrap(),
        vec![],
        vec![vault, cap],
    );
    let res =
        client.dev_inspect_ptb(sender, pt).await.context("dev-inspecting commitment_of")?;
    decode_return_value::<bool>(&res, 0).context("decoding commitment_of.exists")
}

/// Address-owner field of a BalanceManager's JSON, with a type check.
pub async fn manager_owner(client: &ChainClient, id: ObjectID) -> Result<String> {
    let (obj, json) = client
        .get_object_json(id)
        .await
        .with_context(|| format!("reading BalanceManager {id}"))?;
    let type_ok = obj
        .type_()
        .map(|t| t.to_string().ends_with("::balance_manager::BalanceManager"))
        .unwrap_or(false);
    if !type_ok {
        bail!("{id} is not a BalanceManager");
    }
    json.as_ref()
        .and_then(|j| j.get("owner"))
        .and_then(|o| o.as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("BalanceManager {id} JSON missing owner"))
}

/// Address-owner of an object, or `None` when it is shared/immutable or
/// owned by an object.
async fn owner_of(client: &ChainClient, id: ObjectID) -> Result<Option<SuiAddress>> {
    use sui_types::object::Owner;
    let obj = client.get_object(id).await?;
    Ok(match obj.owner() {
        Owner::AddressOwner(a) => Some(*a),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::ids::{ObjectId, SuiAddress as PtAddress};

    fn addr(b: u8) -> PtAddress {
        let mut bytes = [0u8; 32];
        bytes[31] = b;
        PtAddress::new(bytes)
    }

    fn oid(b: u8) -> ObjectId {
        let mut bytes = [0u8; 32];
        bytes[31] = b;
        ObjectId::new(bytes)
    }

    fn vault(id: u8, shares: u128) -> TradingVault {
        TradingVault {
            vault_id: oid(id),
            accounting_asset: protocol_types::asset::AssetType::new("0x2::tusdc::TUSDC"),
            creator: addr(1),
            curator: addr(1),
            curator_cap_id: oid(100 + id),
            state: "open".to_owned(),
            lockup_ms: 0,
            curator_fee_bps: 0,
            unwind_grace_ms: 0,
            deposits_paused: false,
            mm_release_enabled: false,
            total_shares: shares,
            position_count: 0,
            pending_withdrawals: 0,
            latest_pps_e12: None,
            updated_at_ms: 0,
            external_account: None,
            external_exposure: 0,
            latest_external_equity: None,
            external_equity_updated_at_ms: None,
            latest_nav: None,
            nav_updated_at_ms: None,
            // ── v2 (SO-418): an untranched, healthy, unsettled vault ──
            structure_code: 0,
            senior_hurdle_bps_annual: 0,
            target_junior_bps: 0,
            maintenance_junior_bps: 0,
            upside_code: 0,
            residual_participation_bps: 0,
            total_return_cap_bps: 0,
            terms_version: 1,
            spec_hash: None,
            senior_shares: 0,
            junior_shares: 0,
            senior_claim: 0,
            senior_principal_basis: 0,
            senior_nav: None,
            junior_nav: None,
            latest_senior_pps_e12: None,
            latest_junior_pps_e12: None,
            risk_state: 0,
            curator_commitment_breached: false,
            impaired_since_ms: None,
            active_junior_generation: 0,
            reset_old_generation: None,
            reset_proposed_at_ms: None,
            reset_executable_at_ms: None,
            reset_recorded_nav: None,
            reset_recorded_senior_claim: None,
            reset_recorded_required_deposit: None,
            settled: false,
            settlement_final_nav: None,
            senior_pool: None,
            senior_supply: None,
            junior_pool: None,
            junior_supply: None,
            settlement_snapshot_at_ms: None,
            settlement_redeemed: 0,
            senior_lane_head: 0,
            senior_lane_tail: 0,
            junior_lane_head: 0,
            junior_lane_tail: 0,
        }
    }

    /// A wired vault beats any unwired stake — the same wallet also runs
    /// smoke tooling whose husks can out-stake a fresh working vault.
    #[test]
    fn pick_prefers_wired_over_stake() {
        let wired: HashSet<ObjectId> = [oid(2)].into_iter().collect();
        let chosen = pick(vec![vault(1, 1_000_000_000_000), vault(2, 1)], &wired);
        assert_eq!(chosen.unwrap().vault_id, oid(2));
    }

    /// Among equally-wired vaults: largest stake, then lowest id — same
    /// determinism argument as mm-bot's desk (restart re-adopts the same
    /// vault instead of ping-ponging).
    #[test]
    fn pick_falls_back_to_stake_then_id() {
        let none = HashSet::new();
        let chosen = pick(vec![vault(1, 5), vault(3, 100), vault(2, 5)], &none);
        assert_eq!(chosen.unwrap().vault_id, oid(3));
        let tied = pick(vec![vault(3, 5), vault(1, 5), vault(2, 5)], &none);
        assert_eq!(tied.unwrap().vault_id, oid(1));
    }

    #[test]
    fn pick_of_nothing_is_none() {
        assert!(pick(Vec::new(), &HashSet::new()).is_none());
    }

    // ── SO-418 risk gate + tranche budget measure ──────────────────────

    #[test]
    fn risk_off_covers_state_breach_lifecycle_and_settlement() {
        assert!(!risk_off(&vault(1, 0)));
        let mut v = vault(1, 0);
        v.risk_state = 2; // Impaired
        assert!(risk_off(&v));
        let mut v = vault(1, 0);
        v.curator_commitment_breached = true;
        assert!(risk_off(&v));
        let mut v = vault(1, 0);
        v.state = "closing".into();
        assert!(risk_off(&v));
        let mut v = vault(1, 0);
        v.settled = true;
        assert!(risk_off(&v));
    }

    #[test]
    fn junior_capital_is_none_untranched_and_prefers_synced_nav() {
        let mut v = vault(1, 0);
        assert_eq!(junior_capital(&v), None, "untranched: whole book is risk capital");
        v.structure_code = 1;
        assert_eq!(junior_capital(&v), None, "no junior pricing yet");
        // Observed pps fallback: pps_e12 = value×1e12×OFFSET/shares —
        // a par vault reads 1e12.
        v.junior_shares = 3_000 * 1_000_000;
        v.latest_junior_pps_e12 = Some(1_000_000_000_000);
        assert_eq!(junior_capital(&v), Some(3_000));
        // The synced waterfall NAV wins once present.
        v.junior_nav = Some(2_500);
        assert_eq!(junior_capital(&v), Some(2_500));
    }

    #[test]
    fn missing_deposit_assets_skips_accounting_and_present() {
        let configured = vec![
            "0x2::tusdc::TUSDC".to_owned(),
            "0x3::tbtc::TBTC".to_owned(),
            "0x4::tsui::TSUI".to_owned(),
        ];
        // Present arrives in the chain's 0x-less TypeName form — the
        // canonical compare is what makes this match (project rule).
        let present = vec![
            "0000000000000000000000000000000000000000000000000000000000000003::tbtc::TBTC"
                .to_owned(),
        ];
        let missing = missing_deposit_assets(&configured, &present, "0x2::tusdc::TUSDC");
        assert_eq!(missing, vec![canonicalize_move_type("0x4::tsui::TSUI")]);
    }

    #[test]
    fn unique_accounting_requires_one_quote_asset() {
        let one = unique_accounting(&[
            "0x2::tusdc::TUSDC".to_owned(),
            "0000000000000000000000000000000000000000000000000000000000000002::tusdc::TUSDC"
                .to_owned(),
        ])
        .unwrap();
        assert_eq!(one, canonicalize_move_type("0x2::tusdc::TUSDC"));
        assert!(unique_accounting(&[]).is_err());
        assert!(unique_accounting(&[
            "0x2::tusdc::TUSDC".to_owned(),
            "0x3::tbtc::TBTC".to_owned(),
        ])
        .is_err());
    }
}
