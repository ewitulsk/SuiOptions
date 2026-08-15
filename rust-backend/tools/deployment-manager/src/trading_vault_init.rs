//! Post-publish activation for the trading-vault package family (SO-292):
//! resolve the shared governance objects each package's `init` created,
//! then run the activation PTB — allowlist the three integration
//! witnesses + the Pyth oracle witness, and seed the Pyth feed registry
//! from the token catalog. Without this a fresh deployment ships inert
//! (empty registries), which previously required manual PTBs after every
//! redeploy.
//!
//! Pool allowlisting is deliberately NOT done here: pools are created
//! on demand, so whoever creates one allowlists it at the same time.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_tx::chain::ChainClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::ObjectArg;

use protocol_types::OracleProvider;

use crate::json_store::TokenSpec;
use crate::signer::Signer;

/// The shared objects the trading-vault family's inits create, recorded
/// into deployments.json so services stop re-deriving them from publish
/// digests.
#[derive(Debug, Clone)]
pub struct TradingVaultObjects {
    pub vault_protocol_config_id: ObjectID,
    pub integration_registry_id: ObjectID,
    pub oracle_registry_id: ObjectID,
    pub pyth_feed_registry_id: ObjectID,
    /// SO-335. Sibling of `pyth_feed_registry_id`: the two providers'
    /// feed tables are separate objects and are seeded independently, so
    /// a deployment can carry both catalogs and switch between them
    /// without a republish.
    pub switchboard_feed_registry_id: ObjectID,
    pub pool_allowlist_id: ObjectID,
    pub equity_book_id: ObjectID,
    pub vol_book_id: ObjectID,
}

/// Ed25519 public key of the hedge-signer that attests self-serve
/// external-account registrations (SO-308), per deployments.json env slot.
/// Seeding it into the `VaultProtocolConfig` at activation is what enables
/// the attested (curator-signed, capped) registration path; an env with no
/// entry here deploys with the path disabled and stays admin-only.
///
/// prod has no entry on purpose: no hedge-signer is provisioned there (no
/// `options/prod/hedge-signer` secret exists), so the attested-registration
/// path stays disabled on prod until the service is stood up. At that point
/// derive the pubkey from its `[sui]` key the same way staging's was and add
/// it here — never guess a value.
const REGISTRAR_PUBKEYS: &[(&str, &str)] = &[(
    "staging",
    "5c6c64713225f1379004908bbd4372124fd39c71a02d61cd62e614767e497c44",
)];

pub fn registrar_pubkey_for_env(env: &str) -> Option<&'static str> {
    REGISTRAR_PUBKEYS
        .iter()
        .find(|(e, _)| *e == env)
        .map(|(_, key)| *key)
}

/// Addresses seeded into the shared ingress `Whitelist` right after the
/// whitelist package publish, per deployments.json env slot. The deployer
/// is always seeded automatically and does not need an entry. Service
/// wallets that push funds into the protocol — orderbook relayer /
/// staging-mm-bot / mm-bot — should be listed here (or passed via
/// `--ingress-member`) once their addresses are settled.
///
/// prod stays empty on purpose: its service wallets aren't finalized —
/// never guess an address here.
const INGRESS_MEMBERS: &[(&str, &[&str])] = &[];

pub fn ingress_members_for_env(env: &str) -> &'static [&'static str] {
    INGRESS_MEMBERS
        .iter()
        .find(|(e, _)| *e == env)
        .map(|(_, members)| *members)
        .unwrap_or(&[])
}

/// Index one publish outcome's init-created objects by `module::name`.
///
/// Sourced from the publish RESPONSE (`DepPublishOutcome::created_objects`),
/// never a follow-up `GetTransaction`: the load-balanced public gRPC
/// endpoint serves tx lookups from nodes that can lag the executing node by
/// 30s+ (two consecutive redeploys died on exactly that, 2026-08-10), while
/// the response is authoritative and already in hand.
fn index_created(objs: &[(String, String, ObjectID)]) -> BTreeMap<String, ObjectID> {
    objs.iter()
        .map(|(module, name, id)| (format!("{module}::{name}"), *id))
        .collect()
}

