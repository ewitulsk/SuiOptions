//! Pyth pull-oracle receiver helpers for the keeper — posting
//! `PriceUpdateV2` accounts that options_vault's oracle-gated cranks read.
//!
//! Status: **real, but hand-encoded instructions.** The receiver *program*
//! crate (`pyth-solana-receiver`) is not published on crates.io, so the two
//! instructions we need — `post_update_atomic` (single-tx guardian-subset
//! verification) and `reclaim_rent` — are encoded here from the receiver's
//! source/IDL: Anchor `global:<name>` discriminators plus the Borsh params
//! type `PostUpdateAtomicParams`, which IS published (in
//! `pyth-solana-receiver-sdk`, anchor-1.x compatible, same crate the
//! programs' consumers use for `PriceUpdateV2`). Account order/flags are
//! locked by unit tests below against the receiver's `PostUpdateAtomic` /
//! `ReclaimRent` accounts structs.
//!
//! Flow (guide 09): fetch Hermes update bytes (pyth-client crate, caller's
//! job) → [`parse_accumulator_update`] → per feed, [`post_update_atomic_ix`]
//! writing into a caller-provided update-account keypair (long-lived and
//! reused: `init_if_needed` + `write_authority` lets the same keypair be
//! overwritten each refresh) → crank reads it → [`reclaim_rent_ix`] if the
//! account is ever retired.

use anyhow::{anyhow, Context, Result};
use borsh::BorshSerialize;
use sha2::{Digest, Sha256};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

pub use pyth_solana_receiver_sdk::price_update::PriceUpdateV2;
pub use pyth_solana_receiver_sdk::PostUpdateAtomicParams;
pub use pythnet_sdk::wire::v1::MerklePriceUpdate;

/// The Pyth receiver program (`rec5…`, same address on mainnet + devnet).
pub const PYTH_RECEIVER_ID: Pubkey = pyth_solana_receiver_sdk::ID;

/// The Pyth-operated Wormhole verification bridge that owns the guardian
/// set accounts the receiver checks signatures against (same address on
/// mainnet + devnet). The receiver's on-chain `Config` pins it; pass a
/// different id if the config is ever migrated.
pub const WORMHOLE_RECEIVER_ID: Pubkey =
    anchor_lang::pubkey!("HDwcJBJXjL9FpJ7UBsYBtaDjsBUhuLCUYoz3zr8SWWaQ");

/// Anchor's `sighash("global", name)` instruction discriminator.
fn anchor_discriminator(name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("global:{name}").as_bytes());
    digest[..8].try_into().unwrap()
}

/// Split a Hermes accumulator-update blob (the `update.data` bytes from
/// `latest_with_update_data`) into the Wormhole VAA and the per-feed
/// Merkle price updates it carries.
pub fn parse_accumulator_update(bytes: &[u8]) -> Result<(Vec<u8>, Vec<MerklePriceUpdate>)> {
    let update = pythnet_sdk::wire::v1::AccumulatorUpdateData::try_from_slice(bytes)
        .map_err(|e| anyhow!("parsing accumulator update data: {e:?}"))?;
    let pythnet_sdk::wire::v1::Proof::WormholeMerkle { vaa, updates } = update.proof;
    Ok((Vec::<u8>::from(vaa), updates))
}

/// The guardian-set index a VAA was signed under (header: `version: u8`
/// then `guardian_set_index: u32` big-endian).
pub fn vaa_guardian_set_index(vaa: &[u8]) -> Result<u32> {
    let bytes: [u8; 4] = vaa
        .get(1..5)
        .context("VAA shorter than its 5-byte header")?
        .try_into()
        .unwrap();
    Ok(u32::from_be_bytes(bytes))
}

/// The Wormhole guardian-set account for `index` under the verification
/// bridge program.
pub fn guardian_set_pda(wormhole_program: &Pubkey, index: u32) -> Pubkey {
    Pubkey::find_program_address(&[b"GuardianSet", &index.to_be_bytes()], wormhole_program).0
}

/// `post_update_atomic`: verify a guardian subset in-transaction and write
/// one feed's `PriceUpdateV2` into `price_update_account` — a keypair the
/// caller owns and co-signs. Reposting to the same account (same
/// `write_authority`) overwrites in place — no rent churn.
///
/// `treasury_id` selects one of the receiver's 256 fee treasuries (any
/// value; spread the write load).
pub fn post_update_atomic_ix(
    payer: &Pubkey,
    price_update_account: &Pubkey,
    write_authority: &Pubkey,
    wormhole_program: &Pubkey,
    vaa: Vec<u8>,
    merkle_price_update: MerklePriceUpdate,
    treasury_id: u8,
) -> Result<Instruction> {
    let guardian_set = guardian_set_pda(wormhole_program, vaa_guardian_set_index(&vaa)?);
    let params = PostUpdateAtomicParams {
        vaa,
        merkle_price_update,
        treasury_id,
    };
    let mut data = anchor_discriminator("post_update_atomic").to_vec();
    params
        .serialize(&mut data)
        .context("borsh-serializing PostUpdateAtomicParams")?;

    // Account order/flags mirror the receiver's `PostUpdateAtomic` struct.
    let accounts = vec![
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(guardian_set, false),
        AccountMeta::new_readonly(pyth_solana_receiver_sdk::pda::get_config_address(), false),
        AccountMeta::new(
            pyth_solana_receiver_sdk::pda::get_treasury_address(treasury_id),
            false,
        ),
        // init_if_needed keypair account: must co-sign.
        AccountMeta::new(*price_update_account, true),
        AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
        AccountMeta::new_readonly(*write_authority, true),
    ];
    Ok(Instruction {
        program_id: PYTH_RECEIVER_ID,
        accounts,
        data,
    })
}

