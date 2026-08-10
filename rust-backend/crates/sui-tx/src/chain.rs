//! gRPC chain access.
//!
//! Sui deactivated JSON-RPC on public fullnodes (see
//! `docs/sui-json-rpc-migration.md`), so every read and write in this
//! workspace goes through `sui_rpc_api::Client` (gRPC) instead of
//! `sui_sdk::SuiClient`. The proto types convert to/from `sui-types` at the
//! same git pin, so call sites keep the `sui-types` vocabulary they already
//! use — `Object`, `ObjectRef`, `TransactionData`, `TransactionEffects`.
//!
//! One deliberate gap: gRPC has no events query. Event reads live in
//! [`crate::events`] and go over GraphQL.
//!
//! `sui_rpc_api::Client` takes `&mut self` on most reads but is cheap to
//! clone (it is a `tonic` channel handle), so every method here takes
//! `&self` and clones internally. That keeps `ChainClient` usable behind a
//! shared reference, which is how every service holds it.

use anyhow::{anyhow, Context, Result};
use move_core_types::language_storage::StructTag;
use sui_rpc_api::client::SimulateTransactionResponse;
use sui_rpc_api::Client;
use sui_types::base_types::{ObjectID, ObjectRef, SuiAddress};
use sui_types::digests::{ChainIdentifier, TransactionDigest};
use sui_types::object::{Object, Owner};
use sui_types::transaction::{
    ObjectArg, SharedObjectMutability, Transaction, TransactionData,
};

pub use sui_rpc_api::client::ExecutedTransaction;

/// Gas envelope for dev-inspect simulations. Checks are disabled so these
/// are never charged or validated — they only have to be well-formed.
const DEV_INSPECT_GAS_BUDGET: u64 = 50_000_000_000;
const DEV_INSPECT_GAS_PRICE: u64 = 1000;

/// A gRPC chain client bound to one endpoint.
#[derive(Clone)]
pub struct ChainClient {
    inner: Client,
    /// Host only (never the full URL — an operator override can carry a
    /// token in the path).
    host: String,
}

impl ChainClient {
    pub fn new(url: &str) -> Result<Self> {
        let inner = Client::new(url.to_owned())
            .map_err(|e| anyhow!("building gRPC client for {}: {e}", redact(url)))?;
        Ok(Self {
            inner,
            host: redact(url),
        })
    }

    /// Host of the endpoint, safe to log.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Escape hatch for the few call sites that need a raw proto request
    /// (dynamic-field listing, coin metadata).
    pub fn raw(&self) -> Client {
        self.inner.clone()
    }

    // ---- object reads -------------------------------------------------

    pub async fn get_object(&self, id: ObjectID) -> Result<Object> {
        self.inner
            .clone()
            .get_object(id)
            .await
            .with_context(|| format!("gRPC GetObject {id}"))
    }

    /// `Ok(None)` when the object does not exist (or was wrapped/deleted),
    /// mirroring the old `SuiObjectResponse.data == None` branch. Other
    /// transport errors still propagate.
    pub async fn try_get_object(&self, id: ObjectID) -> Result<Option<Object>> {
        match self.inner.clone().get_object(id).await {
            Ok(o) => Ok(Some(o)),
            Err(s) if s.code() == tonic::Code::NotFound => Ok(None),
            Err(s) => Err(anyhow!("gRPC GetObject {id}: {s}")),
        }
    }