pub fn resolve_objects(
    trading_vault: &[(String, String, ObjectID)],
    oracle_pyth: &[(String, String, ObjectID)],
    oracle_switchboard: &[(String, String, ObjectID)],
    deepbook_adapter: &[(String, String, ObjectID)],
    options_adapter: &[(String, String, ObjectID)],
    equity_oracle: &[(String, String, ObjectID)],
) -> Result<TradingVaultObjects> {
    let tv = index_created(trading_vault);
    let op = index_created(oracle_pyth);
    let osw = index_created(oracle_switchboard);
    let dba = index_created(deepbook_adapter);
    let oa = index_created(options_adapter);
    let eo = index_created(equity_oracle);
    let pick = |map: &BTreeMap<String, ObjectID>, key: &str| {
        map.get(key)
            .copied()
            .ok_or_else(|| anyhow!("{key} not found in publish effects"))
    };
    Ok(TradingVaultObjects {
        vault_protocol_config_id: pick(&tv, "registry::VaultProtocolConfig")?,
        integration_registry_id: pick(&tv, "registry::IntegrationRegistry")?,
        oracle_registry_id: pick(&tv, "registry::OracleRegistry")?,
        pyth_feed_registry_id: pick(&op, "oracle_pyth::PythFeedRegistry")?,
        switchboard_feed_registry_id: pick(
            &osw,
            "oracle_switchboard::SwitchboardFeedRegistry",
        )?,
        pool_allowlist_id: pick(&dba, "deepbook_adapter::PoolAllowlist")?,
        equity_book_id: pick(&eo, "equity_oracle::EquityBook")?,
        vol_book_id: pick(&oa, "vol_book::VolBook")?,
    })
}

async fn shared_mut_arg(client: &ChainClient, id: ObjectID) -> Result<ObjectArg> {
    client.shared_object_arg(id, /* mutable */ true).await
}

