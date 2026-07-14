//! The prize test (guide doc 09 §Verification): one full vault round
//! driven end-to-end by the KEEPER's own planner + instruction builders
//! against the real three-program stack under LiteSVM.
//!
//! Pyth accounts are FORGED — raw `PriceUpdateV2` bytes written straight
//! into the SVM (LiteSVM lets us set accounts), bypassing the
//! wormhole-verified `post_update_atomic` path, which cannot run here
//! (the receiver program's .so is not part of our tree and guardian-set
//! verification needs live Wormhole state). The posting path itself is
//! unit-tested in `src/pyth_leg.rs`; the ORACLE-READING side — feed
//! pinning, staleness, confidence — runs for real in every oracle-gated
//! crank below.
//!
//! Skips gracefully (with a note) when the `.so` files haven't been
//! built (`anchor build` in solana-contracts).
//!
//! Flow, each step chosen by `planner::plan` over state read back from
//! the SVM (this doubles as the planning smoke test — the plan sequence
//! IS the assertion):
//!   genesis deposit → FinalizeRound → SelectBucketNeeded (strike pick
//!   over 3 candidates) → OpenRfq slice → MM bids → SettleRfq →
//!   (warp to expiry) CrankRedeem → OpenSwapRfq → MM bids →
//!   SettleSwapRfq → FinalizeRound → round 2.

use anchor_lang::prelude::Clock;
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use litesvm::{types::FailedTransactionMetadata, LiteSVM};
use litesvm_token::{CreateAssociatedTokenAccount, CreateMint, MintTo};
use solana_sdk::account::Account as SolanaAccount;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;

use auction_venue::state::{Auction, AuctionMode};
use options_vault::oracle::{PRICE_UPDATE_V2_DISCRIMINATOR, PYTH_RECEIVER_ID};
use options_vault::state::{Vault, VaultConfig, VaultPosition};

use pyth_client::types::PriceFeedId;
use solana_tx::{ix, pda};

use solana_keeper::discovery::DiscoveredVault;
use solana_keeper::planner::{plan, Action, BucketMeta, PlanInput};
use solana_keeper::state::{token_balance, view_from_parts, RfqView, SwapRfqView, VaultView};
use solana_keeper::strike::{pick_bucket, BucketCandidate};
use solana_keeper::submit;

const DEPLOY_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../solana-contracts/target/deploy"
);

const GENESIS_SECS: i64 = 1_000_000;
const GENESIS_MS: u64 = GENESIS_SECS as u64 * 1000;
const DAY_MS: u64 = 86_400_000;

const U_FEED: [u8; 32] = [1u8; 32];
const S_FEED: [u8; 32] = [2u8; 32];

fn test_config() -> VaultConfig {
    VaultConfig {
        mgmt_fee_bps_annual: 200,
        perf_fee_bps: 2_000,
        round_ms: 7 * DAY_MS,
        selling_window_ms: DAY_MS,
        min_strike_bps_over_spot: 500,
        max_strike_bps_over_spot: 2_000,
        min_expiry_lead_ms: 3_600_000,
        max_expiry_lead_ms: 14 * DAY_MS,
        min_reserve_premium_bps: 50,
        max_slice_amount: 1_000_000,
        max_open_rfqs: 5,
        rfq_duration_ms: 600_000,
        rfq_snipe_window_ms: 60_000,
        rfq_snipe_extension_ms: 120_000,
        rfq_max_extension_ms: 600_000,
        rfq_min_increment_bps: 500,
        hold_premium_in_settlement: false,
        max_swap_slippage_bps: 100,
        underlying_feed_id: U_FEED,
        settlement_feed_id: S_FEED,
        max_price_age_secs: 3_600,
        max_conf_bps: 500,
        underlying_decimals: 8,
        settlement_decimals: 6,
    }
}

struct Ctx {
    svm: LiteSVM,
    admin: Keypair,
    user: Keypair,
    mm: Keypair,
    meta: DiscoveredVault,
    u_price: Pubkey,
    s_price: Pubkey,
}

