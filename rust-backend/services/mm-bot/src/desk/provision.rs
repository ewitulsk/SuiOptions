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
//! `trading_vault::create_vault` is permissionless and takes `curator` as
//! a plain argument, so anyone can mint a cap naming this wallet curator
//! and transfer it here for free. "Do I hold a cap?" is therefore not a
//! safe adoption test — a hostile vault could hand the desk a book with a
//! junk deposit asset, a punitive curator fee, or depositors who are the
//! attacker.
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
    /// 0 = creator rotates the curator, 1 = curator, 2 = either. The bot
    /// is both, so this only matters after a hand-off.
    pub rotation_authority: u8,
    pub max_positions: u64,
    pub unwind_grace_ms: u64,
    pub gas_budget: u64,
}

impl Default for ProvisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            lockup_ms: 0,
            curator_fee_bps: 0,
            rotation_authority: 2,
            max_positions: 64,
            unwind_grace_ms: 24 * 60 * 60 * 1_000,
            gas_budget: 200_000_000,
        }
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
        return Ok(ResolvedVault { vault_id, curator_cap: cap, provisioned: false });
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
        return Ok(ResolvedVault { vault_id, curator_cap: cap, provisioned: false });
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

    provision(&p, me).await
}

/// Create, enable the release gate, and (testnet) seed a fresh vault.
async fn provision(p: &ResolveParams<'_>, me: SuiAddress) -> Result<ResolvedVault> {
    let created = trading_vault::create_vault(
        &p.wrap.client,
        &p.wrap.signer,
        p.trading_vault_package,
        p.vault_protocol_config,
        p.settlement_coin_type,
        &CreateVaultSpec {
            curator: me,
            lockup_ms: p.cfg.lockup_ms,
            curator_fee_bps: p.cfg.curator_fee_bps,
            rotation_authority: p.cfg.rotation_authority,
            max_positions: p.cfg.max_positions,
            unwind_grace_ms: p.cfg.unwind_grace_ms,
        },
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

    // Seed. An unseeded vault is NAV 0, and NAV 0 declines every RFQ, so
    // on testnet provisioning is not finished until this lands.
    if let Some(seed) = p.testnet_seed {
        let refs = TradingVaultRefs {
            package: p.trading_vault_package,
            vault_id: created.vault_id,
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
            seed.amount,
            p.cfg.gas_budget,
        )
        .await
        .context("minting and depositing the testnet vault seed")?;
        info!(
            vault = %created.vault_id.to_hex_literal(),
            amount = seed.amount,
            digest = %sui_tx::tx::tx_digest(&resp),
            "seeded the new vault from the testnet faucet"
        );
    } else {
        warn!(
            vault = %created.vault_id.to_hex_literal(),
            "vault provisioned unseeded — NAV is 0 and every RFQ will decline until it is funded"
        );
    }

    Ok(ResolvedVault {
        vault_id: created.vault_id,
        curator_cap: Some(created.curator_cap_id),
        provisioned: true,
    })
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
    let got = canonicalize_move_type(&view.deposit_asset.to_string());
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
/// restart lands on the same one. Lowest id wins; the rest are logged.
fn pick(mut vaults: Vec<TradingVault>) -> Option<TradingVault> {
    vaults.sort_by_key(|v| v.vault_id.to_hex());
    if vaults.len() > 1 {
        let others: Vec<String> =
            vaults[1..].iter().map(|v| format!("0x{}", v.vault_id.to_hex())).collect();
        warn!(
            adopted = %format!("0x{}", vaults[0].vault_id.to_hex()),
            ignored = ?others,
            "several self-created trading vaults — adopting the lowest id"
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

    fn vault(id: u8, creator: u8, state: &str) -> TradingVault {
        TradingVault {
            vault_id: oid(id),
            deposit_asset: protocol_types::asset::AssetType::new("0x2::tusdc::TUSDC"),
            creator: addr(creator),
            curator: addr(9),
            curator_cap_id: oid(100 + id),
            state: state.to_owned(),
            lockup_ms: 0,
            curator_fee_bps: 0,
            rotation_authority: 2,
            max_positions: 64,
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

    /// Deterministic so a restart re-adopts the same vault instead of
    /// ping-ponging (or provisioning another).
    #[test]
    fn pick_is_lowest_id_and_stable() {
        let chosen = pick(vec![vault(3, 1, "open"), vault(1, 1, "open"), vault(2, 1, "open")]);
        assert_eq!(chosen.unwrap().vault_id, oid(1));
        let reordered = pick(vec![vault(2, 1, "open"), vault(3, 1, "open"), vault(1, 1, "open")]);
        assert_eq!(reordered.unwrap().vault_id, oid(1));
    }

    #[test]
    fn pick_of_nothing_is_none() {
        assert!(pick(Vec::new()).is_none());
    }
}