/// One PTB: witness allowlisting + feed seeding. Returns the digest.
#[allow(clippy::too_many_arguments)]
pub async fn activate(
    client: &ChainClient,
    signer: &Signer,
    objects: &TradingVaultObjects,
    admin_cap_id: ObjectID,
    trading_vault_pkg: ObjectID,
    oracle_pyth_pkg: ObjectID,
    oracle_switchboard_pkg: ObjectID,
    deepbook_adapter_pkg: ObjectID,
    options_adapter_pkg: ObjectID,
    exchange_adapter_pkg: ObjectID,
    equity_oracle_pkg: ObjectID,
    token_info: &BTreeMap<String, TokenSpec>,
    registrar_pubkey: Option<&str>,
    gas_budget: u64,
) -> Result<String> {
    // Let the fullnode index the freshly shared registries.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut pt = ProgrammableTransactionBuilder::new();

    let admin = pt.obj(
        client
            .owned_object_arg(admin_cap_id)
            .await
            .context("fetching AdminCap")?,
    )?;

    let ireg = pt.obj(shared_mut_arg(client, objects.integration_registry_id).await?)?;
    let oreg = pt.obj(shared_mut_arg(client, objects.oracle_registry_id).await?)?;
    let feed_reg = pt.obj(shared_mut_arg(client, objects.pyth_feed_registry_id).await?)?;
    let sb_feed_reg =
        pt.obj(shared_mut_arg(client, objects.switchboard_feed_registry_id).await?)?;

    let type_name_call = |pt: &mut ProgrammableTransactionBuilder, ty: &str| -> Result<_> {
        let tag = TypeTag::from_str(ty).with_context(|| format!("parsing witness type {ty}"))?;
        Ok(pt.programmable_move_call(
            ObjectID::from_hex_literal("0x1")?,
            Identifier::new("type_name")?,
            Identifier::new("with_defining_ids")?,
            vec![tag],
            vec![],
        ))
    };

    // Integration witnesses.
    for witness in [
        format!("{deepbook_adapter_pkg}::deepbook_adapter::DeepBookAdapter"),
        format!("{options_adapter_pkg}::options_adapter::OptionsAdapter"),
        format!("{exchange_adapter_pkg}::exchange_adapter::ExchangeAdapter"),
        format!("{trading_vault_pkg}::vault_mm::VaultMm"),
    ] {
        let t = type_name_call(&mut pt, &witness)?;
        pt.programmable_move_call(
            trading_vault_pkg,
            Identifier::new("registry")?,
            Identifier::new("allow_adapter")?,
            vec![],
            vec![admin, ireg, t],
        );
    }
    // Oracle witnesses: Pyth for catalog assets, the options intrinsic
    // oracle for per-bucket option coins (SO-297), and the keeper-attested
    // external-account equity oracle (SO-299).
    //
    // BOTH price providers are allowlisted at deploy time (SO-335). That
    // is deliberate: the live provider is a runtime config field, and a
    // switch must not require an on-chain ceremony. Narrowing which
    // adapter may price which asset is `registry::pin_oracle`, and
    // retiring one is `disallow_oracle` — both post-deploy decisions.
    for witness in [
        format!("{oracle_pyth_pkg}::oracle_pyth::PythOracle"),
        format!("{oracle_switchboard_pkg}::oracle_switchboard::SwitchboardOracle"),
        format!("{options_adapter_pkg}::options_oracle::OptionsOracle"),
        format!("{equity_oracle_pkg}::equity_oracle::EquityOracle"),
    ] {
        let t = type_name_call(&mut pt, &witness)?;
        pt.programmable_move_call(
            trading_vault_pkg,
            Identifier::new("registry")?,
            Identifier::new("allow_oracle")?,
            vec![],
            vec![admin, oreg, t],
        );
    }

    // Feed seeding from the token catalog, per provider (skip tokens with
    // no feed for that provider — a token may legitimately be covered by
    // one issuer and not the other, and a synthetic test token by
    // neither). Seeding both is what lets the provider switch be a config
    // change rather than a ceremony.
    let mut seeded = 0usize;
    let mut sb_seeded = 0usize;
    for (symbol, spec) in token_info {
        for (provider, pkg, module, reg_arg, count) in [
            (
                OracleProvider::Pyth,
                oracle_pyth_pkg,
                "oracle_pyth",
                feed_reg,
                &mut seeded,
            ),
            (
                OracleProvider::Switchboard,
                oracle_switchboard_pkg,
                "oracle_switchboard",
                sb_feed_reg,
                &mut sb_seeded,
            ),
        ] {
            let Some(feed) = spec.feed_for(provider) else {
                continue;
            };
            let bytes = hex::decode(feed.trim_start_matches("0x"))
                .with_context(|| format!("decoding {provider} feed id for {symbol}"))?;
            let coin_type = TypeTag::from_str(&spec.coin_type)
                .with_context(|| format!("parsing coin type for {symbol}"))?;
            let feed_arg = pt.pure(bytes)?;
            let dec_arg = pt.pure(spec.decimals)?;
            pt.programmable_move_call(
                pkg,
                Identifier::new(module)?,
                Identifier::new("set_feed")?,
                vec![coin_type],
                vec![admin, reg_arg, feed_arg, dec_arg],
            );
            *count += 1;
        }
    }

    // Registrar pubkey (SO-308): without it the attested self-serve
    // `set_external_account_attested` path aborts, leaving registration
    // admin-only.
    match registrar_pubkey {
        Some(hex_key) => {
            let bytes = hex::decode(hex_key.trim_start_matches("0x"))
                .context("decoding registrar pubkey")?;
            let cfg = pt.obj(shared_mut_arg(client, objects.vault_protocol_config_id).await?)?;
            let key_arg = pt.pure(bytes)?;
            pt.programmable_move_call(
                trading_vault_pkg,
                Identifier::new("registry")?,
                Identifier::new("set_registrar_pubkey")?,
                vec![],
                vec![admin, cfg, key_arg],
            );
        }
        None => tracing::info!(
            "no registrar pubkey configured for this env — attested \
             external-account registration stays disabled (admin-only)"
        ),
    }

    // Poster allowlists (SO-310): the keeper posts external-account equity
    // and implied vol, and both books reject unknown senders. The keeper
    // signs with the deployer key in every env we run, so the activation
    // sender is the poster.
    let poster = pt.pure(signer.address)?;
    let equity_book = pt.obj(shared_mut_arg(client, objects.equity_book_id).await?)?;
    pt.programmable_move_call(
        equity_oracle_pkg,
        Identifier::new("equity_oracle")?,
        Identifier::new("add_poster")?,
        vec![],
        vec![admin, equity_book, poster],
    );
    let vol_book = pt.obj(shared_mut_arg(client, objects.vol_book_id).await?)?;
    pt.programmable_move_call(
        options_adapter_pkg,
        Identifier::new("vol_book")?,
        Identifier::new("add_poster")?,
        vec![],
        vec![admin, vol_book, poster],
    );

    tracing::info!(
        pyth_feeds = seeded,
        switchboard_feeds = sb_seeded,
        "submitting trading-vault activation tx"
    );
    let resp =
        sui_tx::tx::submit_ptb(client, signer, pt, gas_budget, "trading-vault activation").await?;
    Ok(sui_tx::tx::tx_digest(&resp).to_string())
}
