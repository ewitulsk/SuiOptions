//! Resolving the trading vault the desk curates — adopt, or provision one
//! (SO-345).
//!
//! The desk trades only as a vault curator, so a usable vault is a hard
//! precondition: without one the book reconstructs at NAV 0 and
//! `limits::evaluate` hard-declines every RFQ on the premium budget. That
//! is what a stale `[desk].vault_id` produced after a contract redeploy —
//! two warnings and then a bot that quoted nothing for a day.
//!
//! ## Why holding a CuratorCap proves nothing
//!
//! `trading_vault::create_vault` is permissionless and the `CuratorCap`
//! is freely transferable, so anyone can create a vault and send its cap
//! here for free. "Do I hold a cap?" is therefore not a safe adoption
//! test — a hostile vault could hand the desk a book with a junk deposit
//! asset, a punitive curator fee, or depositors who are the attacker.
//!
//! `creator` is `ctx.sender()` at creation, immutable, and indexed. So:
//!
//! - **Pinned** (`[desk].vault_id` set): adopt that vault whatever its
//!   provenance. The operator asserted intent, which is the escape hatch
//!   for a vault provisioned by a human and handed over.
//! - **Auto** (`vault_id` empty + `[desk.provision].enabled`): only ever
//!   adopt a vault this wallet created. Anything else gets ignored and a
//!   fresh vault is provisioned.
//!
//! Either way the vault is then verified against chain state, not against
//! the config's claims about it.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sui_tx::chain::ChainClient;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::trading_vault::{self, CreateVaultSpec, TradingVaultRefs};
use sui_types::base_types::{ObjectID, SuiAddress};
use tracing::{error, info, warn};

use indexer_graphql::{IndexerClient, TradingVault};
use protocol_types::asset::canonicalize_move_type;

/// `[desk.provision]` — self-provisioning for a desk with no vault.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProvisionConfig {
    /// Create a vault when discovery finds none this wallet created.
    /// Default false: on prod a vault custodies real depositor funds and
    /// should be provisioned deliberately, then pinned.
    pub enabled: bool,
    /// Depositor lockup. 0 = withdraw any time (subject to the queue).
    pub lockup_ms: u64,
    pub curator_fee_bps: u64,
    pub unwind_grace_ms: u64,
    pub gas_budget: u64,
    // ── v2 capital structure (SO-418). Defaults = UNTRANCHED: the desk
    // vault stays untranched unless the operator opts into tranching —
    // structure_code 1 requires ALL six tranche params set coherently or
    // `create_vault` aborts on chain.
    /// 0 = untranched (default), 1 = senior/junior.
    pub structure_code: u8,
    pub senior_hurdle_bps_annual: u64,
    pub target_junior_bps: u64,
    pub maintenance_junior_bps: u64,
    pub upside_code: u8,
    pub residual_participation_bps: u64,
    pub total_return_cap_bps: u64,
    /// Terms-document version recorded immutably on the vault (§9.2).
    pub terms_version: u64,
    /// Hex content hash of the terms document `terms_version` names.
    /// Empty = no hash recorded.
    pub spec_hash: String,
    /// Curator escrowed-commitment funding (§8.6), accounting-asset raw
    /// units, deposited via `deposit_into_commitment` when the vault is
    /// provisioned (and topped in on adoption when the commitment slot is
    /// missing). 0 disables — but an unfunded commitment trips the
    /// `curator_commitment_breached` gate once the protocol floor is
    /// nonzero, which parks the desk risk-off. Default $100k at 6
    /// decimals.
    pub commitment_deposit: u64,
}

impl Default for ProvisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lockup_ms: 0,
            curator_fee_bps: 0,
            unwind_grace_ms: 24 * 60 * 60 * 1_000,
            gas_budget: 200_000_000,
            structure_code: 0,
            senior_hurdle_bps_annual: 0,
            target_junior_bps: 0,
            maintenance_junior_bps: 0,
            upside_code: 0,
            residual_participation_bps: 0,
            total_return_cap_bps: 0,
            terms_version: 1,
            spec_hash: String::new(),
            commitment_deposit: 100_000_000_000,
        }
    }
}