    /// Block until `id` is readable, or give up after `attempts`.
    ///
    /// `ExecuteTransaction` returns on validator finality, but the
    /// fullnode's read view lags it — so an object this address just
    /// created can still 404 on `GetObject`. Any follow-up transaction
    /// that references it has to build an `ObjectArg` from a read, and
    /// that read is what fails. Retrying the *submission* cannot help:
    /// the failure happens while building, before anything is signed.
    ///
    /// Same lag as the stale gas reference in
    /// [`crate::tx::is_stale_gas_rejection`], observed from the other
    /// side — there a read was too old, here it is too early.
    pub async fn await_object(&self, id: ObjectID, attempts: u32) -> Result<Object> {
        let mut delay = std::time::Duration::from_millis(300);
        for attempt in 1..=attempts {
            if let Some(obj) = self.try_get_object(id).await? {
                return Ok(obj);
            }
            if attempt < attempts {
                tracing::debug!(%id, attempt, "object not visible to the fullnode yet; waiting");
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
        Err(anyhow!(
            "object {id} still not visible to the fullnode after {attempts} attempts"
        ))
    }

    /// Object plus its JSON rendering — the replacement for
    /// `SuiObjectDataOptions::with_content()`. Field names match the Move
    /// struct; enums render as `{"@variant": "..."}`.
    pub async fn get_object_json(
        &self,
        id: ObjectID,
    ) -> Result<(Object, Option<serde_json::Value>)> {
        self.inner
            .clone()
            .get_object_with_json(id)
            .await
            .with_context(|| format!("gRPC GetObject(+json) {id}"))
    }

    /// [`ChainClient::get_object_json`] with the `Ok(None)`-on-absent shape
    /// of [`ChainClient::try_get_object`].
    pub async fn try_get_object_json(
        &self,
        id: ObjectID,
    ) -> Result<Option<(Object, Option<serde_json::Value>)>> {
        match self.inner.clone().get_object_with_json(id).await {
            Ok(v) => Ok(Some(v)),
            Err(s) if s.code() == tonic::Code::NotFound => Ok(None),
            Err(s) => Err(anyhow!("gRPC GetObject(+json) {id}: {s}")),
        }
    }

    pub async fn multi_get_objects(&self, ids: &[ObjectID]) -> Result<Vec<Object>> {
        self.inner
            .batch_get_objects(ids)
            .await
            .with_context(|| format!("gRPC BatchGetObjects ({} ids)", ids.len()))
    }

    // ---- object args --------------------------------------------------

    /// Build a `SharedObject` `ObjectArg` from a current chain read.
    pub async fn shared_object_arg(&self, id: ObjectID, mutable: bool) -> Result<ObjectArg> {
        let obj = self.get_object(id).await?;
        match obj.owner() {
            Owner::Shared {
                initial_shared_version,
            } => Ok(ObjectArg::SharedObject {
                id,
                initial_shared_version: *initial_shared_version,
                mutability: if mutable {
                    SharedObjectMutability::Mutable
                } else {
                    SharedObjectMutability::Immutable
                },
            }),
            other => Err(anyhow!("object {id} is not shared: {other:?}")),
        }
    }

    /// Build an owned-object `ObjectArg` (e.g. an AdminCap held by the
    /// deployer).
    pub async fn owned_object_arg(&self, id: ObjectID) -> Result<ObjectArg> {
        let obj = self.get_object(id).await?;
        Ok(ObjectArg::ImmOrOwnedObject(obj.compute_object_reference()))
    }

    /// Resolve an object id to the right `ObjectArg` by reading its owner —
    /// shared objects become `SharedObject`, everything else
    /// `ImmOrOwnedObject`. This is what the retired JSON-RPC
    /// `transaction_builder().move_call(..)` did internally, and it lets
    /// callers pass a bare id without knowing the ownership up front.
    pub async fn object_arg(&self, id: ObjectID, mutable: bool) -> Result<ObjectArg> {
        let obj = self.get_object(id).await?;
        match obj.owner() {
            Owner::Shared {
                initial_shared_version,
            } => Ok(ObjectArg::SharedObject {
                id,
                initial_shared_version: *initial_shared_version,
                mutability: if mutable {
                    SharedObjectMutability::Mutable
                } else {
                    SharedObjectMutability::Immutable
                },
            }),
            _ => Ok(ObjectArg::ImmOrOwnedObject(obj.compute_object_reference())),
        }
    }

    // ---- dynamic fields -----------------------------------------------

    /// Every dynamic field (and dynamic *object* field) under `parent`,
    /// paged to exhaustion. Replaces `read_api().get_dynamic_fields(..)`.
    pub async fn dynamic_fields(&self, parent: ObjectID) -> Result<Vec<DynamicFieldEntry>> {
        let mut out = Vec::new();
        let mut token: Option<bytes::Bytes> = None;
        loop {
            let page = self
                .inner
                .get_dynamic_fields(parent, Some(100), token.clone())
                .await
                .map_err(|s| anyhow!("gRPC ListDynamicFields {parent}: {s}"))?;
            for df in &page.dynamic_fields {
                let Some(field_id) = df.field_id.as_ref().and_then(|s| s.parse().ok()) else {
                    continue;
                };
                // The field object is a `0x2::dynamic_field::Field<K, V>`;
                // its first type parameter is the NAME type, which is what
                // callers match on to tell one key struct from another.
                //
                // Dynamic OBJECT fields wrap the caller's key as
                // `0x2::dynamic_object_field::Wrapper<K>` — unwrap it, or
                // every dof key silently matches nothing and dof-backed
                // state (vault positions!) looks empty. Observed live:
                // appraisals composed 0 of 1 positions and aborted with
                // code 82 at consume_appraisal.
                let name_type = df
                    .field_object
                    .as_ref()
                    .and_then(|o| o.object_type.as_deref())
                    .and_then(|t| sui_types::parse_sui_struct_tag(t).ok())
                    .and_then(|t| t.type_params.first().cloned())
                    .map(|t| match t {
                        move_core_types::language_storage::TypeTag::Struct(ref s)
                            if s.address == move_core_types::account_address::AccountAddress::TWO
                                && s.module.as_str() == "dynamic_object_field"
                                && s.name.as_str() == "Wrapper"
                                && s.type_params.len() == 1 =>
                        {
                            s.type_params[0].clone()
                        }
                        other => other,
                    })
                    .map(|t| t.to_canonical_string(/* with_prefix */ true));
                out.push(DynamicFieldEntry {
                    field_id,
                    name_type,
                    value_type: df.value_type.clone(),
                    child_id: df.child_id.as_ref().and_then(|s| s.parse().ok()),
                });
            }
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }

    // ---- coins --------------------------------------------------------

    /// Objects owned by `owner`, up to `limit`. Replaces
    /// `read_api().get_owned_objects(..)` with no type filter.
    pub async fn owned_objects(&self, owner: SuiAddress, limit: u32) -> Result<Vec<Object>> {
        let page = self
            .inner
            .get_owned_objects(owner, None, Some(limit), None)
            .await
            .map_err(|s| anyhow!("gRPC ListOwnedObjects for {owner}: {s}"))?;
        Ok(page.items)
    }

    /// Objects of exactly `object_type` owned by `owner`, up to `limit`.
    /// Replaces `get_owned_objects` with a `StructType` filter.
    pub async fn owned_objects_of_type(
        &self,
        owner: SuiAddress,
        object_type: StructTag,
        limit: u32,
    ) -> Result<Vec<Object>> {
        let page = self
            .inner
            .get_owned_objects(owner, Some(object_type), Some(limit), None)
            .await
            .map_err(|s| anyhow!("gRPC ListOwnedObjects for {owner}: {s}"))?;
        Ok(page.items)
    }

    /// Coins of `coin_type` owned by `owner`, largest balance first.
    /// Replaces `coin_read_api().get_coins(..)`.
    pub async fn coins(&self, owner: SuiAddress, coin_type: &StructTag) -> Result<Vec<CoinRef>> {
        let coin_struct = coin_wrapper(coin_type);
        let page = self
            .inner
            .get_owned_objects(owner, Some(coin_struct), Some(200), None)
            .await
            .map_err(|s| anyhow!("gRPC ListOwnedObjects (coins) for {owner}: {s}"))?;

        let mut coins: Vec<CoinRef> = page
            .items
            .iter()
            .filter_map(|o| {
                let c = o.as_coin_maybe()?;
                Some(CoinRef {
                    object_ref: o.compute_object_reference(),
                    balance: c.value(),
                })
            })
            .collect();
        coins.sort_unstable_by(|a, b| b.balance.cmp(&a.balance));
        Ok(coins)
    }

    /// How `owner` can pay a `gas_budget`, given what it actually holds.
    ///
    /// See [`plan_gas`] for the rules. This is the only gas selector in the
    /// workspace — every submission path goes through
    /// [`crate::tx::gas_tx_data`], which calls this.
    pub async fn gas_payment(&self, owner: SuiAddress, gas_budget: u64) -> Result<GasPayment> {
        let coins = self.coins(owner, &sui_coin_type()).await?;
        let address_balance = self.address_balance(owner, &sui_coin_type()).await?;
        plan_gas(&coins, address_balance, gas_budget)
            .with_context(|| format!("selecting gas payment for {owner}"))
    }

    /// Total spendable balance of `coin_type` — coin objects *and* the
    /// address balance. Replaces `coin_read_api().get_balance(..)`.
    pub async fn balance(&self, owner: SuiAddress, coin_type: &StructTag) -> Result<u128> {
        let b = self
            .inner
            .get_balance(owner, coin_type)
            .await
            .map_err(|s| anyhow!("gRPC GetBalance for {owner}: {s}"))?;
        Ok(b.balance() as u128)
    }

    /// The `coin_type` funds held as an *address balance* (the accumulator),
    /// as opposed to `Coin<T>` objects.
    ///
    /// This is where faucet drips and plain transfers land now, and it is not
    /// reachable as a transaction input without a
    /// `sui::funds_accumulator::Withdrawal` (see [`crate::tx::funding`]) or,
    /// for gas, an empty gas payment (see [`crate::tx::gas_tx_data`]).
    ///
    /// Nodes that predate address balances leave the field unset, which reads
    /// as zero — exactly the right answer there.
    pub async fn address_balance(&self, owner: SuiAddress, coin_type: &StructTag) -> Result<u64> {
        let b = self
            .inner
            .get_balance(owner, coin_type)
            .await
            .map_err(|s| anyhow!("gRPC GetBalance for {owner}: {s}"))?;
        Ok(b.address_balance_opt().unwrap_or(0))
    }

    /// Every coin type `owner` holds, with its total balance. Replaces
    /// `coin_read_api().get_all_balances(..)`.
    pub async fn all_balances(&self, owner: SuiAddress) -> Result<Vec<(String, u128)>> {
        use futures::TryStreamExt;
        let stream = self.inner.list_balances(owner);
        futures::pin_mut!(stream);
        let mut out = Vec::new();
        while let Some(b) = stream
            .try_next()
            .await
            .map_err(|s| anyhow!("gRPC ListBalances for {owner}: {s}"))?
        {
            out.push((b.coin_type().to_owned(), b.balance() as u128));
        }
        Ok(out)
    }

    // ---- gas / epoch --------------------------------------------------

    pub async fn reference_gas_price(&self) -> Result<u64> {
        self.inner
            .get_reference_gas_price()
            .await
            .map_err(|s| anyhow!("gRPC GetEpoch (reference gas price): {s}"))
    }

    pub async fn latest_checkpoint(&self) -> Result<u64> {
        let cp = self
            .inner
            .clone()
            .get_latest_checkpoint()
            .await
            .map_err(|s| anyhow!("gRPC GetCheckpoint (latest): {s}"))?;
        Ok(*cp.sequence_number())
    }

    pub async fn chain_identifier(&self) -> Result<String> {
        Ok(self.chain_id().await?.to_string())
    }

    /// The chain identifier as the SDK type. Address-balance gas payments are
    /// bound to it (`TransactionExpiration::ValidDuring`) so a transaction
    /// signed for one network cannot be replayed on another.
    pub async fn chain_id(&self) -> Result<ChainIdentifier> {
        self.inner
            .get_chain_identifier()
            .await
            .map_err(|s| anyhow!("gRPC GetServiceInfo (chain id): {s}"))
    }

    /// Current epoch — the other half of a `ValidDuring` expiration.
    pub async fn current_epoch(&self) -> Result<u64> {
        self.inner
            .get_current_epoch()
            .await
            .map_err(|s| anyhow!("gRPC GetEpoch (current epoch): {s}"))
    }

    // ---- simulate / execute -------------------------------------------

    /// Simulate with checks DISABLED — the `dev_inspect_transaction_block`
    /// replacement, used to read Move return values without paying gas or
    /// owning the objects.
    pub async fn dev_inspect(&self, tx: &TransactionData) -> Result<SimulateTransactionResponse> {
        self.inner
            .simulate_transaction(tx, false, false)
            .await
            .map_err(|s| anyhow!("gRPC SimulateTransaction (dev-inspect): {s}"))
    }

    /// Dev-inspect a PTB: build a gas-less `TransactionData` for `sender`
    /// and simulate it with checks disabled. This is the direct replacement
    /// for `dev_inspect_transaction_block(sender, kind, ..)` — no gas coin,
    /// no signature, no ownership requirement.
    pub async fn dev_inspect_ptb(
        &self,
        sender: SuiAddress,
        pt: sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder,
    ) -> Result<SimulateTransactionResponse> {
        // Checks are disabled, so the gas payment is never validated or
        // charged; an empty payment keeps this read free of coin lookups.
        let tx = TransactionData::new_programmable(
            sender,
            vec![],
            pt.finish(),
            DEV_INSPECT_GAS_BUDGET,
            DEV_INSPECT_GAS_PRICE,
        );
        self.dev_inspect(&tx).await
    }

    /// Simulate with checks ENABLED — the `dry_run_transaction_block`
    /// replacement: a real feasibility check against current state.
    pub async fn dry_run(&self, tx: &TransactionData) -> Result<SimulateTransactionResponse> {
        self.inner
            .simulate_transaction(tx, true, false)
            .await
            .map_err(|s| anyhow!("gRPC SimulateTransaction (dry-run): {s}"))
    }

    /// Submit a signed transaction and wait for finality.
    pub async fn execute(&self, tx: &Transaction) -> Result<ExecutedTransaction> {
        self.inner
            .clone()
            .execute_transaction(tx)
            .await
            .map_err(|s| anyhow!("gRPC ExecuteTransaction: {s}"))
    }

    pub async fn get_transaction(&self, digest: &TransactionDigest) -> Result<ExecutedTransaction> {
        self.inner
            .clone()
            .get_transaction(digest)
            .await
            .map_err(|s| anyhow!("gRPC GetTransaction {digest}: {s}"))
    }

    /// `Ok(None)` while the node has not yet indexed the digest — the
    /// polling shape the cctp-relay and the deploy checkpoint lookup want.
    pub async fn try_get_transaction(
        &self,
        digest: &TransactionDigest,
    ) -> Result<Option<ExecutedTransaction>> {
        match self.inner.clone().get_transaction(digest).await {
            Ok(t) => Ok(Some(t)),
            Err(s) if s.code() == tonic::Code::NotFound => Ok(None),
            Err(s) => Err(anyhow!("gRPC GetTransaction {digest}: {s}")),
        }
    }
}

/// BCS bytes of the `n`-th return value of the LAST command in a simulated
/// transaction — the `results.last().return_values.first()` shape every
/// dev-inspect call site used.
///
/// The simulation's own execution status is checked first, so a Move abort
/// surfaces as an error here rather than as "no values returned".
pub fn return_value_bytes(
    resp: &SimulateTransactionResponse,
    n: usize,
) -> Result<&[u8]> {
    use sui_types::effects::TransactionEffectsAPI;
    let status = resp.transaction.effects.status();
    if status.is_err() {
        return Err(anyhow!("simulation reverted: {status:?}"));
    }
    let last = resp
        .command_outputs
        .last()
        .ok_or_else(|| anyhow!("simulation returned no command results"))?;
    let out = last
        .return_values
        .get(n)
        .ok_or_else(|| anyhow!("simulation command has no return value at index {n}"))?;
    let bcs = out
        .value
        .as_ref()
        .ok_or_else(|| anyhow!("simulation return value {n} carries no BCS payload"))?;
    Ok(&bcs.value())
}

/// Decode the `n`-th return value of the last simulated command as `T`.
pub fn decode_return_value<T: serde::de::DeserializeOwned>(
    resp: &SimulateTransactionResponse,
    n: usize,
) -> Result<T> {
    let bytes = return_value_bytes(resp, n)?;
    bcs::from_bytes::<T>(bytes).context("decoding simulated return value")
}

/// One dynamic field under a parent object.
#[derive(Debug, Clone)]
pub struct DynamicFieldEntry {
    /// The `Field<K, V>` object itself. Read it with
    /// [`ChainClient::get_object_json`] to get `{ "name": K, "value": V }`.
    pub field_id: ObjectID,
    /// Canonical type string of the field's NAME (`K`), when the node
    /// returned the field object.
    pub name_type: Option<String>,
    /// Type of the field's value — or, for a dynamic OBJECT field, the type
    /// of the child object.
    pub value_type: Option<String>,
    /// Set only for dynamic object fields: the child object's id.
    pub child_id: Option<ObjectID>,
}

impl DynamicFieldEntry {
    /// Does this field's name type end with `suffix`
    /// (e.g. `"::vault::PositionKey"`)?
    pub fn name_type_ends_with(&self, suffix: &str) -> bool {
        self.name_type.as_deref().is_some_and(|t| t.ends_with(suffix))
    }
}

/// An object touched by a transaction, in the shape the old
/// `ObjectChange::Created` carried. `object_type` is the canonical type
/// string as the node rendered it (`0x2::coin::TreasuryCap<0x..::call::CALL>`).
#[derive(Debug, Clone)]
pub struct ChangedObject {
    pub object_id: ObjectID,
    pub object_type: String,
    pub version: u64,
    pub digest: String,
}

/// Objects *created* by a transaction — the `ObjectChange::Created` subset
/// of the old `object_changes`.
pub fn created_objects(resp: &ExecutedTransaction) -> Vec<ChangedObject> {
    use sui_rpc::proto::sui::rpc::v2::changed_object::{IdOperation, OutputObjectState};
    resp.changed_objects
        .iter()
        .filter(|o| {
            matches!(o.output_state(), OutputObjectState::ObjectWrite)
                && matches!(o.id_operation(), IdOperation::Created)
        })
        .filter_map(|o| {
            Some(ChangedObject {
                object_id: o.object_id().parse().ok()?,
                object_type: o.object_type().to_owned(),
                version: o.output_version(),
                digest: o.output_digest().to_owned(),
            })
        })
        .collect()
}

/// Package id published by a transaction, if any.
pub fn published_package(resp: &ExecutedTransaction) -> Option<ObjectID> {
    resp.get_new_package_obj().map(|r| r.0)
}

/// Protocol cap on how many objects one transaction may name as gas payment
/// (`max_gas_payment_objects`). Selecting more is a rejection, not a bigger
/// budget.
const MAX_GAS_PAYMENT_OBJECTS: usize = 256;

/// Where a transaction's gas comes from.
///
/// A wallet's SUI lives in two places now: `Coin<SUI>` objects, and the
/// *address balance* an accumulator holds for it. Faucet drips and plain
/// transfers land in the latter, which no `Coin<SUI>` read can see — a wallet
/// can hold 10 SUI and own no coin at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GasPayment {
    /// Coin objects, largest-first, together covering the budget.
    Coins(Vec<ObjectRef>),
    /// The sender's address balance, spent through an empty gas payment. Costs
    /// a `ValidDuring` expiration (see [`crate::tx::gas_tx_data`]) and forbids
    /// `Argument::GasCoin`, since there is no gas *object* to borrow.
    AddressBalance,
}

/// Pick a gas payment out of what a wallet holds.
///
/// Coin objects win whenever they cover the budget: they need no expiration
/// nonce, they keep `Argument::GasCoin` usable, and they are the only thing
/// mainnet accepts today (address balances are a non-mainnet protocol feature
/// at our pin). Only when the coins fall short do we spend the address
/// balance.
///
/// `coins` must be balance-descending, as [`ChainClient::coins`] returns them,
/// so the fewest possible objects are named.
fn plan_gas(coins: &[CoinRef], address_balance: u64, gas_budget: u64) -> Result<GasPayment> {
    let mut payment = Vec::new();
    let mut covered: u64 = 0;
    for c in coins.iter().take(MAX_GAS_PAYMENT_OBJECTS) {
        payment.push(c.object_ref);
        covered = covered.saturating_add(c.balance);
        if covered >= gas_budget {
            return Ok(GasPayment::Coins(payment));
        }
    }
    if address_balance >= gas_budget {
        return Ok(GasPayment::AddressBalance);
    }
    Err(anyhow!(
        "insufficient SUI for a gas budget of {gas_budget} MIST: \
         {covered} MIST across {} coin object(s) and {address_balance} MIST of address balance",
        payment.len()
    ))
}

/// An owned coin: what the gas selector and the coin-splitting builders
/// need out of a coin read.
#[derive(Debug, Clone, Copy)]
pub struct CoinRef {
    pub object_ref: ObjectRef,
    pub balance: u64,
}

impl CoinRef {
    pub fn object_id(&self) -> ObjectID {
        self.object_ref.0
    }
}

/// `0x2::sui::SUI`.
pub fn sui_coin_type() -> StructTag {
    StructTag {
        address: sui_types::SUI_FRAMEWORK_ADDRESS,
        module: move_core_types::ident_str!("sui").to_owned(),
        name: move_core_types::ident_str!("SUI").to_owned(),
        type_params: vec![],
    }
}

/// Wrap `T` into `0x2::coin::Coin<T>` — `ListOwnedObjects` filters on the
/// object's own type, not the coin's type parameter.
fn coin_wrapper(inner: &StructTag) -> StructTag {
    StructTag {
        address: sui_types::SUI_FRAMEWORK_ADDRESS,
        module: move_core_types::ident_str!("coin").to_owned(),
        name: move_core_types::ident_str!("Coin").to_owned(),
        type_params: vec![sui_types::TypeTag::Struct(Box::new(inner.clone()))],
    }
}

/// Strip everything but scheme+host so an operator override carrying a
/// token in its path never reaches a log line.
fn redact(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or(url)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coin_wrapper_builds_the_object_type_not_the_type_param() {
        let t = coin_wrapper(&sui_coin_type());
        assert_eq!(
            t.to_canonical_string(true),
            "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>"
        );
    }

    fn coin(balance: u64) -> CoinRef {
        CoinRef {
            object_ref: (
                ObjectID::random(),
                sui_types::base_types::SequenceNumber::from_u64(1),
                sui_types::digests::ObjectDigest::random(),
            ),
            balance,
        }
    }

    #[test]
    fn coins_are_preferred_and_only_as_many_as_needed() {
        let coins = [coin(300), coin(200), coin(100)];
        let payment = plan_gas(&coins, 10_000, 400).unwrap();
        // Address balance covers it on its own, but coins keep GasCoin usable
        // and work on mainnet — and the third coin isn't needed.
        assert_eq!(
            payment,
            GasPayment::Coins(vec![coins[0].object_ref, coins[1].object_ref])
        );
    }

    /// The regression this whole path exists for: faucet SUI arrives as an
    /// address balance, so the wallet is rich and owns no coin object.
    #[test]
    fn address_balance_pays_when_there_are_no_coins() {
        assert_eq!(
            plan_gas(&[], 1_000_000, 50_000).unwrap(),
            GasPayment::AddressBalance
        );
    }

    /// The other half: dust coins that individually can't cover the budget.
    /// The old selector took the single largest coin and gave up here.
    #[test]
    fn dust_coins_are_summed_before_falling_back() {
        let coins = [coin(30), coin(30), coin(30)];
        assert_eq!(
            plan_gas(&coins, 0, 90).unwrap(),
            GasPayment::Coins(coins.iter().map(|c| c.object_ref).collect())
        );
        // ...and when even all of them fall short, the address balance does it.
        assert_eq!(
            plan_gas(&coins, 500, 200).unwrap(),
            GasPayment::AddressBalance
        );
    }

    #[test]
    fn short_everywhere_reports_both_sides() {
        let err = plan_gas(&[coin(10)], 5, 100).unwrap_err().to_string();
        assert!(err.contains("10 MIST across 1 coin"), "{err}");
        assert!(err.contains("5 MIST of address balance"), "{err}");
    }

    /// Coins and address balance don't combine: a transaction pays from one or
    /// the other, so "they add up to enough" is not enough.
    #[test]
    fn coins_and_address_balance_do_not_combine() {
        assert!(plan_gas(&[coin(60)], 60, 100).is_err());
    }

    #[test]
    fn redact_keeps_host_only() {
        assert_eq!(redact("https://example.com/secret-token/sui"), "example.com");
        assert_eq!(redact("https://fullnode.testnet.sui.io:443"), "fullnode.testnet.sui.io:443");
    }
}
