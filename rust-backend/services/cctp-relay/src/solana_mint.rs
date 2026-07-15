//! Destination-Solana mint: MessageTransmitter `receiveMessage` with the
//! account list from Circle's solana-cctp-contracts example, plus an
//! idempotent ATA create for the recipient, fee-paid and signed by the
//! service's Solana key.
//!
//! Circle's v1 programs are Anchor 0.28, so instructions are built manually
//! (discriminator + borsh params).

use anyhow::{anyhow, bail, Context, Result};
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_signer::Signer as _;
use solana_transaction::Transaction;

use crate::message::CctpMessage;
use crate::solana_rpc::SolanaRpc;

/// Circle CCTP v1 program ids (devnet + mainnet).
pub const TOKEN_MESSENGER_MINTER: &str = "CCTPiPYPc6AsJuwueEnWgSgucamXDZwBd53dQ11YiKX3";
pub const MESSAGE_TRANSMITTER: &str = "CCTPmbSD7gX1bxKPAmg77w8oFzNFpaQiQUWD43TKaecd";

const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

/// sha256("global:receive_message")[..8] — Circle's Anchor 0.28 discriminator.
const RECEIVE_MESSAGE_DISCRIMINATOR: [u8; 8] = [38, 144, 127, 225, 31, 225, 238, 25];

/// UsedNonces bucket size (message-transmitter state.rs).
const MAX_NONCES: u64 = 6400;

pub struct SolanaMinter {
    pub rpc: SolanaRpc,
    pub keypair: Keypair,
    pub usdc_mint: Pubkey,
}

/// Parse a Solana keypair from either a base58-encoded 64-byte secret or a
/// JSON byte array (solana-cli id.json format).
pub fn parse_keypair(raw: &str) -> Result<Keypair> {
    let raw = raw.trim();
    let bytes: Vec<u8> = if raw.starts_with('[') {
        serde_json::from_str(raw).context("parsing solana key JSON array")?
    } else {
        bs58::decode(raw).into_vec().context("decoding solana key base58")?
    };
    Keypair::try_from(bytes.as_slice()).map_err(|e| anyhow!("bad solana keypair: {e}"))
}

/// Derive the associated token account for `owner`.
pub fn derive_ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let token_program: Pubkey = TOKEN_PROGRAM.parse().unwrap();
    let ata_program: Pubkey = ATA_PROGRAM.parse().unwrap();
    Pubkey::derive_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    )
    .expect("ATA derivation")
    .0
}

