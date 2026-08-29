//! Sui hub access: PTB builders for delivering spoke→hub messages through
//! the dev relayer endpoint and for pushing ConfigSync, behind the
//! [`HubChain`] trait so tests can mock the chain.
//!
//! Delivery PTB shapes (one message per PTB):
//!
//! - `DepositNotice` / `WithdrawRequest` (valuation-bearing):
//!   price legs (oracle-service descriptor + `/oracle/legs`, mirroring
//!   the keeper/staging-mm-bot composer) → `begin_appraisal<Accounting>`
//!   (+ holdings legs) → `multichain::record_spoke_state` with the spoke
//!   marker attestation → `endpoint_relayer::deliver(bytes)` →
//!   `multichain::handle_*` → `endpoint_relayer::send(out)`.
//! - `PayoutReceipt` / `StateSync`: `deliver(bytes)` → `handle_*`
//!   (no appraisal, no outbound).
//! - ConfigSync: `multichain::build_config_sync` → `endpoint_relayer::send`.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use serde_json::Value;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Argument, Command};

use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb, tx_digest};
use vault_messages::MsgType;

/// The hub-chain operations the deliverer/cranks need.
#[async_trait]
pub trait HubChain: Send + Sync {
    /// Deliver one spoke→hub wire message; returns the tx digest.
    async fn deliver(&self, msg_type: MsgType, message: &[u8]) -> Result<String>;
    /// Build + send a ConfigSync for the configured spoke.
    async fn send_config_sync(&self) -> Result<String>;
    /// The hub's last APPLIED spoke→hub seq for the configured spoke
    /// (`Spoke.inbound_seq`, read off the vault object).
    async fn spoke_inbound_seq(&self) -> Result<u64>;
}

/// Parsed object identities off the `[hub]` config block.
pub struct HubRefs {
    pub trading_vault_pkg: ObjectID,
    pub vault_id: ObjectID,
    pub protocol_config_id: ObjectID,
    pub endpoint_registry_id: ObjectID,
    pub oracle_registry_id: ObjectID,
    pub deepbook_adapter_pkg: Option<ObjectID>,
    pub options_adapter_pkg: Option<ObjectID>,
    pub exchange_adapter_pkg: Option<ObjectID>,
    pub equity_oracle_pkg: Option<ObjectID>,
    pub equity_book_id: Option<ObjectID>,
    pub vol_book_id: Option<ObjectID>,
}

impl HubRefs {
    pub fn parse(cfg: &crate::config::HubConfig) -> Result<Self> {
        let id = |s: &str, name: &str| {
            ObjectID::from_hex_literal(s).with_context(|| format!("bad hub.{name}: {s}"))
        };
        let opt = |o: &Option<String>, name: &str| -> Result<Option<ObjectID>> {
            o.as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| id(s, name))
                .transpose()
        };
        Ok(Self {
            trading_vault_pkg: id(&cfg.trading_vault_pkg, "trading_vault_pkg")?,
            vault_id: id(&cfg.vault_id, "vault_id")?,
            protocol_config_id: id(&cfg.protocol_config_id, "protocol_config_id")?,
            endpoint_registry_id: id(&cfg.endpoint_registry_id, "endpoint_registry_id")?,
            oracle_registry_id: id(&cfg.oracle_registry_id, "oracle_registry_id")?,
            deepbook_adapter_pkg: opt(&cfg.deepbook_adapter_pkg, "deepbook_adapter_pkg")?,
            options_adapter_pkg: opt(&cfg.options_adapter_pkg, "options_adapter_pkg")?,
            exchange_adapter_pkg: opt(&cfg.exchange_adapter_pkg, "exchange_adapter_pkg")?,
            equity_oracle_pkg: opt(&cfg.equity_oracle_pkg, "equity_oracle_pkg")?,
            equity_book_id: opt(&cfg.equity_book_id, "equity_book_id")?,
            vol_book_id: opt(&cfg.vol_book_id, "vol_book_id")?,
        })
    }

    fn appraisal_refs(&self) -> sui_tx::tx::appraisal::AppraisalRefs {
        sui_tx::tx::appraisal::AppraisalRefs {
            trading_vault_pkg: self.trading_vault_pkg,
            deepbook_adapter_pkg: self.deepbook_adapter_pkg,
            options_adapter_pkg: self.options_adapter_pkg,
            exchange_adapter_pkg: self.exchange_adapter_pkg,
            vault_id: self.vault_id,
            protocol_config_id: self.protocol_config_id,
            oracle_registry_id: self.oracle_registry_id,
            equity_oracle_pkg: self.equity_oracle_pkg,
            equity_book_id: self.equity_book_id,
            vol_book_id: self.vol_book_id,
        }
    }
}

