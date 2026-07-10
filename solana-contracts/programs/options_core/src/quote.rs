use anchor_lang::prelude::*;
use solana_instructions_sysvar::load_instruction_at_checked;

use crate::error::CoreError;

/// The Ed25519 native signature-verification program.
pub const ED25519_PROGRAM_ID: Pubkey = solana_sdk_ids::ed25519_program::ID;

/// The structured payload signed by the MM's hot key — the port of
/// `quote::Quote`. Canonical bytes are the Borsh (AnchorSerialize)
/// encoding, replacing Sui's BCS; field order is frozen and mirrors the
/// Move struct (IDs/addresses become 32-byte pubkeys, so the layout is
/// nearly identical). The off-chain signer must produce exactly these
/// bytes — lock with golden vectors against the quoting service.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    /// Domain separator: the protocol's `Config` PDA address (Sui derived
    /// its `protocol_id` from the AdminCap object id).
    pub protocol_id: Pubkey,
    /// The signing MM's `MmAccount` address.
    pub signer_account: Pubkey,
    /// Wallet that receives the signer's minted tokens: the call/put coins
    /// in writer flow, the `Position` in trader flow.
    pub signer_token_recipient: Pubkey,
    pub bucket: Pubkey,
    pub write_amount: u64,
    /// Gross premium in settlement smallest-units.
    pub premium: u64,
    pub valid_until_ms: u64,
    pub nonce: u64,
}

/// Which side the quote's signer is on — the port of `bucket::FlowKind`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowKind {
    /// Executor writes (provides collateral); the signer is the trader MM
    /// buying the option, paying premium from their MM account.
    Writer,
    /// Executor buys (provides premium); the signer is the writer MM
    /// selling the option, collateral debited from their MM account.
    Trader,
}

/// Solana can't verify Ed25519 in-program; the transaction carries a
/// native Ed25519SigVerify instruction and we introspect it via the
/// Instructions sysvar — the standard precompile pattern. The runtime has
/// already verified the signature when this executes; our job is to pin
/// WHAT was verified: exactly one signature, self-contained data (all
/// instruction indices == u16::MAX, so offsets can't point into other
/// instructions), the MM's registered pubkey, and the canonical quote
/// bytes as the message.
pub fn verify_ed25519_quote_ix(
    instructions_sysvar: &AccountInfo,
    sig_ix_index: u8,
    expected_pubkey: &[u8],
    expected_msg: &[u8],
) -> Result<()> {
    let ix = load_instruction_at_checked(sig_ix_index as usize, instructions_sysvar)
        .map_err(|_| CoreError::MissingSigVerification)?;
    require!(
        ix.program_id == ED25519_PROGRAM_ID,
        CoreError::MissingSigVerification
    );
    let data = ix.data;
    // Header: [num_signatures: u8, padding: u8], then one 14-byte
    // Ed25519SignatureOffsets record.
    require!(data.len() >= 16, CoreError::MissingSigVerification);
    require!(data[0] == 1, CoreError::MissingSigVerification);

    let off = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]);
    let signature_instruction_index = off(4);
    let public_key_offset = off(6) as usize;
    let public_key_instruction_index = off(8);
    let message_data_offset = off(10) as usize;
    let message_data_size = off(12) as usize;
    let message_instruction_index = off(14);

    // Self-contained only: every referenced buffer lives in the verify
    // instruction itself.
    require!(
        signature_instruction_index == u16::MAX
            && public_key_instruction_index == u16::MAX
            && message_instruction_index == u16::MAX,
        CoreError::MissingSigVerification
    );

    let pubkey = data
        .get(public_key_offset..public_key_offset + 32)
        .ok_or(CoreError::MissingSigVerification)?;
    require!(pubkey == expected_pubkey, CoreError::QuoteSignatureInvalid);

    let msg = data
        .get(message_data_offset..message_data_offset + message_data_size)
        .ok_or(CoreError::MissingSigVerification)?;
    require!(msg == expected_msg, CoreError::QuoteSignatureInvalid);

    Ok(())
}