impl Ctx {
    fn setup() -> Option<Self> {
        let mut svm = LiteSVM::new();
        for (id, name) in [
            (options_core::ID, "options_core"),
            (auction_venue::ID, "auction_venue"),
            (options_vault::ID, "options_vault"),
        ] {
            let path = format!("{DEPLOY_DIR}/{name}.so");
            match std::fs::read(&path) {
                Ok(bytes) => svm.add_program(id, &bytes).unwrap(),
                Err(_) => {
                    eprintln!("skipping litesvm round test: {path} not built");
                    return None;
                }
            }
        }
        let admin = Keypair::new();
        let user = Keypair::new();
        let mm = Keypair::new();
        for kp in [&admin, &user, &mm] {
            svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
        }
        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = GENESIS_SECS;
        svm.set_sysvar(&clock);

        let underlying = CreateMint::new(&mut svm, &admin).decimals(8).send().unwrap();
        let settlement = CreateMint::new(&mut svm, &admin).decimals(6).send().unwrap();
        let vault = pda::vault(&options_vault::ID, &underlying, &settlement, 0);

        let mut ctx = Ctx {
            svm,
            admin,
            user,
            mm,
            meta: DiscoveredVault {
                vault,
                underlying_mint: underlying,
                settlement_mint: settlement,
                underlying_decimals: 8,
                settlement_decimals: 6,
                underlying_feed: PriceFeedId(U_FEED),
                settlement_feed: PriceFeedId(S_FEED),
            },
            u_price: Pubkey::new_unique(),
            s_price: Pubkey::new_unique(),
        };

        // Initialize core, create the vault (both via solana-tx builders).
        let admin_pk = ctx.admin.pubkey();
        ctx.send(&ctx.admin.insecure_clone(), &[ix::initialize(&admin_pk)], &[]);
        ctx.send(
            &ctx.admin.insecure_clone(),
            &[ix::create_vault(&admin_pk, &underlying, &settlement, 0, test_config())],
            &[],
        );
        ctx.refresh_prices();
        Some(ctx)
    }

    fn send(&mut self, payer: &Keypair, ixs: &[Instruction], extra: &[&Keypair]) {
        self.try_send(payer, ixs, extra)
            .unwrap_or_else(|e| panic!("transaction failed: {:?}\nlogs: {:#?}", e.err, e.meta.logs));
    }