impl ProvisionConfig {
    /// The v2 `CreateVaultSpec` this config describes.
    fn vault_spec(&self) -> Result<CreateVaultSpec> {
        let spec_hash = if self.spec_hash.is_empty() {
            Vec::new()
        } else {
            hex::decode(self.spec_hash.trim_start_matches("0x"))
                .map_err(|e| anyhow!("bad [desk.provision].spec_hash: {e}"))?
        };
        Ok(CreateVaultSpec {
            lockup_ms: self.lockup_ms,
            curator_fee_bps: self.curator_fee_bps,
            unwind_grace_ms: self.unwind_grace_ms,
            structure_code: self.structure_code,
            senior_hurdle_bps_annual: self.senior_hurdle_bps_annual,
            target_junior_bps: self.target_junior_bps,
            maintenance_junior_bps: self.maintenance_junior_bps,
            upside_code: self.upside_code,
            residual_participation_bps: self.residual_participation_bps,
            total_return_cap_bps: self.total_return_cap_bps,
            terms_version: self.terms_version,
            spec_hash,
        })
    }
}

/// Testnet faucet seed for a freshly-created vault (`[testnet]`).
#[derive(Debug, Clone)]
pub struct TestnetSeed {
    pub tokens_package: ObjectID,
    pub module: String,
    pub faucet_id: ObjectID,
    pub amount: u64,
}

pub struct ResolveParams<'a> {
    pub wrap: &'a SuiClientWrapper,
    pub indexer: &'a IndexerClient,
    pub cfg: &'a ProvisionConfig,
    /// `[desk].vault_id`, trimmed. Empty ⇒ auto path.
    pub pinned_vault_id: &'a str,
    /// `[desk].mm_release_enabled` — operator consent to flip the release
    /// gate on when the chain says it is off and we hold the cap.
    pub allow_mm_release_toggle: bool,
    pub trading_vault_package: ObjectID,
    pub vault_protocol_config: ObjectID,
    /// Shared `whitelist::Whitelist` — the ingress gate on `create_vault`
    /// and `vault::deposit` (SO-383).
    pub whitelist: ObjectID,
    /// The desk's settlement coin type — a vault denominated in anything
    /// else would put the book, NAV and every limit against the wrong asset.
    pub settlement_coin_type: &'a str,
    /// `Some` only on testnet with `mint_and_deposit_liquidity = true`.
    pub testnet_seed: Option<&'a TestnetSeed>,
}

pub struct ResolvedVault {
    pub vault_id: ObjectID,
    /// `None` when the cap is not owned by this wallet — vault-funded bids
    /// and vault-custody exits stay disabled, WS quoting still runs.
    pub curator_cap: Option<ObjectID>,
    /// True when this call created the vault (and, on testnet, seeded it).
    pub provisioned: bool,
    /// SO-418: the vault booted risk-off (capital risk state not Healthy,
    /// or curator commitment breached). NOT a boot failure — the desk
    /// adopts it and idles quoting until the state cures, so a vault in
    /// breach never rolls a deploy back (the health gate stays green).
    pub risk_off: bool,
}

