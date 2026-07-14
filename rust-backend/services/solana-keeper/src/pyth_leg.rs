//! The Pyth leg — replaces the Sui keeper's in-PTB VAA prepend. Before
//! each oracle-gated crank, the keeper fetches the freshest Hermes
//! accumulator update for the vault's two pinned feeds and posts each
//! feed's `PriceUpdateV2` via the receiver's `post_update_atomic`
//! (guardian-subset verification in-transaction), then sends the crank
//! referencing those accounts.
//!
//! ## Update-account strategy (the doc-09 "Decision", as `solana-tx`
//! actually supports it)
//!
//! `solana_tx::pyth::post_update_atomic_ix` builds the receiver's
//! `init_if_needed` flow: the update account is a **keypair the keeper
//! owns and co-signs**, with `write_authority` = the keeper wallet.
//! Reposting to the same keypair overwrites the account in place — no
//! create/close rent churn. So the keeper maintains **one persistent
//! update account per feed**: the keypair is generated (in-memory) the
//! first time a feed is needed, the account itself is created by the
//! receiver on the first post, and every later post reuses it. Restarts
//! generate fresh keypairs and simply strand ~0.007 SOL of rent per feed
//! (reclaimable via `reclaim_rent` by nobody but the old key — accepted;
//! keeper restarts are rare and the alternative is persisting keys).
//!
//! ## Tx packing
//!
//! post×2 + crank go in ONE transaction when the serialized size fits
//! Solana's 1232-byte packet limit; a real 13-signature mainnet VAA makes
//! each post ~1.1 KB, so in practice this splits into post-tx(s) followed
//! by the crank tx — the accounts persist between transactions, and the
//! vault's staleness window (`max_price_age_secs`) dwarfs the extra slot.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;

use pyth_client::types::PriceFeedId;
use solana_tx::pyth::{parse_accumulator_update, post_update_atomic_ix, MerklePriceUpdate};
use solana_tx::SolanaClientWrapper;

/// Solana's packet-size ceiling for a serialized transaction.
pub const MAX_TX_BYTES: usize = 1232;

/// One post instruction plus the update-account keypair that must co-sign
/// the transaction carrying it.
pub struct PostLeg {
    pub feed: PriceFeedId,
    pub ix: Instruction,
    pub update_keypair: Keypair,
}

/// Owns the per-feed persistent update accounts and builds post
/// instructions from fresh Hermes data.
pub struct PythPoster {
    pub hermes_url: String,
    wormhole_program: Pubkey,
    payer: Pubkey,
    accounts: HashMap<PriceFeedId, Keypair>,
}

impl PythPoster {
    pub fn new(hermes_url: String, wormhole_program: Pubkey, payer: Pubkey) -> Self {
        Self { hermes_url, wormhole_program, payer, accounts: HashMap::new() }
    }

    /// The persistent update account for `feed` (keypair generated on
    /// first use; the account is created on chain by the first post).
    pub fn price_account(&mut self, feed: PriceFeedId) -> Pubkey {
        self.accounts.entry(feed).or_insert_with(Keypair::new).pubkey()
    }

    /// Fetch the latest Hermes accumulator update covering `feeds` and
    /// build one `post_update_atomic` per feed, targeting the per-feed
    /// persistent accounts. Errors if any requested feed is missing from
    /// the response (classified Retry upstream — Hermes hiccup).
    pub async fn post_legs(
        &mut self,
        http: &reqwest::Client,
        feeds: &[PriceFeedId],
    ) -> Result<Vec<PostLeg>> {
        let (payloads, _parsed) =
            pyth_client::latest_with_update_data(http, &self.hermes_url, feeds)
                .await
                .context("fetching hermes update data")?;
        if payloads.is_empty() {
            return Err(anyhow!("hermes returned no update payloads"));
        }
        let mut legs = Vec::new();
        for payload in &payloads {
            let (vaa, updates) =
                parse_accumulator_update(payload).context("parsing accumulator update")?;
            for update in updates {
                let feed = feed_id_of_update(&update)?;
                if !feeds.contains(&feed) {
                    continue;
                }
                let update_account = self.price_account(feed);
                let ix = post_update_atomic_ix(
                    &self.payer,
                    &update_account,
                    &self.payer, // write authority: repost-in-place reuse
                    &self.wormhole_program,
                    vaa.clone(),
                    update,
                    // Spread the fee-treasury write load deterministically.
                    feed.0[0],
                )?;
                legs.push(PostLeg {
                    feed,
                    ix,
                    update_keypair: self
                        .accounts
                        .get(&feed)
                        .expect("ensured above")
                        .insecure_clone(),
                });
            }
        }
        for feed in feeds {
            if !legs.iter().any(|l| l.feed == *feed) {
                return Err(anyhow!("hermes update data is missing feed {feed}"));
            }
        }
        Ok(legs)
    }
}