/// `reclaim_rent`: close a `PriceUpdateV2` account, rent back to `payer`
/// (which must be its `write_authority`).
pub fn reclaim_rent_ix(payer: &Pubkey, price_update_account: &Pubkey) -> Instruction {
    Instruction {
        program_id: PYTH_RECEIVER_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*price_update_account, false),
        ],
        data: anchor_discriminator("reclaim_rent").to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pythnet_sdk::accumulators::merkle::MerklePath;

    #[test]
    fn receiver_id_matches_vault_pin() {
        // options_vault hardcodes the receiver as the PriceUpdateV2 owner;
        // drift here would post updates the vault refuses to read.
        assert_eq!(PYTH_RECEIVER_ID, options_vault::oracle::PYTH_RECEIVER_ID);
        // …and manually parses the account, pinning its discriminator.
        use anchor_lang::Discriminator as _;
        assert_eq!(
            PriceUpdateV2::DISCRIMINATOR,
            options_vault::oracle::PRICE_UPDATE_V2_DISCRIMINATOR
        );
    }

    #[test]
    fn guardian_set_index_reads_big_endian_header() {
        let mut vaa = vec![1u8]; // version
        vaa.extend_from_slice(&7u32.to_be_bytes());
        vaa.extend_from_slice(&[0u8; 16]);
        assert_eq!(vaa_guardian_set_index(&vaa).unwrap(), 7);
        assert!(vaa_guardian_set_index(&[1, 2]).is_err());
    }

    #[test]
    fn post_update_atomic_shape_matches_receiver() {
        let payer = Pubkey::new_unique();
        let update_acc = Pubkey::new_unique();
        let mut vaa = vec![1u8];
        vaa.extend_from_slice(&5u32.to_be_bytes());
        vaa.extend_from_slice(&[0u8; 32]);
        let merkle = MerklePriceUpdate {
            message: vec![1, 2, 3].into(),
            proof: MerklePath::new(vec![[9u8; 20]]),
        };
        let ix = post_update_atomic_ix(
            &payer,
            &update_acc,
            &payer,
            &WORMHOLE_RECEIVER_ID,
            vaa.clone(),
            merkle.clone(),
            3,
        )
        .unwrap();

        assert_eq!(ix.program_id, PYTH_RECEIVER_ID);
        // Discriminator, then the Borsh params.
        assert_eq!(&ix.data[..8], &anchor_discriminator("post_update_atomic"));
        let params = PostUpdateAtomicParams {
            vaa,
            merkle_price_update: merkle,
            treasury_id: 3,
        };
        let mut expected = Vec::new();
        params.serialize(&mut expected).unwrap();
        assert_eq!(&ix.data[8..], expected.as_slice());

        // payer, guardian_set, config, treasury, update account, system
        // program, write authority — exactly the receiver's struct order.
        assert_eq!(ix.accounts.len(), 7);
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
        assert_eq!(
            ix.accounts[1].pubkey,
            guardian_set_pda(&WORMHOLE_RECEIVER_ID, 5)
        );
        assert!(!ix.accounts[1].is_writable);
        assert_eq!(
            ix.accounts[2].pubkey,
            pyth_solana_receiver_sdk::pda::get_config_address()
        );
        assert_eq!(
            ix.accounts[3].pubkey,
            pyth_solana_receiver_sdk::pda::get_treasury_address(3)
        );
        assert!(ix.accounts[3].is_writable);
        assert_eq!(ix.accounts[4].pubkey, update_acc);
        assert!(ix.accounts[4].is_signer && ix.accounts[4].is_writable);
        assert_eq!(ix.accounts[5].pubkey, anchor_lang::system_program::ID);
        assert_eq!(ix.accounts[6].pubkey, payer);
        assert!(ix.accounts[6].is_signer && !ix.accounts[6].is_writable);
    }

    #[test]
    fn reclaim_rent_shape() {
        let payer = Pubkey::new_unique();
        let acc = Pubkey::new_unique();
        let ix = reclaim_rent_ix(&payer, &acc);
        assert_eq!(ix.program_id, PYTH_RECEIVER_ID);
        assert_eq!(ix.data, anchor_discriminator("reclaim_rent").to_vec());
        assert_eq!(ix.accounts.len(), 2);
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
        assert!(!ix.accounts[1].is_signer && ix.accounts[1].is_writable);
    }
}