/// Resolve the vault the desk will curate, provisioning one if configured.
pub async fn resolve(p: ResolveParams<'_>) -> Result<ResolvedVault> {
    let me = p.wrap.signer.address;

    if !p.pinned_vault_id.is_empty() {
        let vault_id = ObjectID::from_hex_literal(p.pinned_vault_id)
            .map_err(|e| anyhow!("bad [desk].vault_id {}: {e}", p.pinned_vault_id))?;
        let view = fetch(p.indexer, vault_id).await?.ok_or_else(|| {
            anyhow!(
                "[desk].vault_id {} is not a trading vault of this deployment — it is most \
                 likely pinned from a previous contract publish. Re-pin it, or clear it and \
                 set [desk.provision].enabled = true.",
                vault_id.to_hex_literal()
            )
        })?;
        info!(vault = %vault_id.to_hex_literal(), creator = %view.creator, "adopting pinned vault");
        let cap = verify(&p, &view, me).await?;
        // A pin can point at a vault we created and failed to finish, so
        // the resume applies here too — it self-gates on creator == self.
        resume_seed(&p, &view, me).await?;
        let risk_off = adopt_commitment_and_risk(&p, &view, cap).await;
        return Ok(ResolvedVault { vault_id, curator_cap: cap, provisioned: false, risk_off });
    }

    if !p.cfg.enabled {
        return Err(anyhow!(
            "[desk] enabled with no vault_id and [desk.provision].enabled = false — the desk \
             trades only as a vault curator"
        ));
    }

    // Auto path. Only vaults this wallet created are candidates; a cap
    // someone else minted for us is deliberately ignored.
    let mine = self_created(p.indexer, me).await?;
    if let Some(view) = pick(mine) {
        let vault_id = ObjectID::new(*view.vault_id.as_bytes());
        info!(vault = %vault_id.to_hex_literal(), "adopting self-created vault");
        let cap = verify(&p, &view, me).await?;
        resume_seed(&p, &view, me).await?;
        let risk_off = adopt_commitment_and_risk(&p, &view, cap).await;
        return Ok(ResolvedVault { vault_id, curator_cap: cap, provisioned: false, risk_off });
    }

    // Nothing to adopt. Before creating, insist the indexer is current:
    // a lagging view reads exactly like "no vault exists" and would make
    // us create a duplicate on every boot.
    let progress = p
        .indexer
        .progress()
        .await
        .context("reading indexer progress before provisioning a vault")?;
    if !progress.caught_up {
        return Err(anyhow!(
            "refusing to provision a vault while the indexer is behind the chain tip \
             (checkpoint {} of {:?}) — a lagging view is indistinguishable from 'no vault \
             exists' and would create a duplicate",
            progress.current_checkpoint,
            progress.tip_checkpoint
        ));
    }

    provision(&p).await
}

/// Create, enable the release gate, fund the curator commitment, and
/// (testnet) seed a fresh vault. SO-418: provisioning is not finished —
/// and the boot (hence the deploy health gate) does not pass — until the
/// escrowed curator commitment is funded and the vault reads
/// `is_risk_off == false` on chain.
async fn provision(p: &ResolveParams<'_>) -> Result<ResolvedVault> {
    let created = trading_vault::create_vault(
        &p.wrap.client,
        &p.wrap.signer,
        p.trading_vault_package,
        p.vault_protocol_config,
        p.whitelist,
        p.settlement_coin_type,
        &p.cfg.vault_spec()?,
        p.cfg.gas_budget,
    )
    .await
    .context("creating the desk's trading vault")?;
    info!(
        vault = %created.vault_id.to_hex_literal(),
        curator_cap = %created.curator_cap_id.to_hex_literal(),
        digest = %created.digest,
        "provisioned a trading vault"
    );

    // Created vaults land with the release gate off; we hold the cap, so
    // there is no human step here.
    trading_vault::set_mm_release_enabled(
        &p.wrap.client,
        &p.wrap.signer,
        p.trading_vault_package,
        created.vault_id,
        created.curator_cap_id,
        true,
        p.cfg.gas_budget,
    )
    .await
    .context("enabling the vault_mm release gate on the new vault")?;
    info!(vault = %created.vault_id.to_hex_literal(), "vault_mm release enabled");

    // Fund the escrowed curator commitment (§8.6) BEFORE the seed: the
    // vault holds nothing yet, so `begin_appraisal` is complete on the
    // spot and the whole funding fits one PTB. A hard error here fails
    // the boot on purpose — a curator vault without its commitment parks
    // risk-off at the first crank.
    if p.cfg.commitment_deposit > 0 {
        fund_commitment(p, created.vault_id, created.curator_cap_id)
            .await
            .context("funding the curator commitment on the new vault")?;
    } else {
        warn!(
            vault = %created.vault_id.to_hex_literal(),
            "[desk.provision].commitment_deposit = 0 — curator commitment unfunded; the vault \
             goes risk-off once the protocol commitment floor is nonzero"
        );
    }

    // Seed. An unseeded vault is NAV 0, and NAV 0 declines every RFQ, so
    // on testnet provisioning is not finished until this lands.
    if !seed(p, created.vault_id).await? {
        warn!(
            vault = %created.vault_id.to_hex_literal(),
            "vault provisioned unseeded — NAV is 0 and every RFQ will decline until it is funded"
        );
    }

    // The provision-path risk gate: a fresh, funded vault must read
    // healthy on chain before the desk reports itself bootable.
    let risk_off = is_risk_off(&p.wrap.client, p.wrap.signer.address, p.trading_vault_package, created.vault_id)
        .await
        .unwrap_or(false);
    if risk_off {
        return Err(anyhow!(
            "freshly provisioned vault {} reads is_risk_off on chain — provisioning is \
             incomplete (commitment underfunded vs the protocol floor?)",
            created.vault_id.to_hex_literal()
        ));
    }

    Ok(ResolvedVault {
        vault_id: created.vault_id,
        curator_cap: Some(created.curator_cap_id),
        provisioned: true,
        risk_off: false,
    })
}