/// The feed id a Merkle price-update leg carries (parses the leg's
/// `PriceFeedMessage`).
pub fn feed_id_of_update(update: &MerklePriceUpdate) -> Result<PriceFeedId> {
    let message: pythnet_sdk::messages::Message =
        pythnet_sdk::wire::from_slice::<byteorder::BE, _>(update.message.as_ref())
            .map_err(|e| anyhow!("parsing merkle price-update message: {e:?}"))?;
    match message {
        pythnet_sdk::messages::Message::PriceFeedMessage(m) => Ok(PriceFeedId(m.feed_id)),
        other => Err(anyhow!("unexpected pyth message kind: {other:?}")),
    }
}

/// Whether `ixs`, signed by `num_signers` keys with `payer` as fee payer,
/// fit one Solana packet. Measured on a real unsigned transaction (the
/// signature slots serialize at full width, so the size is exact).
pub fn tx_fits(payer: &Pubkey, ixs: &[Instruction]) -> bool {
    let message = Message::new(ixs, Some(payer));
    let tx = Transaction::new_unsigned(message);
    match bincode::serialize(&tx) {
        Ok(bytes) => bytes.len() <= MAX_TX_BYTES,
        Err(_) => false,
    }
}

/// Send an oracle-gated crank: the post legs and the crank in one
/// transaction when it fits, else posts first (together, else one-by-one)
/// and the crank second — the update accounts persist between txs.
pub async fn send_oracle_gated(
    wrap: &SolanaClientWrapper,
    legs: &[PostLeg],
    crank_ixs: &[Instruction],
    crank_signers: &[&Keypair],
    label: &str,
) -> Result<()> {
    let payer = wrap.signer.pubkey();
    let post_ixs: Vec<Instruction> = legs.iter().map(|l| l.ix.clone()).collect();
    let post_signers: Vec<&Keypair> = legs.iter().map(|l| &l.update_keypair).collect();

    let mut combined = post_ixs.clone();
    combined.extend_from_slice(crank_ixs);
    if tx_fits(&payer, &combined) {
        let mut signers = post_signers.clone();
        signers.extend_from_slice(crank_signers);
        wrap.send_and_confirm(&combined, &signers, label).await?;
        return Ok(());
    }

    if !post_ixs.is_empty() {
        if legs.len() > 1 && tx_fits(&payer, &post_ixs) {
            wrap.send_and_confirm(&post_ixs, &post_signers, "pyth::post_update_atomic")
                .await?;
        } else {
            for leg in legs {
                wrap.send_and_confirm(
                    std::slice::from_ref(&leg.ix),
                    &[&leg.update_keypair],
                    "pyth::post_update_atomic",
                )
                .await?;
            }
        }
    }
    wrap.send_and_confirm(crank_ixs, crank_signers, label).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::instruction::AccountMeta;

    fn ix_with_data(bytes: usize) -> Instruction {
        Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![AccountMeta::new(Pubkey::new_unique(), true)],
            data: vec![0u8; bytes],
        }
    }

    #[test]
    fn tx_fits_measures_real_serialized_size() {
        let payer = Pubkey::new_unique();
        assert!(tx_fits(&payer, &[ix_with_data(100)]));
        // A single ~1.2KB instruction cannot fit with overhead.
        assert!(!tx_fits(&payer, &[ix_with_data(1_200)]));
        // Two mid-size instructions overflow together but fit alone.
        let a = ix_with_data(600);
        let b = ix_with_data(600);
        assert!(tx_fits(&payer, std::slice::from_ref(&a)));
        assert!(!tx_fits(&payer, &[a, b]));
    }

    #[test]
    fn feed_id_extraction_round_trips() {
        use pythnet_sdk::messages::{Message, PriceFeedMessage};
        let feed = [7u8; 32];
        let msg = Message::PriceFeedMessage(PriceFeedMessage {
            feed_id: feed,
            price: 42,
            conf: 1,
            exponent: -8,
            publish_time: 1_700_000_000,
            prev_publish_time: 1_699_999_999,
            ema_price: 42,
            ema_conf: 1,
        });
        let bytes = pythnet_sdk::wire::to_vec::<_, byteorder::BE>(&msg).unwrap();
        let update = MerklePriceUpdate {
            message: bytes.into(),
            proof: pythnet_sdk::accumulators::merkle::MerklePath::new(vec![]),
        };
        assert_eq!(feed_id_of_update(&update).unwrap(), PriceFeedId(feed));
    }

    #[test]
    fn poster_reuses_one_account_per_feed() {
        let mut poster = PythPoster::new(
            "https://hermes-beta.pyth.network".into(),
            solana_tx::pyth::WORMHOLE_RECEIVER_ID,
            Pubkey::new_unique(),
        );
        let feed_a = PriceFeedId([1u8; 32]);
        let feed_b = PriceFeedId([2u8; 32]);
        let a1 = poster.price_account(feed_a);
        let a2 = poster.price_account(feed_a);
        let b = poster.price_account(feed_b);
        assert_eq!(a1, a2, "same feed ⇒ same persistent account");
        assert_ne!(a1, b, "distinct feeds ⇒ distinct accounts");
    }
}
