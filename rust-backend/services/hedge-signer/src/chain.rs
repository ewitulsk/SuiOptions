//! On-chain lookups the FROST keygen gate and `prune-share` need.
//!
//! Keygen is permissionless — a freshly generated parent address is inert
//! until a protocol admin registers it with
//! `trading_vault::vault::set_external_account` — so the only thing worth
//! checking is that the caller named a REAL vault: a live shared
//! `vault::TradingVault` of the pinned trading_vault package that has no
//! external account registered yet. Fail closed: an RPC that will not
//! answer is not an approval.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use move_core_types::language_storage::StructTag;
use sui_tx::chain::ChainClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::object::Owner;

const VAULT_MODULE: &str = "vault";
const VAULT_STRUCT: &str = "TradingVault";

/// What a vault id resolves to on chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultLookup {
    /// A live shared `TradingVault` of the pinned package. `external` is
    /// the address of its registered external account, if `vault.external`
    /// is already `Some`.
    Vault { external: Option<SuiAddress> },
    /// The chain answered, but the id is not a usable vault (missing,
    /// not shared, or some other type). Carries the operator-facing reason.
    NotAVault(String),
}

/// Resolve a vault id on chain. `Err` means the lookup ITSELF failed (RPC
/// down, unreadable response) — callers must fail closed on it, never treat
/// it as "no external account".
#[async_trait]
pub trait VaultResolver: Send + Sync {
    async fn resolve(&self, vault_id: &str) -> Result<VaultLookup>;
}

/// True when `ty` is the pinned package's `vault::TradingVault`.
pub fn is_trading_vault_type(ty: &StructTag, package: ObjectID) -> bool {
    ObjectID::from(ty.address) == package
        && ty.module.as_str() == VAULT_MODULE
        && ty.name.as_str() == VAULT_STRUCT
}

/// [`VaultResolver`] over a gRPC object read.
pub struct RpcVaultResolver {
    client: ChainClient,
    /// The trading_vault package the service pins (from token-info). A
    /// vault published by an older package version is not this deployment's.
    trading_vault_package: ObjectID,
}

impl RpcVaultResolver {
    pub fn new(client: ChainClient, trading_vault_package: ObjectID) -> Self {
        Self {
            client,
            trading_vault_package,
        }
    }
}

#[async_trait]
impl VaultResolver for RpcVaultResolver {
    async fn resolve(&self, vault_id: &str) -> Result<VaultLookup> {
        let id = match ObjectID::from_hex_literal(vault_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(VaultLookup::NotAVault(format!(
                    "vault_id {vault_id} is not an object id: {e}"
                )))
            }
        };
        let Some((object, json)) = self
            .client
            .try_get_object_json(id)
            .await
            .with_context(|| format!("GetObject {id}"))?
        else {
            return Ok(VaultLookup::NotAVault(format!(
                "object {id} does not exist on chain"
            )));
        };
        if !matches!(object.owner(), Owner::Shared { .. }) {
            return Ok(VaultLookup::NotAVault(format!(
                "object {id} is not a shared object"
            )));
        }
        let Some(ty) = object.struct_tag() else {
            return Ok(VaultLookup::NotAVault(format!(
                "object {id} has no readable Move type"
            )));
        };
        if !is_trading_vault_type(&ty, self.trading_vault_package) {
            return Ok(VaultLookup::NotAVault(format!(
                "object {id} is {ty}, not {}::{VAULT_MODULE}::{VAULT_STRUCT}",
                self.trading_vault_package
            )));
        }
        let Some(fields) = json else {
            return Ok(VaultLookup::NotAVault(format!(
                "object {id} has no readable Move content"
            )));
        };
        Ok(VaultLookup::Vault {
            external: external_account(&fields)?,
        })
    }
}

/// Read `vault.external.account`. `Ok(None)` — no external account
/// registered yet. An unreadable field is an error, not a `None`.
fn external_account(fields: &serde_json::Value) -> Result<Option<SuiAddress>> {
    let Some(value) = fields.get("external") else {
        return Err(anyhow!("vault has no readable `external` field"));
    };
    // A Move `Option<ExternalAccount>` renders as the inner struct or as
    // null in the gRPC/GraphQL JSON rendering.
    if value.is_null() {
        return Ok(None);
    }
    match value.get("account").and_then(serde_json::Value::as_str) {
        Some(a) => <SuiAddress as std::str::FromStr>::from_str(a)
            .map(Some)
            .map_err(|e| anyhow!("vault.external.account is not an address: {e}")),
        None => Err(anyhow!(
            "vault.external is not an ExternalAccount: {value}"
        )),
    }
}

/// Coin types `addr` still holds a non-zero balance of, formatted for an
/// operator message. Empty ⇒ the address is drained.
pub async fn nonzero_balances(client: &ChainClient, addr: SuiAddress) -> Result<Vec<String>> {
    let balances = client
        .all_balances(addr)
        .await
        .with_context(|| format!("listing balances for {addr}"))?;
    Ok(balances
        .into_iter()
        .filter(|(_, total)| *total > 0)
        .map(|(coin_type, total)| format!("{coin_type} ({total})"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn pkg() -> ObjectID {
        ObjectID::from_hex_literal("0x77").unwrap()
    }

    fn tag(s: &str) -> StructTag {
        StructTag::from_str(s).unwrap()
    }

    #[test]
    fn pinned_vault_type_matches_whatever_the_address_padding() {
        // The RPC renders the package address padded; the pin comes from
        // token-info unpadded. Both must compare equal.
        assert!(is_trading_vault_type(
            &tag("0x0000000000000000000000000000000000000000000000000000000000000077::vault::TradingVault"),
            pkg()
        ));
        assert!(is_trading_vault_type(
            &tag("0x77::vault::TradingVault"),
            pkg()
        ));
    }

    #[test]
    fn other_package_module_or_struct_does_not_match() {
        assert!(!is_trading_vault_type(
            &tag("0x78::vault::TradingVault"),
            pkg()
        ));
        assert!(!is_trading_vault_type(
            &tag("0x77::vault_mm::TradingVault"),
            pkg()
        ));
        assert!(!is_trading_vault_type(
            &tag("0x77::vault::CuratorCap"),
            pkg()
        ));
    }

    /// A Move object's fields exactly as the gRPC/GraphQL `json` rendering
    /// gives them: struct fields nested directly, `Option` as null-or-inner.
    fn fields(json: serde_json::Value) -> serde_json::Value {
        json
    }

    #[test]
    fn unset_external_reads_as_none() {
        let f = fields(serde_json::json!({ "external": null, "creator": "0x1" }));
        assert_eq!(external_account(&f).unwrap(), None);
    }

    #[test]
    fn registered_external_reads_the_account_address() {
        let acct = "0x00000000000000000000000000000000000000000000000000000000000000ee";
        let f = fields(serde_json::json!({
            "external": { "account": acct, "exposure": "0" },
        }));
        assert_eq!(
            external_account(&f).unwrap(),
            Some(SuiAddress::from_str(acct).unwrap())
        );
    }

    #[test]
    fn missing_external_field_is_an_error_not_a_none() {
        let f = fields(serde_json::json!({ "creator": "0x1" }));
        assert!(external_account(&f).is_err());
    }
}