/// Adoption-path commitment + risk check (SO-418): fund the escrowed
/// curator commitment when the slot is missing (cap held, config
/// nonzero), then report whether the vault is risk-off. NEVER fails the
/// boot — a vault in breach adopts as healthy-but-idle so a deploy is
/// not rolled back by market state (the known health-gate trap).
async fn adopt_commitment_and_risk(
    p: &ResolveParams<'_>,
    view: &TradingVault,
    cap: Option<ObjectID>,
) -> bool {
    let vault_id = ObjectID::new(*view.vault_id.as_bytes());
    let cap_id = ObjectID::new(*view.curator_cap_id.as_bytes());

    // Commitment presence from chain (the indexer has no commitment
    // column; `commitment_of` is a cheap dev-inspect).
    match commitment_of(&p.wrap.client, p.wrap.signer.address, p.trading_vault_package, vault_id, cap_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => match (cap, p.cfg.commitment_deposit) {
            (Some(cap_id), amount) if amount > 0 => {
                // Best-effort: works whenever the vault's appraisal is
                // trivial (accounting asset only, no positions). A vault
                // holding appraised assets needs the full composer —
                // deliberately out of scope here; the failure alerts and
                // the vault runs (possibly risk-off, hence idle).
                if let Err(e) = fund_commitment(p, vault_id, cap_id).await {
                    error!(
                        alert_id = "tx-failed-mm-bot-desk",
                        vault = %vault_id.to_hex_literal(),
                        error = %format!("{e:#}"),
                        "adopted vault has no curator commitment and funding it failed — the \
                         vault will park risk-off once the protocol floor binds"
                    );
                }
            }
            _ => warn!(
                vault = %vault_id.to_hex_literal(),
                cap_held = cap.is_some(),
                commitment_deposit = p.cfg.commitment_deposit,
                "adopted vault has no curator commitment and it cannot be funded from here"
            ),
        },
        Err(e) => warn!(
            vault = %vault_id.to_hex_literal(),
            error = %format!("{e:#}"),
            "commitment presence read failed; continuing"
        ),
    }

    let risk_off = view.risk_state != 0 || view.curator_commitment_breached;
    if risk_off {
        warn!(
            vault = %vault_id.to_hex_literal(),
            risk_state = view.risk_state,
            commitment_breached = view.curator_commitment_breached,
            "adopting a RISK-OFF vault — desk boots healthy-but-idle (no quotes, no bids, no \
             new listings) until the state cures"
        );
    }
    risk_off
}