pub struct HubClient {
    pub wrap: SuiClientWrapper,
    pub oracle: oracle_client::OracleClient,
    pub refs: HubRefs,
    pub spoke_id: u64,
    /// Canonical Sui marker type for the spoke asset (the attestation the
    /// deposit/withdraw handlers and `record_spoke_state` consume).
    pub marker: String,
    pub gas_budget: u64,
}

impl HubClient {
    fn call(
        &self,
        pt: &mut ProgrammableTransactionBuilder,
        module: &str,
        function: &str,
        args: Vec<Argument>,
    ) -> Argument {
        pt.programmable_move_call(
            self.refs.trading_vault_pkg,
            Identifier::new(module).unwrap(),
            Identifier::new(function).unwrap(),
            vec![],
            args,
        )
    }

    fn attestation_tag(&self) -> Result<TypeTag> {
        TypeTag::from_str(&format!(
            "{}::price::PriceAttestation",
            self.refs.trading_vault_pkg
        ))
        .context("attestation type tag")
    }

    /// The appraisal-bearing prefix for DepositNotice / WithdrawRequest:
    /// price legs + `begin_appraisal` + holdings legs + the
    /// `record_spoke_state` leg. Returns (appraisal, marker attestation).
    async fn compose_valued_prefix(
        &self,
        pt: &mut ProgrammableTransactionBuilder,
    ) -> Result<(Argument, Argument)> {
        let client = &self.wrap.client;
        let holdings = sui_tx::tx::appraisal::discover_holdings(client, self.refs.vault_id)
            .await
            .context("discovering hub vault holdings")?;
        let descriptor = self.oracle.descriptor().await.context("fetching /oracle/descriptor")?;
        if descriptor.provider != protocol_types::OracleProvider::Switchboard {
            // The Pyth leg path needs the keeper's feed/price-table context;
            // wire it up when a Pyth pin actually serves the spoke marker.
            bail!(
                "live oracle provider {} is not supported by vault-messenger yet — \
                 pin the Switchboard adapter or extend the composer",
                descriptor.provider
            );
        }
        let (appraisal, attestations) = sui_tx::tx::appraisal::compose_switchboard_appraisal(
            client,
            pt,
            &self.refs.appraisal_refs(),
            &holdings,
            &BTreeMap::new(),
            &descriptor,
            &self.oracle,
            &[self.marker.clone()],
        )
        .await
        .context("composing the appraisal")?;

        let att = *attestations.get(&self.marker).ok_or_else(|| {
            anyhow!(
                "no attestation composed for spoke marker {} — descriptor feed missing?",
                self.marker
            )
        })?;

        // record_spoke_state(&vault, cfg, &mut appraisal, spoke_id, atts, clock)
        let vault_ro = pt.obj(shared_object_arg(client, self.refs.vault_id, false).await?)?;
        let cfg = pt.obj(shared_object_arg(client, self.refs.protocol_config_id, false).await?)?;
        let clock = clock_arg(pt)?;
        let spoke_arg = pt.pure(self.spoke_id)?;
        let atts_vec = pt.command(Command::make_move_vec(
            Some(self.attestation_tag()?.into()),
            vec![att],
        ));
        self.call(
            pt,
            "multichain",
            "record_spoke_state",
            vec![vault_ro, cfg, appraisal, spoke_arg, atts_vec, clock],
        );
        Ok((appraisal, att))
    }

    /// `endpoint_relayer::deliver(reg, bytes)` → `VerifiedInbound`.
    async fn compose_deliver(
        &self,
        pt: &mut ProgrammableTransactionBuilder,
        message: &[u8],
    ) -> Result<(Argument, Argument)> {
        let reg = pt.obj(
            shared_object_arg(&self.wrap.client, self.refs.endpoint_registry_id, false).await?,
        )?;
        let bytes_arg = pt.pure(message.to_vec())?;
        let inbound = self.call(pt, "endpoint_relayer", "deliver", vec![reg, bytes_arg]);
        Ok((reg, inbound))
    }
}

