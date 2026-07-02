//! Sui destination submitter (the EVM → Sui direction).
//!
//! Move has no dynamic dispatch, so the Inbox can't call an arbitrary app; the
//! app must be the entry point (relayer-dispatch-design). This submitter is
//! *generic* over any app that follows the standard `bridge_receive` convention:
//! it reads the `dst_app` object's type on chain to learn `(package, module,
//! type args)`, then builds a single `MoveCall` to `PKG::MODULE::bridge_receive`
//! passing the L1 shared objects + the message/envelope as BCS `vector<u8>`
//! args. No per-app relayer configuration.
//!
//! [`parse_dispatch`] is the pure, unit-tested core (type-string → call target);
//! [`SuiDestSubmitter`] wraps it with the on-chain read + PTB submission.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use bridge_types::{Bytes32, CrossChainMessage, SignatureEnvelope};
use sui_json_rpc_types::SuiObjectDataOptions;
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::{Identifier, TypeTag};

use sui_tx::sui_client::Signer;
use sui_tx::tx::{shared_object_arg, submit_ptb};

use crate::relay::DestSubmitter;

/// The standard convention entry every Locker-style app exposes.
pub const RECEIVE_FUNCTION: &str = "bridge_receive";
/// The Sui system `Clock` object id (0x6), a `bridge_receive` argument.
pub const CLOCK_OBJECT_ID: &str = "0x0000000000000000000000000000000000000000000000000000000000000006";

/// The MoveCall target derived from a `dst_app` object's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatch {
    /// Defining package, e.g. `0xPKG`.
    pub package: String,
    /// Module, e.g. `locker`.
    pub module: String,
    /// Type arguments (the app's generic params), e.g. the wrapped coin type.
    pub type_args: Vec<String>,
}

/// Derive the `bridge_receive` call target from a Sui object type string like
/// `0xPKG::locker::Locker<0xTOK::coin::COIN>`. The package + module come from the
/// object's own type, the type args are its generics — so a standard app needs
/// zero relayer config (its on-chain type *is* the dispatch descriptor).
pub fn parse_dispatch(object_type: &str) -> Result<Dispatch> {
    let s = object_type.trim();
    // Split off the optional generic parameter list.
    let (base, type_args) = match s.find('<') {
        Some(open) => {
            if !s.ends_with('>') {
                bail!("malformed object type (unbalanced generics): {s}");
            }
            (&s[..open], split_top_level(&s[open + 1..s.len() - 1])?)
        }
        None => (s, Vec::new()),
    };

    let parts: Vec<&str> = base.split("::").collect();
    if parts.len() != 3 {
        bail!("expected `PKG::module::Struct`, got {base:?}");
    }
    let package = parts[0].to_string();
    if !package.starts_with("0x") || package.len() < 3 {
        bail!("bad package address in type: {package:?}");
    }
    Ok(Dispatch { package, module: parts[1].to_string(), type_args })
}

/// Generic Sui destination submitter. Resolves the call target from the
/// `dst_app` object's on-chain type, then submits a single `bridge_receive`
/// MoveCall. Works for any app following the convention with zero per-app config.
pub struct SuiDestSubmitter {
    client: SuiClient,
    signer: Signer,
    inbox_id: ObjectID,
    keys_id: ObjectID,
    gas_budget: u64,
}

impl SuiDestSubmitter {
    pub async fn connect(
        rpc_url: &str,
        sui_key: &str,
        inbox_id: &str,
        keys_id: &str,
        gas_budget: u64,
    ) -> Result<Self> {
        let client = SuiClientBuilder::default()
            .build(rpc_url)
            .await
            .context("building Sui client for the destination submitter")?;
        Ok(Self {
            client,
            signer: Signer::from_string(sui_key).context("loading relayer Sui key")?,
            inbox_id: inbox_id.parse().context("parsing inbox object id")?,
            keys_id: keys_id.parse().context("parsing group-key registry id")?,
            gas_budget,
        })
    }