/// Mint (testnet faucet) or gather (wallet coins) `commitment_deposit`
/// of the accounting asset and `deposit_into_commitment` it, one PTB.
/// The appraisal is the bare `begin_appraisal` — complete only while the
/// vault holds nothing but the accounting asset (which is the case at
/// provision time, the only path that hard-requires this).
async fn fund_commitment(
    p: &ResolveParams<'_>,
    vault_id: ObjectID,
    curator_cap: ObjectID,
) -> Result<()> {
    let amount = p.cfg.commitment_deposit;
    let refs = TradingVaultRefs {
        package: p.trading_vault_package,
        vault_id,
        protocol_config_id: p.vault_protocol_config,
        deposit_type: p.settlement_coin_type,
    };
    let mut pt = sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder::new();
    let funds = match p.testnet_seed {
        Some(seed) => {
            // Testnet: mint the commitment from the faucet, same PTB.
            let faucet =
                pt.obj(sui_tx::tx::shared_object_arg(&p.wrap.client, seed.faucet_id, true).await?)?;
            let amount_arg = pt.pure(&amount)?;
            pt.programmable_move_call(
                seed.tokens_package,
                move_core_types::identifier::Identifier::new(seed.module.as_str())
                    .map_err(|e| anyhow!("module name {}: {e}", seed.module))?,
                move_core_types::identifier::Identifier::new("mint").unwrap(),
                vec![],
                vec![faucet, amount_arg],
            )
        }
        None => {
            // No faucet (prod): pay from the wallet's own coins.
            sui_tx::tx::deepbook::gather_exact_coin(
                &p.wrap.client,
                &p.wrap.signer,
                &mut pt,
                p.settlement_coin_type,
                amount,
            )
            .await
            .context("gathering the commitment deposit from wallet coins")?
        }
    };
    let appraisal =
        trading_vault::build_begin_appraisal(&p.wrap.client, &mut pt, &refs).await?;
    trading_vault::build_deposit_into_commitment(
        &p.wrap.client,
        &mut pt,
        &refs,
        p.whitelist,
        curator_cap,
        appraisal,
        funds,
    )
    .await?;
    let resp = sui_tx::tx::submit_ptb(
        &p.wrap.client,
        &p.wrap.signer,
        pt,
        p.cfg.gas_budget,
        "desk commitment funding",
    )
    .await?;
    info!(
        vault = %vault_id.to_hex_literal(),
        amount,
        digest = %sui_tx::tx::tx_digest(&resp),
        "curator commitment funded (deposit_into_commitment)"
    );
    Ok(())
}

/// Dev-inspect `vault::commitment_of(vault, cap_id).0` — does the
/// escrowed commitment slot exist for this cap?
async fn commitment_of(
    client: &ChainClient,
    sender: SuiAddress,
    package: ObjectID,
    vault_id: ObjectID,
    cap_id: ObjectID,
) -> Result<bool> {
    let mut pt = sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder::new();
    let vault = pt.obj(sui_tx::tx::shared_object_arg(client, vault_id, false).await?)?;
    let cap = pt.pure(&cap_id)?;
    pt.programmable_move_call(
        package,
        move_core_types::identifier::Identifier::new("vault").unwrap(),
        move_core_types::identifier::Identifier::new("commitment_of").unwrap(),
        vec![],
        vec![vault, cap],
    );
    let res = client
        .dev_inspect_ptb(sender, pt)
        .await
        .context("dev-inspecting commitment_of")?;
    sui_tx::chain::decode_return_value::<bool>(&res, 0).context("decoding commitment_of.exists")
}

/// Dev-inspect `vault::is_risk_off(vault)` — the §8.4b gate the quote
/// sessions and vault_mm releases abort on (code 124).
async fn is_risk_off(
    client: &ChainClient,
    sender: SuiAddress,
    package: ObjectID,
    vault_id: ObjectID,
) -> Result<bool> {
    let mut pt = sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder::new();
    let vault = pt.obj(sui_tx::tx::shared_object_arg(client, vault_id, false).await?)?;
    pt.programmable_move_call(
        package,
        move_core_types::identifier::Identifier::new("vault").unwrap(),
        move_core_types::identifier::Identifier::new("is_risk_off").unwrap(),
        vec![],
        vec![vault],
    );
    let res = client.dev_inspect_ptb(sender, pt).await.context("dev-inspecting is_risk_off")?;
    sui_tx::chain::decode_return_value::<bool>(&res, 0).context("decoding is_risk_off")
}

