//! Destination-Sui mint: the 5-call receive PTB from Circle's sui-cctp
//! example (receive_message → handle_receive_message<USDC> →
//! deconstruct → stamp_receipt → complete_receive_message), signed with the
//! service's Sui key.

use std::str::FromStr;

use anyhow::{Context, Result};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Argument, Command, ProgrammableMoveCall};
use sui_types::TypeTag;

use sui_tx::sui_client::Signer;
use sui_tx::tx::{shared_object_arg, submit_ptb};

use crate::config::SuiConfig;

pub struct SuiMinter {
    pub client: SuiClient,
    pub signer: Signer,
    pub cfg: SuiConfig,
}

impl SuiMinter {
    /// Submits the receive PTB; returns the tx digest.
    pub async fn mint(&self, message: &[u8], attestation: &[u8]) -> Result<String> {
        let mt_pkg = ObjectID::from_str(&self.cfg.message_transmitter_package)
            .context("bad message_transmitter_package")?;
        let tmm_pkg = ObjectID::from_str(&self.cfg.token_messenger_minter_package)
            .context("bad token_messenger_minter_package")?;
        let usdc: TypeTag =
            TypeTag::from_str(&self.cfg.usdc_coin_type).context("bad usdc_coin_type")?;
        let authenticator: TypeTag = TypeTag::from_str(&format!(
            "{}::message_transmitter_authenticator::MessageTransmitterAuthenticator",
            self.cfg.token_messenger_minter_package
        ))
        .context("bad authenticator type")?;

        let mut pt = ProgrammableTransactionBuilder::new();

        let mt_state = pt.obj(
            shared_object_arg(
                &self.client,
                ObjectID::from_str(&self.cfg.message_transmitter_state)?,
                true,
            )
            .await?,
        )?;
        let tmm_state = pt.obj(
            shared_object_arg(
                &self.client,
                ObjectID::from_str(&self.cfg.token_messenger_minter_state)?,
                true,
            )
            .await?,
        )?;
        let deny_list = pt.obj(
            shared_object_arg(&self.client, ObjectID::from_str("0x403")?, false).await?,
        )?;
        let treasury = pt.obj(
            shared_object_arg(&self.client, ObjectID::from_str(&self.cfg.usdc_treasury)?, true)
                .await?,
        )?;

        let message_arg = pt.pure(message.to_vec())?;
        let attestation_arg = pt.pure(attestation.to_vec())?;

        let call = |pkg, module: &str, fun: &str, type_args: Vec<TypeTag>, args| {
            Command::MoveCall(Box::new(ProgrammableMoveCall {
                package: pkg,
                module: module.to_string(),
                function: fun.to_string(),
                type_arguments: type_args.into_iter().map(Into::into).collect(),
                arguments: args,
            }))
        };

        // 0: receipt = message_transmitter::receive_message(message, attestation, &mut mt_state)
        pt.command(call(
            mt_pkg,
            "receive_message",
            "receive_message",
            vec![],
            vec![message_arg, attestation_arg, mt_state],
        ));
        // 1: ticket_with_burn = token_messenger_minter::handle_receive_message<USDC>(receipt, ...)
        pt.command(call(
            tmm_pkg,
            "handle_receive_message",
            "handle_receive_message",
            vec![usdc],
            vec![Argument::Result(0), tmm_state, deny_list, treasury],
        ));
        // 2: (stamp_ticket, _burn_message) = deconstruct_stamp_receipt_ticket_with_burn_message(..)
        pt.command(call(
            tmm_pkg,
            "handle_receive_message",
            "deconstruct_stamp_receipt_ticket_with_burn_message",
            vec![],
            vec![Argument::Result(1)],
        ));
        // 3: stamped = message_transmitter::stamp_receipt<Authenticator>(stamp_ticket, &mt_state)
        pt.command(call(
            mt_pkg,
            "receive_message",
            "stamp_receipt",
            vec![authenticator],
            vec![Argument::NestedResult(2, 0), mt_state],
        ));
        // 4: complete_receive_message(stamped, &mt_state)
        pt.command(call(
            mt_pkg,
            "receive_message",
            "complete_receive_message",
            vec![],
            vec![Argument::Result(3), mt_state],
        ));

        let resp = submit_ptb(
            &self.client,
            &self.signer,
            pt,
            self.cfg.gas_budget,
            "cctp-receive",
        )
        .await?;
        Ok(resp.digest.to_string())
    }

    /// On-chain timestamp of a Sui tx (ms since epoch), if finalized.
    pub async fn tx_timestamp_ms(&self, digest: &str) -> Result<Option<u64>> {
        use sui_json_rpc_types::SuiTransactionBlockResponseOptions;
        use sui_types::digests::TransactionDigest;
        let digest = TransactionDigest::from_str(digest).context("bad tx digest")?;
        let resp = self
            .client
            .read_api()
            .get_transaction_with_options(digest, SuiTransactionBlockResponseOptions::new())
            .await;
        match resp {
            Ok(r) => Ok(r.timestamp_ms),
            Err(_) => Ok(None),
        }
    }
}