    fn try_send(
        &mut self,
        payer: &Keypair,
        ixs: &[Instruction],
        extra: &[&Keypair],
    ) -> Result<(), FailedTransactionMetadata> {
        self.svm.expire_blockhash();
        let blockhash = self.svm.latest_blockhash();
        let msg = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &blockhash);
        let mut signers: Vec<&Keypair> = vec![payer];
        signers.extend_from_slice(extra);
        let tx = Transaction::new(&signers, msg, blockhash);
        self.svm.send_transaction(tx).map(|_| ())
    }

    fn now_ms(&self) -> u64 {
        let clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp as u64 * 1000
    }

    fn warp_to_ms(&mut self, ms: u64) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = (ms / 1000) as i64;
        self.svm.set_sysvar(&clock);
    }

    /// Forge the two Pyth `PriceUpdateV2` accounts: $100k underlying
    /// (expo −8), $1 settlement, published "now" — cross = 1000
    /// settlement smallest-units per underlying smallest-unit (spot USD
    /// cross 100_000 in the keeper's human units).
    fn refresh_prices(&mut self) {
        let now = self.now_ms() as i64 / 1000;
        self.write_price(self.u_price, U_FEED, 10_000_000_000_000, 1_000_000, -8, now);
        self.write_price(self.s_price, S_FEED, 100_000_000, 10_000, -8, now);
    }

    fn write_price(
        &mut self,
        key: Pubkey,
        feed: [u8; 32],
        price: i64,
        conf: u64,
        expo: i32,
        publish_time: i64,
    ) {
        let mut data = Vec::with_capacity(134);
        data.extend_from_slice(&PRICE_UPDATE_V2_DISCRIMINATOR);
        data.extend_from_slice(&[0u8; 32]); // write_authority
        data.push(1); // VerificationLevel::Full
        data.extend_from_slice(&feed);
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&conf.to_le_bytes());
        data.extend_from_slice(&expo.to_le_bytes());
        data.extend_from_slice(&publish_time.to_le_bytes());
        data.extend_from_slice(&publish_time.to_le_bytes()); // prev
        data.extend_from_slice(&price.to_le_bytes()); // ema
        data.extend_from_slice(&conf.to_le_bytes()); // ema conf
        data.extend_from_slice(&0u64.to_le_bytes()); // posted_slot
        self.svm
            .set_account(
                key,
                SolanaAccount {
                    lamports: 1_000_000,
                    data,
                    owner: PYTH_RECEIVER_ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
    }

    fn read<T: AccountDeserialize>(&self, key: &Pubkey) -> T {
        let account = self.svm.get_account(key).unwrap();
        T::try_deserialize(&mut account.data.as_slice()).unwrap()
    }

    fn balance(&self, token_account: &Pubkey) -> u64 {
        token_balance(&self.svm.get_account(token_account).unwrap().data).unwrap()
    }

    fn fund(&mut self, owner: &Pubkey, mint: &Pubkey, amount: u64) -> Pubkey {
        let admin = self.admin.insecure_clone();
        let addr = pda::ata(owner, mint);
        if self.svm.get_account(&addr).map(|a| a.lamports == 0).unwrap_or(true) {
            CreateAssociatedTokenAccount::new(&mut self.svm, &admin, mint)
                .owner(owner)
                .send()
                .unwrap();
        }
        if amount > 0 {
            MintTo::new(&mut self.svm, &admin, mint, &addr, amount).send().unwrap();
        }
        addr
    }

    /// What the tick loop assembles over RPC, read straight off the SVM:
    /// the Vault account + the four balance-carrying token PDAs.
    fn view(&self) -> VaultView {
        let vp = options_vault::ID;
        let vault: Vault = self.read(&self.meta.vault);
        let bal = |key: Pubkey| self.balance(&key);
        view_from_parts(
            &vault,
            bal(pda::vault_deployable(&vp, &self.meta.vault)),
            bal(pda::vault_proceeds(&vp, &self.meta.vault)),
            bal(pda::vault_pending(&vp, &self.meta.vault)),
            bal(pda::vault_queued_shares(&vp, &self.meta.vault)),
        )
        .unwrap()
    }

    /// The auction-discovery step (indexer + live account reads in prod):
    /// here, walk the vault's auction nonces and read the live accounts.
    fn open_auctions(&self) -> (Vec<RfqView>, Vec<SwapRfqView>) {
        let vault: Vault = self.read(&self.meta.vault);
        let mut rfqs = Vec::new();
        let mut swaps = Vec::new();
        for nonce in 0..vault.auction_nonce {
            let key = pda::auction(&auction_venue::ID, &self.meta.vault, nonce);
            let Some(account) = self.svm.get_account(&key) else { continue };
            let Ok(a) = Auction::try_deserialize(&mut account.data.as_slice()) else { continue };
            match a.mode {
                AuctionMode::CoveredCall => rfqs.push(RfqView {
                    auction: key,
                    bucket: a.bucket,
                    deadline_ms: a.deadline_ms,
                    amount: a.amount,
                }),
                AuctionMode::Swap => swaps.push(SwapRfqView {
                    auction: key,
                    deadline_ms: a.deadline_ms,
                    amount_s: a.amount,
                }),
                AuctionMode::CashSecuredPut => {}
            }
        }
        (rfqs, swaps)
    }

    /// One keeper tick's pure decision over live SVM state.
    fn next_action(&self) -> Action {
        let view = self.view();
        let (auctions, swaps) = self.open_auctions();
        let meta = view.current_bucket.map(|_| BucketMeta { invalidated: false });
        plan(&PlanInput {
            view: &view,
            now_ms: self.now_ms(),
            auctions: &auctions,
            swap_auctions: &swaps,
            bucket_meta: meta.as_ref(),
            stagger_ms: 90 * 60_000,
            max_slices: 4,
        })
    }

    fn new_call_bucket(&mut self, salt: u64, expiry_ms: u64, strike: u128) -> Pubkey {
        let admin = self.admin.insecure_clone();
        let (u, s) = (self.meta.underlying_mint, self.meta.settlement_mint);
        self.send(
            &admin,
            &[ix::create_bucket(&admin.pubkey(), &u, &s, salt, expiry_ms, strike, 0)],
            &[],
        );
        pda::bucket(&options_core::ID, &u, &s, salt)
    }

    fn deposit(&mut self, amount: u64) {
        let user = self.user.insecure_clone();
        let receipt = Keypair::new();
        let ix = Instruction::new_with_bytes(
            options_vault::ID,
            &options_vault::instruction::Deposit { amount }.data(),
            options_vault::accounts::Deposit {
                depositor: user.pubkey(),
                vault: self.meta.vault,
                pending: pda::vault_pending(&options_vault::ID, &self.meta.vault),
                depositor_token: pda::ata(&user.pubkey(), &self.meta.underlying_mint),
                receipt: receipt.pubkey(),
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: pda::event_authority(&options_vault::ID),
                program: options_vault::ID,
            }
            .to_account_metas(None),
        );
        self.send(&user, &[ix], &[&receipt]);
    }

    /// MM bids on a venue auction (what the mm-bot does in prod).
    fn bid(&mut self, auction: &Pubkey, source_mint: &Pubkey, amount: u64) {
        let mm = self.mm.insecure_clone();
        let ix = ix::bid(
            &mm.pubkey(),
            auction,
            &pda::ata(&mm.pubkey(), source_mint),
            None,
            amount,
            mm.pubkey(),
        );
        self.send(&mm, &[ix], &[]);
    }
}