    /// Read `dst_app`'s type on chain and derive its `bridge_receive` target.
    async fn resolve(&self, dst_app: ObjectID) -> Result<Dispatch> {
        let resp = self
            .client
            .read_api()
            .get_object_with_options(dst_app, SuiObjectDataOptions::new().with_type())
            .await
            .context("reading dst_app object type")?;
        let ty = resp
            .data
            .and_then(|d| d.type_)
            .ok_or_else(|| anyhow!("dst_app {dst_app} has no type (not an object?)"))?;
        parse_dispatch(&ty.to_string())
    }
}

#[async_trait]
impl DestSubmitter for SuiDestSubmitter {
    /// Best-effort: the on-chain `consumed` set in `inbox::receive` is the real
    /// exactly-once guard (a re-delivery simply aborts). A devInspect-based
    /// `is_consumed` check to pre-skip re-deliveries is a follow-up; returning
    /// false here is always correct, just not gas-optimal on a delivery race.
    async fn is_delivered(&self, _digest: &Bytes32) -> Result<bool> {
        Ok(false)
    }

    async fn submit(&self, message: &CrossChainMessage, envelope: &SignatureEnvelope) -> Result<()> {
        let dst_app = ObjectID::from_bytes(message.dst_app)
            .map_err(|e| anyhow!("dst_app is not a valid object id: {e}"))?;
        let dispatch = self.resolve(dst_app).await?;

        let package: ObjectID = dispatch.package.parse().context("parsing dispatch package id")?;
        let module = Identifier::new(dispatch.module).context("invalid module identifier")?;
        let function = Identifier::new(RECEIVE_FUNCTION).expect("valid identifier");
        let type_args: Vec<TypeTag> = dispatch
            .type_args
            .iter()
            .map(|t| sui_types::parse_sui_type_tag(t).with_context(|| format!("parsing type arg {t}")))
            .collect::<Result<_>>()?;

        // Resolve shared-object args (initial_shared_version fetched per read).
        let inbox = shared_object_arg(&self.client, self.inbox_id, true).await?;
        let keys = shared_object_arg(&self.client, self.keys_id, false).await?;
        let app = shared_object_arg(&self.client, dst_app, true).await?;
        let clock = shared_object_arg(&self.client, CLOCK_OBJECT_ID.parse().unwrap(), false).await?;

        let mut pt = ProgrammableTransactionBuilder::new();
        let a_inbox = pt.obj(inbox)?;
        let a_keys = pt.obj(keys)?;
        let a_app = pt.obj(app)?;
        // message/envelope as plain `vector<u8>` pure args (decoded via from_bcs).
        let a_msg = pt.pure(message.to_move_bcs())?;
        let a_env = pt.pure(envelope.to_move_bcs())?;
        let a_clock = pt.obj(clock)?;
        pt.programmable_move_call(
            package,
            module,
            function,
            type_args,
            vec![a_inbox, a_keys, a_app, a_msg, a_env, a_clock],
        );

        submit_ptb(&self.client, &self.signer, pt, self.gas_budget, "bridge_receive").await?;
        Ok(())
    }
}

/// Split a generic argument list on top-level commas (respecting nested `<>`).
fn split_top_level(inner: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in inner.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                args.push(inner[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(anyhow!("unbalanced generics in type args: {inner}"));
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        args.push(last.to_string());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_generic_locker_type() {
        let d = parse_dispatch(
            "0xabc::locker::Locker<0xdef::wtbtc::WTBTC>",
        )
        .unwrap();
        assert_eq!(d.package, "0xabc");
        assert_eq!(d.module, "locker");
        assert_eq!(d.type_args, vec!["0xdef::wtbtc::WTBTC".to_string()]);
    }

    #[test]
    fn parses_nested_generics() {
        let d = parse_dispatch("0x1::m::S<0x2::a::A<0x3::b::B>, 0x4::c::C>").unwrap();
        assert_eq!(d.type_args, vec!["0x2::a::A<0x3::b::B>".to_string(), "0x4::c::C".to_string()]);
    }

    #[test]
    fn parses_non_generic_type() {
        let d = parse_dispatch("0xabc::locker::Locker").unwrap();
        assert_eq!(d.module, "locker");
        assert!(d.type_args.is_empty());
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_dispatch("0xabc::locker").is_err()); // too few segments
        assert!(parse_dispatch("notaddr::m::S").is_err()); // no 0x
        assert!(parse_dispatch("0x1::m::S<0x2::a::A").is_err()); // unbalanced
    }
}