/// Mint and deposit the `[testnet]` seed. `Ok(false)` when no seed is
/// configured — the caller decides whether that is worth a warning.
async fn seed(p: &ResolveParams<'_>, vault_id: ObjectID) -> Result<bool> {
    let Some(seed) = p.testnet_seed else {
        return Ok(false);
    };
    let refs = TradingVaultRefs {
        package: p.trading_vault_package,
        vault_id,
        protocol_config_id: p.vault_protocol_config,
        deposit_type: p.settlement_coin_type,
    };
    let resp = sui_tx::tx::test_tokens::mint_and_deposit_into_vault(
        &p.wrap.client,
        &p.wrap.signer,
        seed.tokens_package,
        &seed.module,
        seed.faucet_id,
        &refs,
        p.whitelist,
        seed.amount,
        p.cfg.gas_budget,
    )
    .await
    .context("minting and depositing the testnet vault seed")?;
    info!(
        vault = %vault_id.to_hex_literal(),
        amount = seed.amount,
        digest = %sui_tx::tx::tx_digest(&resp),
        "seeded the vault from the testnet faucet"
    );
    Ok(true)
}

/// Finish a provision that died between `create_vault` and its seed.
///
/// Creating, enabling the release gate and seeding are three transactions,
/// so a crash in the middle leaves a real vault holding nothing — and the
/// adopt path would otherwise take it and never fund it, which is exactly
/// what happened on the first staging rollout.
///
/// Narrow on purpose. Only a vault THIS WALLET created, holding NO shares,
/// with a `[testnet]` seed configured. A vault someone else made is never
/// funded from our faucet, and one that already has depositors is never
/// topped up — this finishes an interrupted setup, it is not a balance
/// top-up loop.
async fn resume_seed(p: &ResolveParams<'_>, view: &TradingVault, me: SuiAddress) -> Result<()> {
    let me = protocol_types::ids::SuiAddress::new(me.to_inner());
    if !should_resume_seed(view, me, p.testnet_seed.is_some()) {
        return Ok(());
    }
    let vault_id = ObjectID::new(*view.vault_id.as_bytes());
    info!(
        vault = %vault_id.to_hex_literal(),
        "adopted vault holds no shares — finishing its interrupted provision"
    );
    seed(p, vault_id).await?;
    Ok(())
}