#[test]
fn keeper_drives_one_full_round() {
    let Some(mut ctx) = Ctx::setup() else { return };
    let cranker = ctx.admin.insecure_clone(); // the keeper's gas wallet
    let user_pk = ctx.user.pubkey();
    let mm_pk = ctx.mm.pubkey();
    let (u_mint, s_mint) = (ctx.meta.underlying_mint, ctx.meta.settlement_mint);
    let (u_price, s_price) = (ctx.u_price, ctx.s_price);
    ctx.fund(&user_pk, &u_mint, 1_000);
    ctx.fund(&mm_pk, &s_mint, 1_000_000);
    ctx.fund(&mm_pk, &u_mint, 1_000);

    // ── genesis: queue a deposit; the planner finalizes round 0 ──
    ctx.deposit(1_000);
    assert_eq!(ctx.next_action(), Action::FinalizeRound);
    let view = ctx.view();
    assert_eq!(view.round, 0);
    assert_eq!(view.pending_deposits, 1_000);
    // The keeper's finalize builder also creates the core treasury's fee
    // ATA idempotently (exercises the hand-encoded CreateIdempotent).
    let meta = ctx.meta.clone();
    let ixs = submit::build_finalize_round(&cranker.pubkey(), &meta, view.round, &u_price, &s_price);
    ctx.send(&cranker, &ixs, &[]);

    // ── round 1: strike selection over three candidates ──
    assert_eq!(ctx.next_action(), Action::SelectBucketNeeded);
    let expiry = GENESIS_MS + 7 * DAY_MS;
    // Chain strike scale 0, 8→6 decimals ⇒ USD cross = strike × 10².
    let _below_band = ctx.new_call_bucket(0, expiry, 1_020); // $102k: +2% < 5% floor
    let below_kstar = ctx.new_call_bucket(1, expiry, 1_100); // $110k: in band
    let snap_target = ctx.new_call_bucket(2, expiry, 1_150); // $115k: in band, ≥ K*
    let candidates: Vec<BucketCandidate> = [1_020u128, 1_100, 1_150]
        .iter()
        .enumerate()
        .map(|(i, k)| BucketCandidate {
            bucket: pda::bucket(&options_core::ID, &u_mint, &s_mint, i as u64),
            strike_raw: *k,
            strike_scale: 0,
            expiry_ms: expiry,
        })
        .collect();
    let view = ctx.view();
    // spot cross = $100k/$1 = 100_000; σ_iv = 1.0 weekly ⇒ K* ≈ 113.5k:
    // 110k is skipped (below K*), snap lands on 115k.
    let pick = pick_bucket(&candidates, 100_000.0, 1.0, ctx.now_ms(), &view.config, 8, 6, 0.20)
        .expect("an in-band candidate exists");
    assert_eq!(pick.bucket, snap_target, "snap-up must pass over {below_kstar}");
    assert!(!pick.grid_coverage_miss);
    assert!(pick.strike_usd >= pick.k_star_usd);
    assert!(
        pick.clears_reserve(100_000.0, view.config.min_reserve_premium_bps),
        "weekly pick must clear the reserve: {pick:?}"
    );
    ctx.refresh_prices();
    let ix = submit::build_select_bucket(&cranker.pubkey(), &meta, &pick.bucket, &u_price, &s_price);
    ctx.send(&cranker, &[ix], &[]);
    let bucket = pick.bucket;

    // ── the planner opens the first slice (1000 / 4 slots = 250) ──
    assert_eq!(
        ctx.next_action(),
        Action::OpenRfq { bucket, slice_amount: 250 }
    );
    let view = ctx.view();
    let ix = submit::build_open_rfq(
        &cranker.pubkey(),
        &meta,
        &bucket,
        view.auction_nonce,
        250,
        &u_price,
        &s_price,
    );
    ctx.send(&cranker, &[ix], &[]);
    let rfq_auction = pda::auction(&auction_venue::ID, &meta.vault, 0);
    // Reserve = 50 bps of the 250 × 1000 settlement notional.
    let a: Auction = ctx.read(&rfq_auction);
    assert_eq!(a.reserve_bid, 1_250);
    // An auction is open and not yet due ⇒ the keeper idles.
    assert_eq!(ctx.next_action(), Action::Idle);

    // ── MM bids; deadline passes; the planner settles the slice ──
    ctx.bid(&rfq_auction, &s_mint, 10_000);
    ctx.warp_to_ms(GENESIS_MS + 600_000);
    assert_eq!(
        ctx.next_action(),
        Action::SettleRfq { auction: rfq_auction, bucket }
    );
    let live: Auction = ctx.read(&rfq_auction);
    assert_eq!(live.best_bidder, Some(mm_pk));
    let view = ctx.view();
    // The keeper creates the winner's call ATA + the treasury fee ATA
    // idempotently inside the same tx.
    let (ixs, position) = submit::build_settle_rfq(
        &cranker.pubkey(),
        &meta,
        &rfq_auction,
        live.best_token_recipient,
        view.positions_tail,
        &bucket,
    );
    ctx.send(&cranker, &ixs, &[&position]);
    let view = ctx.view();
    assert_eq!(view.proceeds_settlement, 10_000);
    assert_eq!(view.pending_positions, 1);
    assert_eq!(view.open_rfqs, 0);
    let call_dest = pda::ata(&mm_pk, &pda::call_mint(&options_core::ID, &bucket));
    assert_eq!(ctx.balance(&call_dest), 250, "winner got the call tokens");

    // ── expiry: the settling ladder redeems the FIFO first ──
    ctx.warp_to_ms(expiry);
    assert_eq!(ctx.next_action(), Action::CrankRedeem { bucket });
    let view = ctx.view();
    let vp: VaultPosition = ctx.read(&pda::vault_position(
        &options_vault::ID,
        &meta.vault,
        view.positions_head,
    ));
    let ix = submit::build_crank_redeem(
        &cranker.pubkey(),
        &meta,
        &bucket,
        view.positions_head,
        &vp.position,
    );
    ctx.send(&cranker, &[ix], &[]);
    let view = ctx.view();
    assert_eq!(view.pending_positions, 0);
    assert_eq!(view.deployable, 1_000, "unexercised collateral came home");

    // ── proceeds conversion: open the coupled swap, MM fills it ──
    assert_eq!(ctx.next_action(), Action::OpenSwapRfq { amount_s: 10_000 });
    ctx.refresh_prices();
    let view = ctx.view();
    let ix = submit::build_open_swap_rfq(
        &cranker.pubkey(),
        &meta,
        view.auction_nonce,
        10_000,
        &u_price,
        &s_price,
    );
    ctx.send(&cranker, &[ix], &[]);
    let swap_auction = pda::auction(&auction_venue::ID, &meta.vault, 1);
    // Band floor: 10_000 settlement ≙ 10 underlying × 99% ⇒ 9.
    let a: Auction = ctx.read(&swap_auction);
    assert_eq!(a.reserve_bid, 9);
    assert_eq!(ctx.next_action(), Action::Idle, "swap open, not yet due");

    ctx.bid(&swap_auction, &u_mint, 10);
    ctx.warp_to_ms(expiry + 600_000);
    assert_eq!(ctx.next_action(), Action::SettleSwapRfq { auction: swap_auction });
    ctx.refresh_prices();
    let live: Auction = ctx.read(&swap_auction);
    let ixs = submit::build_settle_swap_rfq(
        &cranker.pubkey(),
        &meta,
        &swap_auction,
        live.best_bidder,
        &u_price,
        &s_price,
    );
    ctx.send(&cranker, &ixs, &[]);
    let view = ctx.view();
    assert_eq!(view.deployable, 1_010, "premium compounded into underlying");
    assert_eq!(view.proceeds_settlement, 0);

    // ── the round finalizes; the machine asks for the next selection ──
    assert_eq!(ctx.next_action(), Action::FinalizeRound);
    let ixs = submit::build_finalize_round(&cranker.pubkey(), &meta, view.round, &u_price, &s_price);
    ctx.send(&cranker, &ixs, &[]);
    let view = ctx.view();
    assert_eq!(view.round, 2);
    assert!(!view.settling);
    // pps[1]: aum 1010, profit 10 → mgmt floors to 0, perf = 2 ⇒ 1.008.
    let rs: options_vault::state::RoundState =
        ctx.read(&pda::round_state(&options_vault::ID, &meta.vault, 1));
    assert_eq!(rs.pps, 1_008_000_000_000);
    // Perf fee (2 underlying) landed in the treasury ATA the keeper's
    // finalize builder created back at genesis.
    let treasury_u = pda::ata(&pda::treasury(&options_core::ID), &u_mint);
    assert_eq!(ctx.balance(&treasury_u), 2);
    assert_eq!(ctx.next_action(), Action::SelectBucketNeeded);
}