#[async_trait]
impl HubChain for HubClient {
    async fn deliver(&self, msg_type: MsgType, message: &[u8]) -> Result<String> {
        let client = &self.wrap.client;
        let mut pt = ProgrammableTransactionBuilder::new();
        let label = match msg_type {
            MsgType::DepositNotice => "deliver deposit_notice",
            MsgType::WithdrawRequest => "deliver withdraw_request",
            MsgType::PayoutReceipt => "deliver payout_receipt",
            MsgType::StateSync => "deliver state_sync",
            other => bail!("{other:?} is not a spoke->hub message"),
        };

        match msg_type {
            MsgType::DepositNotice | MsgType::WithdrawRequest => {
                let (appraisal, att) = self.compose_valued_prefix(&mut pt).await?;
                let (reg, inbound) = self.compose_deliver(&mut pt, message).await?;
                // Upgrades the vault input (shared by the appraisal legs)
                // to mutable — the builder merges mutabilities.
                let vault = pt.obj(shared_object_arg(client, self.refs.vault_id, true).await?)?;
                let cfg =
                    pt.obj(shared_object_arg(client, self.refs.protocol_config_id, false).await?)?;
                let clock = clock_arg(&mut pt)?;
                let handler = if msg_type == MsgType::DepositNotice {
                    "handle_deposit_notice"
                } else {
                    "handle_withdraw_request"
                };
                let out = self.call(
                    &mut pt,
                    "multichain",
                    handler,
                    vec![vault, cfg, reg, inbound, appraisal, att, clock],
                );
                self.call(&mut pt, "endpoint_relayer", "send", vec![reg, out]);
            }
            MsgType::PayoutReceipt | MsgType::StateSync => {
                let (reg, inbound) = self.compose_deliver(&mut pt, message).await?;
                let vault = pt.obj(shared_object_arg(client, self.refs.vault_id, true).await?)?;
                let handler = if msg_type == MsgType::PayoutReceipt {
                    "handle_payout_receipt"
                } else {
                    "handle_state_sync"
                };
                self.call(&mut pt, "multichain", handler, vec![vault, reg, inbound]);
            }
            _ => unreachable!("checked above"),
        }

        let resp = submit_ptb(client, &self.wrap.signer, pt, self.gas_budget, label).await?;
        Ok(tx_digest(&resp).to_string())
    }

    async fn send_config_sync(&self) -> Result<String> {
        let client = &self.wrap.client;
        let mut pt = ProgrammableTransactionBuilder::new();
        let vault = pt.obj(shared_object_arg(client, self.refs.vault_id, true).await?)?;
        let cfg = pt.obj(shared_object_arg(client, self.refs.protocol_config_id, false).await?)?;
        let reg = pt
            .obj(shared_object_arg(client, self.refs.endpoint_registry_id, false).await?)?;
        let spoke_arg = pt.pure(self.spoke_id)?;
        let out = self.call(
            &mut pt,
            "multichain",
            "build_config_sync",
            vec![vault, cfg, reg, spoke_arg],
        );
        self.call(&mut pt, "endpoint_relayer", "send", vec![reg, out]);
        let resp = submit_ptb(client, &self.wrap.signer, pt, self.gas_budget, "config_sync").await?;
        Ok(tx_digest(&resp).to_string())
    }

    async fn spoke_inbound_seq(&self) -> Result<u64> {
        let (_, json) = self
            .wrap
            .client
            .get_object_json(self.refs.vault_id)
            .await
            .context("fetching hub vault")?;
        let json = json.ok_or_else(|| anyhow!("hub vault has no parsed content"))?;
        spoke_inbound_seq_from_vault(&json, self.spoke_id)
            .ok_or_else(|| anyhow!("spoke {} not found on the hub vault", self.spoke_id))
    }
}

/// Read `Spoke.inbound_seq` for `spoke_id` out of the vault object's JSON
/// (`spokes` is a `VecMap<u64, Spoke>` — `{contents: [{key, value}]}`).
pub fn spoke_inbound_seq_from_vault(vault_json: &Value, spoke_id: u64) -> Option<u64> {
    let entries = vault_json.pointer("/spokes/contents")?.as_array()?;
    for e in entries {
        let key = json_u64(e.get("key")?)?;
        if key == spoke_id {
            return json_u64(e.pointer("/value/inbound_seq")?);
        }
    }
    None
}

/// Move u64s render as JSON strings in some renderings and numbers in
/// others — accept both (same posture as the appraisal composer).
pub fn json_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_spoke_inbound_seq_in_both_renderings() {
        let vault = json!({
            "spokes": { "contents": [
                { "key": "3", "value": { "inbound_seq": "7", "outbound_seq": "2" } },
                { "key": 4, "value": { "inbound_seq": 12 } },
            ]}
        });
        assert_eq!(spoke_inbound_seq_from_vault(&vault, 3), Some(7));
        assert_eq!(spoke_inbound_seq_from_vault(&vault, 4), Some(12));
        assert_eq!(spoke_inbound_seq_from_vault(&vault, 5), None);
        assert_eq!(spoke_inbound_seq_from_vault(&json!({}), 3), None);
    }
}