/// Check a candidate against chain state and return the CuratorCap if this
/// wallet owns it. Errors mean "unusable", not "degraded".
async fn verify(
    p: &ResolveParams<'_>,
    view: &TradingVault,
    me: SuiAddress,
) -> Result<Option<ObjectID>> {
    let vault_id = ObjectID::new(*view.vault_id.as_bytes());

    if view.state != "open" {
        return Err(anyhow!("vault {} is {}, not open", vault_id.to_hex_literal(), view.state));
    }
    if view.deposits_paused {
        return Err(anyhow!("vault {} has deposits paused", vault_id.to_hex_literal()));
    }
    let want = canonicalize_move_type(p.settlement_coin_type);
    let got = canonicalize_move_type(&view.accounting_asset.to_string());
    if want != got {
        return Err(anyhow!(
            "vault {} is denominated in {got}, but the desk settles in {want}",
            vault_id.to_hex_literal()
        ));
    }

    // Ownership comes from a chain read, never the indexer's `curator`.
    // A plain `public_transfer` of a CuratorCap emits no event, so the
    // indexed curator can name a wallet that no longer holds the cap (and
    // miss one that does).
    let cap_id = ObjectID::new(*view.curator_cap_id.as_bytes());
    let cap = match owner_of(&p.wrap.client, cap_id).await {
        Ok(owner) => (owner == Some(me)).then_some(cap_id),
        Err(e) => {
            warn!(error = %format!("{e:#}"), cap = %cap_id.to_hex_literal(), "reading CuratorCap owner failed");
            None
        }
    };
    if cap.is_none() {
        warn!(
            vault = %vault_id.to_hex_literal(),
            cap = %cap_id.to_hex_literal(),
            indexed_curator = %view.curator,
            "CuratorCap is not held by this wallet — vault-funded bids and vault-custody exits disabled"
        );
    }

    // The release gate is read from chain, not taken on the config's word.
    if !view.mm_release_enabled {
        match (cap, p.allow_mm_release_toggle) {
            (Some(cap_id), true) => {
                trading_vault::set_mm_release_enabled(
                    &p.wrap.client,
                    &p.wrap.signer,
                    p.trading_vault_package,
                    vault_id,
                    cap_id,
                    true,
                    p.cfg.gas_budget,
                )
                .await
                .context("enabling the vault_mm release gate")?;
                info!(vault = %vault_id.to_hex_literal(), "vault_mm release enabled (was off on chain)");
            }
            (Some(_), false) => {
                return Err(anyhow!(
                    "vault {} has vault_mm release off on chain and [desk].mm_release_enabled \
                     = false withholds consent to flip it — quotes would revert on release",
                    vault_id.to_hex_literal()
                ))
            }
            (None, _) => {
                return Err(anyhow!(
                    "vault {} has vault_mm release off on chain and this wallet does not hold \
                     its CuratorCap — quotes would revert on release",
                    vault_id.to_hex_literal()
                ))
            }
        }
    }

    Ok(cap)
}

/// The gate on [`resume_seed`], split out so it is directly testable.
fn should_resume_seed(
    view: &TradingVault,
    me: protocol_types::ids::SuiAddress,
    has_seed: bool,
) -> bool {
    has_seed && view.creator == me && view.total_shares == 0
}

/// Every open vault this wallet created. `creator` is the tx sender at
/// creation and cannot be spoofed, which is the whole basis for trusting
/// auto-discovery.
async fn self_created(indexer: &IndexerClient, me: SuiAddress) -> Result<Vec<TradingVault>> {
    let me = protocol_types::ids::SuiAddress::new(me.to_inner());
    Ok(indexer
        .trading_vaults()
        .await
        .context("listing trading vaults")?
        .into_iter()
        .filter(|v| v.creator == me && v.state == "open")
        .collect())
}