/// The recovery path: an auction left open across bucket expiry must
/// route to `settle_rfq_expired` (refund bid + escrow) — the planner's
/// Solana-specific deviation, exercised against the real programs.
#[test]
fn keeper_recovers_auction_stranded_across_expiry() {
    let Some(mut ctx) = Ctx::setup() else { return };
    let cranker = ctx.admin.insecure_clone();
    let user_pk = ctx.user.pubkey();
    let mm_pk = ctx.mm.pubkey();
    let (u_mint, s_mint) = (ctx.meta.underlying_mint, ctx.meta.settlement_mint);
    let (u_price, s_price) = (ctx.u_price, ctx.s_price);
    let meta = ctx.meta.clone();
    ctx.fund(&user_pk, &u_mint, 1_000);
    ctx.fund(&mm_pk, &s_mint, 1_000_000);

    ctx.deposit(1_000);
    let ixs = submit::build_finalize_round(&cranker.pubkey(), &meta, 0, &u_price, &s_price);
    ctx.send(&cranker, &ixs, &[]);

    // Short-dated bucket: expiry 2h out (inside the min 1h lead).
    let expiry = GENESIS_MS + 2 * 3_600_000;
    let bucket = ctx.new_call_bucket(0, expiry, 1_150);
    ctx.refresh_prices();
    let ix = submit::build_select_bucket(&cranker.pubkey(), &meta, &bucket, &u_price, &s_price);
    ctx.send(&cranker, &[ix], &[]);
    let view = ctx.view();
    let ix = submit::build_open_rfq(
        &cranker.pubkey(),
        &meta,
        &bucket,
        view.auction_nonce,
        250,
        &u_price,
        &s_price,
    );
    ctx.send(&cranker, &[ix], &[]);
    let auction = pda::auction(&auction_venue::ID, &meta.vault, 0);
    ctx.bid(&auction, &s_mint, 10_000);
    let mm_settlement_before = ctx.balance(&pda::ata(&mm_pk, &s_mint));

    // Nobody settles before expiry (keeper outage): the bucket dies with
    // the auction (and its winning bid) stranded.
    ctx.warp_to_ms(expiry);
    assert_eq!(ctx.next_action(), Action::SettleRfqExpired { auction, bucket });
    let live: Auction = ctx.read(&auction);
    let ixs = submit::build_settle_rfq_expired(
        &cranker.pubkey(),
        &meta,
        &auction,
        &bucket,
        live.best_bidder,
    );
    ctx.send(&cranker, &ixs, &[]);
    let view = ctx.view();
    assert_eq!(view.open_rfqs, 0);
    assert_eq!(view.deployable, 1_000, "escrow refunded to deployable");
    assert_eq!(
        ctx.balance(&pda::ata(&mm_pk, &s_mint)),
        mm_settlement_before + 10_000,
        "stranded bid refunded to the bidder"
    );
    // No proceeds, no positions: the idle round finalizes.
    assert_eq!(ctx.next_action(), Action::FinalizeRound);
}