impl SolanaMinter {
    pub fn address(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    /// Build, sign, and submit the mint tx; returns the signature.
    ///
    /// `destination_wallet` (when known) lets us create the recipient's ATA
    /// idempotently in the same tx; without it the recipient token account
    /// must already exist.
    pub async fn mint(
        &self,
        raw_message: &[u8],
        attestation: &[u8],
        decoded: &CctpMessage,
        destination_wallet: Option<&str>,
    ) -> Result<String> {
        let mt: Pubkey = MESSAGE_TRANSMITTER.parse().unwrap();
        let tmm: Pubkey = TOKEN_MESSENGER_MINTER.parse().unwrap();
        let token_program: Pubkey = TOKEN_PROGRAM.parse().unwrap();
        let system_program: Pubkey = SYSTEM_PROGRAM.parse().unwrap();

        let recipient_token_account = Pubkey::new_from_array(decoded.burn.mint_recipient);
        let src_domain = decoded.source_domain.to_string();

        let mut instructions: Vec<Instruction> = Vec::new();

        // Idempotent ATA create when we know the owner wallet. Sanity-check
        // that the derived ATA matches the message's mint recipient so we
        // never mint into an account we mis-derived.
        if let Some(wallet) = destination_wallet {
            let owner: Pubkey = wallet.parse().context("bad destination_wallet")?;
            let ata = derive_ata(&owner, &self.usdc_mint);
            if ata != recipient_token_account {
                bail!(
                    "mint recipient {} is not the USDC ATA of destination wallet {} (expected {})",
                    recipient_token_account,
                    wallet,
                    ata
                );
            }
            instructions.push(Instruction {
                program_id: ATA_PROGRAM.parse().unwrap(),
                accounts: vec![
                    AccountMeta::new(self.keypair.pubkey(), true),
                    AccountMeta::new(ata, false),
                    AccountMeta::new_readonly(owner, false),
                    AccountMeta::new_readonly(self.usdc_mint, false),
                    AccountMeta::new_readonly(system_program, false),
                    AccountMeta::new_readonly(token_program, false),
                ],
                data: vec![1], // CreateIdempotent
            });
        } else if !self
            .rpc
            .account_exists(&recipient_token_account.to_string())
            .await?
        {
            bail!(
                "recipient token account {} does not exist and destination wallet is unknown",
                recipient_token_account
            );
        }

        // PDAs (per Circle's examples/utils.ts).
        fn pda<const N: usize>(seeds: &[&[u8]; N], program: &Pubkey) -> Pubkey {
            Pubkey::derive_program_address(seeds, program)
                .expect("pda derivation")
                .0
        }
        let authority_pda = pda(&[b"message_transmitter_authority", tmm.as_ref()], &mt);
        let message_transmitter = pda(&[b"message_transmitter"], &mt);
        let first_nonce = ((decoded.nonce - 1) / MAX_NONCES) * MAX_NONCES + 1;
        // Domains < 11 use no seed delimiter (message-transmitter state.rs).
        let used_nonces = if decoded.source_domain < 11 {
            pda(
                &[b"used_nonces", src_domain.as_bytes(), first_nonce.to_string().as_bytes()],
                &mt,
            )
        } else {
            pda(
                &[b"used_nonces", src_domain.as_bytes(), b"-", first_nonce.to_string().as_bytes()],
                &mt,
            )
        };
        let mt_event_authority = pda(&[b"__event_authority"], &mt);
        let token_messenger = pda(&[b"token_messenger"], &tmm);
        let remote_token_messenger = pda(&[b"remote_token_messenger", src_domain.as_bytes()], &tmm);
        let token_minter = pda(&[b"token_minter"], &tmm);
        let local_token = pda(&[b"local_token", self.usdc_mint.as_ref()], &tmm);
        let token_pair = pda(
            &[b"token_pair", src_domain.as_bytes(), &decoded.burn.burn_token],
            &tmm,
        );
        let custody = pda(&[b"custody", self.usdc_mint.as_ref()], &tmm);
        let tmm_event_authority = pda(&[b"__event_authority"], &tmm);

        // Borsh ReceiveMessageParams { message: Vec<u8>, attestation: Vec<u8> }.
        let mut data =
            Vec::with_capacity(8 + 4 + raw_message.len() + 4 + attestation.len());
        data.extend_from_slice(&RECEIVE_MESSAGE_DISCRIMINATOR);
        data.extend_from_slice(&(raw_message.len() as u32).to_le_bytes());
        data.extend_from_slice(raw_message);
        data.extend_from_slice(&(attestation.len() as u32).to_le_bytes());
        data.extend_from_slice(attestation);

        instructions.push(Instruction {
            program_id: mt,
            accounts: vec![
                // Declared accounts of ReceiveMessageContext…
                AccountMeta::new(self.keypair.pubkey(), true), // payer
                AccountMeta::new_readonly(self.keypair.pubkey(), true), // caller
                AccountMeta::new_readonly(authority_pda, false),
                AccountMeta::new_readonly(message_transmitter, false),
                AccountMeta::new(used_nonces, false),
                AccountMeta::new_readonly(tmm, false), // receiver
                AccountMeta::new_readonly(system_program, false),
                // …the Anchor 0.28 event-CPI pair…
                AccountMeta::new_readonly(mt_event_authority, false),
                AccountMeta::new_readonly(mt, false),
                // …then remaining accounts for TokenMessengerMinter's handler.
                AccountMeta::new_readonly(token_messenger, false),
                AccountMeta::new_readonly(remote_token_messenger, false),
                AccountMeta::new(token_minter, false),
                AccountMeta::new(local_token, false),
                AccountMeta::new_readonly(token_pair, false),
                AccountMeta::new(recipient_token_account, false),
                AccountMeta::new(custody, false),
                AccountMeta::new_readonly(token_program, false),
                AccountMeta::new_readonly(tmm_event_authority, false),
                AccountMeta::new_readonly(tmm, false),
            ],
            data,
        });

        let blockhash: solana_hash::Hash = self
            .rpc
            .latest_blockhash()
            .await?
            .parse()
            .map_err(|e| anyhow!("bad blockhash: {e:?}"))?;
        let message = Message::new_with_blockhash(
            &instructions,
            Some(&self.keypair.pubkey()),
            &blockhash,
        );
        let mut tx = Transaction::new_unsigned(message);
        tx.try_sign(&[&self.keypair], blockhash)
            .map_err(|e| anyhow!("signing solana tx: {e}"))?;

        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(bincode::serialize(&tx).context("serializing solana tx")?);
        self.rpc.send_transaction(&encoded).await
    }
}