/// Deterministic choice when several self-created vaults exist, so a
/// restart lands on the same one. LARGEST stake wins — on a shared
/// wallet the desk's working vault competes with e2e-smoke husks whose
/// creator is the same address, and adopting a husk by id order latched
/// the NAV kill switch live (2026-08-04: $1M desk vault lost to a
/// 1-TUSDC smoke vault with a lower id). Lowest id is the tie-break.
fn pick(mut vaults: Vec<TradingVault>) -> Option<TradingVault> {
    vaults.sort_by(|a, b| {
        b.total_shares
            .cmp(&a.total_shares)
            .then_with(|| a.vault_id.to_hex().cmp(&b.vault_id.to_hex()))
    });
    if vaults.len() > 1 {
        let others: Vec<String> =
            vaults[1..].iter().map(|v| format!("0x{}", v.vault_id.to_hex())).collect();
        warn!(
            adopted = %format!("0x{}", vaults[0].vault_id.to_hex()),
            ignored = ?others,
            "several self-created trading vaults — adopting the largest stake"
        );
    }
    vaults.into_iter().next()
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

/// Log the resolution failure the way an operator will find it. A desk
/// without a usable vault declines 100% of flow, so this is an alert, not
/// a warning to scroll past.
pub fn report_unusable(err: &anyhow::Error) {
    error!(
        alert_id = "desk-vault-unusable",
        error = %format!("{err:#}"),
        "desk cannot resolve a usable trading vault — it would decline every RFQ"
    );
}

/// Shared test fixture (SO-418): a healthy, untranched v2 vault view.
/// Other desk modules' tests reuse it so the 60-field literal lives once.
#[cfg(test)]
pub(crate) fn test_vault_view(id: u8, creator: u8, state: &str) -> TradingVault {
    tests::vault(id, creator, state)
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

    pub(crate) fn vault(id: u8, creator: u8, state: &str) -> TradingVault {
        TradingVault {
            vault_id: oid(id),
            accounting_asset: protocol_types::asset::AssetType::new("0x2::tusdc::TUSDC"),
            creator: addr(creator),
            curator: addr(9),
            curator_cap_id: oid(100 + id),
            state: state.to_owned(),
            lockup_ms: 0,
            curator_fee_bps: 0,
            unwind_grace_ms: 0,
            deposits_paused: false,
            mm_release_enabled: true,
            total_shares: 0,
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

    /// The whole point of the auto path: a vault someone else created and
    /// named us curator of is NOT a candidate, however the cap moved.
    #[test]
    fn auto_discovery_ignores_vaults_created_by_others() {
        let me = addr(1);
        let all = vec![vault(1, 2, "open"), vault(2, 3, "open")];
        let mine: Vec<_> =
            all.into_iter().filter(|v| v.creator == me && v.state == "open").collect();
        assert!(mine.is_empty(), "a cap minted by a stranger must never be adopted");
    }

    #[test]
    fn auto_discovery_keeps_only_open_self_created_vaults() {
        let me = addr(1);
        let all = vec![vault(1, 1, "open"), vault(2, 1, "closed"), vault(3, 2, "open")];
        let mine: Vec<_> =
            all.into_iter().filter(|v| v.creator == me && v.state == "open").collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].vault_id, oid(1));
    }

    fn staked(id: u8, shares: u128) -> TradingVault {
        let mut v = vault(id, 1, "open");
        v.total_shares = shares;
        v
    }

    /// Deterministic so a restart re-adopts the same vault instead of
    /// ping-ponging (or provisioning another). Largest stake wins — the
    /// working vault must beat same-wallet smoke husks — and id order
    /// only breaks ties.
    #[test]
    fn pick_prefers_the_largest_stake_then_lowest_id() {
        // The $1M desk vault (high id) beats 1-token husks (low ids).
        let chosen =
            pick(vec![staked(1, 1_000_000), staked(3, 1_000_000_000_000), staked(2, 500_000)]);
        assert_eq!(chosen.unwrap().vault_id, oid(3));
        // Equal stakes: lowest id, insertion-order independent.
        let tied = pick(vec![vault(3, 1, "open"), vault(1, 1, "open"), vault(2, 1, "open")]);
        assert_eq!(tied.unwrap().vault_id, oid(1));
        let reordered = pick(vec![vault(2, 1, "open"), vault(3, 1, "open"), vault(1, 1, "open")]);
        assert_eq!(reordered.unwrap().vault_id, oid(1));
    }

    #[test]
    fn pick_of_nothing_is_none() {
        assert!(pick(Vec::new()).is_none());
    }

    // ── resume-seed gating ─────────────────────────────────────────────
    // The first staging rollout died between create and seed, and the
    // adopt path then took a vault that held nothing and never funded it.

    #[test]
    fn resumes_an_interrupted_self_created_provision() {
        let me = addr(1);
        assert!(should_resume_seed(&vault(1, 1, "open"), me, true));
    }

    #[test]
    fn never_seeds_a_vault_someone_else_created() {
        let me = addr(1);
        let theirs = vault(1, 2, "open");
        assert!(!should_resume_seed(&theirs, me, true), "our faucet must not fund a stranger's vault");
    }

    #[test]
    fn never_tops_up_a_vault_that_already_has_shares() {
        let me = addr(1);
        let mut funded = vault(1, 1, "open");
        funded.total_shares = 1;
        assert!(!should_resume_seed(&funded, me, true), "this finishes a setup, it is not a top-up loop");
    }

    #[test]
    fn no_seed_configured_means_no_mint() {
        let me = addr(1);
        assert!(!should_resume_seed(&vault(1, 1, "open"), me, false));
    }
}
