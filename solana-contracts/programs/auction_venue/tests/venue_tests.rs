//! Auction venue tests — ports `rfq_tests.move`, `rfq_put_tests.move`
//! and the swap-auction cases, running the REAL two-program stack: venue
//! settle CPIs into options_core's `write_collateralized` surface (this
//! doubles as the port plan's CPI-consumer harness for core).

use anchor_lang::{
    prelude::{Clock, Pubkey},
    solana_program::instruction::Instruction,
    AccountDeserialize, InstructionData, ToAccountMetas,
};
use auction_venue::instructions::create::AuctionParams;
use auction_venue::state::{Auction, AuctionMode, AUCTION_SEED, BIDS_SEED, ESCROW_SEED};
use litesvm::{types::FailedTransactionMetadata, LiteSVM};
use litesvm_token::{CreateAssociatedTokenAccount, CreateMint, MintTo};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const GENESIS_SECS: i64 = 1_000_000;
const GENESIS_MS: u64 = GENESIS_SECS as u64 * 1000;
const DAY_MS: u64 = 86_400_000;
const STRIKE: u128 = 5;
const SCALE: u8 = 1;

fn core_id() -> Pubkey {
    options_core::id()
}
fn venue_id() -> Pubkey {
    auction_venue::id()
}
fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    anchor_spl::associated_token::get_associated_token_address(owner, mint)
}
fn event_authority(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program).0
}
fn default_expiry() -> u64 {
    GENESIS_MS + DAY_MS
}

fn default_params(seller: Pubkey, reserve: u64) -> AuctionParams {
    AuctionParams {
        reserve_bid: reserve,
        duration_ms: 600_000,       // 10 min
        snipe_window_ms: 60_000,    // 1 min
        snipe_extension_ms: 120_000, // 2 min
        max_extension_ms: 600_000,  // 10 min cap
        min_increment_bps: 500,     // 5%
        position_recipient: seller,
        settle_authority: None,
    }
}

struct Ctx {
    svm: LiteSVM,
    admin: Keypair,
    seller: Keypair,
    mm1: Keypair,
    mm2: Keypair,
    underlying: Pubkey,
    settlement: Pubkey,
    core_config: Pubkey,
    core_treasury: Pubkey,
}

struct AuctionKeys {
    auction: Pubkey,
    escrow_vault: Pubkey,
    bid_vault: Pubkey,
}

impl Ctx {
    fn setup() -> Self {
        let mut svm = LiteSVM::new();
        svm.add_program(
            core_id(),
            include_bytes!(concat!(env!("CARGO_TARGET_TMPDIR"), "/../deploy/options_core.so")),
        )
        .unwrap();
        svm.add_program(
            venue_id(),
            include_bytes!(concat!(env!("CARGO_TARGET_TMPDIR"), "/../deploy/auction_venue.so")),
        )
        .unwrap();

        let admin = Keypair::new();
        let seller = Keypair::new();
        let mm1 = Keypair::new();
        let mm2 = Keypair::new();
        for kp in [&admin, &seller, &mm1, &mm2] {
            svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
        }
        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = GENESIS_SECS;
        svm.set_sysvar(&clock);

        let underlying = CreateMint::new(&mut svm, &admin).decimals(8).send().unwrap();
        let settlement = CreateMint::new(&mut svm, &admin).decimals(6).send().unwrap();

        let core_config = Pubkey::find_program_address(&[b"config"], &core_id()).0;
        let core_treasury = Pubkey::find_program_address(&[b"treasury"], &core_id()).0;

        let mut ctx = Ctx {
            svm,
            admin,
            seller,
            mm1,
            mm2,
            underlying,
            settlement,
            core_config,
            core_treasury,
        };
        // Initialize core.
        let ix = Instruction::new_with_bytes(
            core_id(),
            &options_core::instruction::Initialize {}.data(),
            options_core::accounts::Initialize {
                admin: ctx.admin.pubkey(),
                config: core_config,
                treasury: core_treasury,
                system_program: anchor_lang::system_program::ID,
                event_authority: event_authority(&core_id()),
                program: core_id(),
            }
            .to_account_metas(None),
        );
        let admin = ctx.admin.insecure_clone();
        ctx.send(&admin, &[ix], &[]).unwrap();
        ctx
    }

    fn send(
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
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
        self.svm.send_transaction(tx).map(|_| ())
    }

    fn warp_to_ms(&mut self, ms: u64) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = (ms / 1000) as i64;
        self.svm.set_sysvar(&clock);
    }

    fn read<T: AccountDeserialize>(&self, key: &Pubkey) -> T {
        let account = self.svm.get_account(key).unwrap();
        T::try_deserialize(&mut account.data.as_slice()).unwrap()
    }

    fn exists(&self, key: &Pubkey) -> bool {
        self.svm.get_account(key).map(|a| a.lamports > 0).unwrap_or(false)
    }

    fn balance(&self, token_account: &Pubkey) -> u64 {
        let acc: anchor_spl::token::TokenAccount = self.read(token_account);
        acc.amount
    }

    fn fund(&mut self, owner: &Pubkey, mint: &Pubkey, amount: u64) -> Pubkey {
        let admin = self.admin.insecure_clone();
        let addr = ata(owner, mint);
        if !self.exists(&addr) {
            CreateAssociatedTokenAccount::new(&mut self.svm, &admin, mint)
                .owner(owner)
                .send()
                .unwrap();
        }
        if amount > 0 {
            MintTo::new(&mut self.svm, &admin, mint, &addr, amount)
                .send()
                .unwrap();
        }
        addr
    }

    fn set_fee(&mut self, bps: u64) {
        let ix = Instruction::new_with_bytes(
            core_id(),
            &options_core::instruction::SetFeeBps { new_bps: bps }.data(),
            options_core::accounts::AdminConfig {
                admin: self.admin.pubkey(),
                config: self.core_config,
                event_authority: event_authority(&core_id()),
                program: core_id(),
            }
            .to_account_metas(None),
        );
        let admin = self.admin.insecure_clone();
        self.send(&admin, &[ix], &[]).unwrap();
    }

    fn new_call_bucket(&mut self, salt: u64) -> (Pubkey, Pubkey) {
        let bucket = Pubkey::find_program_address(
            &[
                b"bucket",
                self.underlying.as_ref(),
                self.settlement.as_ref(),
                &salt.to_le_bytes(),
            ],
            &core_id(),
        )
        .0;
        let call_mint =
            Pubkey::find_program_address(&[b"call_mint", bucket.as_ref()], &core_id()).0;
        let ix = Instruction::new_with_bytes(
            core_id(),
            &options_core::instruction::CreateBucket {
                salt,
                expiry_ms: default_expiry(),
                strike: STRIKE,
                strike_scale: SCALE,
            }
            .data(),
            options_core::accounts::CreateBucket {
                admin: self.admin.pubkey(),
                config: self.core_config,
                underlying_mint: self.underlying,
                settlement_mint: self.settlement,
                bucket,
                call_mint,
                underlying_vault: ata(&bucket, &self.underlying),
                settlement_vault: ata(&bucket, &self.settlement),
                token_program: anchor_spl::token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: event_authority(&core_id()),
                program: core_id(),
            }
            .to_account_metas(None),
        );
        let admin = self.admin.insecure_clone();
        self.send(&admin, &[ix], &[]).unwrap();
        (bucket, call_mint)
    }

    fn new_put_bucket(&mut self, salt: u64) -> (Pubkey, Pubkey) {
        let bucket = Pubkey::find_program_address(
            &[
                b"put_bucket",
                self.underlying.as_ref(),
                self.settlement.as_ref(),
                &salt.to_le_bytes(),
            ],
            &core_id(),
        )
        .0;
        let put_mint = Pubkey::find_program_address(&[b"put_mint", bucket.as_ref()], &core_id()).0;
        let ix = Instruction::new_with_bytes(
            core_id(),
            &options_core::instruction::CreatePutBucket {
                salt,
                expiry_ms: default_expiry(),
                strike: STRIKE,
                strike_scale: SCALE,
            }
            .data(),
            options_core::accounts::CreatePutBucket {
                admin: self.admin.pubkey(),
                config: self.core_config,
                underlying_mint: self.underlying,
                settlement_mint: self.settlement,
                bucket,
                put_mint,
                underlying_vault: ata(&bucket, &self.underlying),
                settlement_vault: ata(&bucket, &self.settlement),
                token_program: anchor_spl::token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: event_authority(&core_id()),
                program: core_id(),
            }
            .to_account_metas(None),
        );
        let admin = self.admin.insecure_clone();
        self.send(&admin, &[ix], &[]).unwrap();
        (bucket, put_mint)
    }

    fn auction_keys(&self, creator: &Pubkey, salt: u64) -> AuctionKeys {
        let auction = Pubkey::find_program_address(
            &[AUCTION_SEED, creator.as_ref(), &salt.to_le_bytes()],
            &venue_id(),
        )
        .0;
        AuctionKeys {
            auction,
            escrow_vault: Pubkey::find_program_address(
                &[ESCROW_SEED, auction.as_ref()],
                &venue_id(),
            )
            .0,
            bid_vault: Pubkey::find_program_address(&[BIDS_SEED, auction.as_ref()], &venue_id()).0,
        }
    }

    /// Create a call auction: seller escrows `amount` underlying;
    /// proceeds → seller settlement ATA, refund → seller underlying ATA.
    fn create_call_auction(
        &mut self,
        salt: u64,
        bucket: Pubkey,
        amount: u64,
        params: AuctionParams,
    ) -> Result<AuctionKeys, FailedTransactionMetadata> {
        let seller = self.seller.insecure_clone();
        let keys = self.auction_keys(&seller.pubkey(), salt);
        let proceeds = self.fund(&seller.pubkey(), &self.settlement.clone(), 0);
        let refund = ata(&seller.pubkey(), &self.underlying);
        let ix = Instruction::new_with_bytes(
            venue_id(),
            &auction_venue::instruction::CreateCallAuction {
                salt,
                escrow_amount: amount,
                params,
            }
            .data(),
            auction_venue::accounts::CreateAuction {
                creator: seller.pubkey(),
                escrow_mint: self.underlying,
                bid_mint: self.settlement,
                auction: keys.auction,
                escrow_vault: keys.escrow_vault,
                bid_vault: keys.bid_vault,
                escrow_source: ata(&seller.pubkey(), &self.underlying),
                proceeds_token: proceeds,
                refund_token: refund,
                bucket,
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: event_authority(&venue_id()),
                program: venue_id(),
            }
            .to_account_metas(None),
        );
        self.send(&seller, &[ix], &[])?;
        Ok(keys)
    }

    fn bid(
        &mut self,
        bidder: &Keypair,
        keys: &AuctionKeys,
        amount: u64,
        prev_bidder: Option<Pubkey>,
    ) -> Result<(), FailedTransactionMetadata> {
        let auction: Auction = self.read(&keys.auction);
        let refund = prev_bidder.map(|p| ata(&p, &auction.bid_mint));
        let ix = Instruction::new_with_bytes(
            venue_id(),
            &auction_venue::instruction::Bid {
                amount,
                token_recipient: bidder.pubkey(),
            }
            .data(),
            auction_venue::accounts::Bid {
                bidder: bidder.pubkey(),
                auction: keys.auction,
                bid_vault: keys.bid_vault,
                bidder_source: ata(&bidder.pubkey(), &auction.bid_mint),
                previous_bidder_refund: refund,
                token_program: anchor_spl::token::ID,
                event_authority: event_authority(&venue_id()),
                program: venue_id(),
            }
            .to_account_metas(None),
        );
        self.send(&bidder.insecure_clone(), &[ix], &[])
    }

    fn settle_call(
        &mut self,
        cranker: &Keypair,
        keys: &AuctionKeys,
        bucket: Pubkey,
        call_mint: Pubkey,
        authority: Option<&Keypair>,
    ) -> Result<Pubkey, FailedTransactionMetadata> {
        let auction: Auction = self.read(&keys.auction);
        let position = Keypair::new();
        let call_dest = match auction.best_token_recipient {
            Some(r) => self.fund(&r, &call_mint, 0),
            None => self.fund(&cranker.pubkey(), &call_mint, 0),
        };
        let treasury_token = self.fund(&self.core_treasury.clone(), &self.settlement.clone(), 0);
        let ix = Instruction::new_with_bytes(
            venue_id(),
            &auction_venue::instruction::SettleCall {}.data(),
            auction_venue::accounts::SettleCall {
                cranker: cranker.pubkey(),
                creator_wallet: auction.creator,
                auction: keys.auction,
                escrow_vault: keys.escrow_vault,
                bid_vault: keys.bid_vault,
                authority: authority.map(|a| a.pubkey()),
                proceeds_token: auction.proceeds_token,
                refund_token: auction.refund_token,
                bucket,
                position: position.pubkey(),
                underlying_vault: ata(&bucket, &self.underlying),
                call_mint,
                call_dest,
                core_config: self.core_config,
                core_treasury_token: treasury_token,
                core_event_authority_acc: event_authority(&core_id()),
                core_program: core_id(),
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: event_authority(&venue_id()),
                program: venue_id(),
            }
            .to_account_metas(None),
        );
        let mut extra: Vec<&Keypair> = vec![&position];
        if let Some(a) = authority {
            extra.push(a);
        }
        self.send(&cranker.insecure_clone(), &[ix], &extra)?;
        Ok(position.pubkey())
    }
}

fn assert_venue_err(
    result: Result<(), FailedTransactionMetadata>,
    expected: auction_venue::error::VenueError,
) {
    use solana_instruction::error::InstructionError;
    use solana_transaction_error::TransactionError;
    let err = result.expect_err("expected failure");
    let expected_code = 6000 + expected as u32;
    match err.err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            assert_eq!(code, expected_code, "logs: {:?}", err.meta.logs)
        }
        other => panic!("expected custom error {expected_code}, got {other:?}"),
    }
}

#[test]
fn call_auction_full_flow_with_outbid_and_fee() {
    let mut ctx = Ctx::setup();
    ctx.set_fee(50); // 0.5%
    let (bucket, call_mint) = ctx.new_call_bucket(0);
    let seller = ctx.seller.insecure_clone();
    let mm1 = ctx.mm1.insecure_clone();
    let mm2 = ctx.mm2.insecure_clone();
    ctx.fund(&seller.pubkey(), &ctx.underlying.clone(), 1_000);
    ctx.fund(&mm1.pubkey(), &ctx.settlement.clone(), 100_000);
    ctx.fund(&mm2.pubkey(), &ctx.settlement.clone(), 100_000);

    let params = default_params(seller.pubkey(), 1_000);
    let keys = ctx
        .create_call_auction(0, bucket, 100, params)
        .unwrap();
    assert_eq!(ctx.balance(&keys.escrow_vault), 100);

    // Below-reserve bid rejected.
    let result = ctx.bid(&mm1, &keys, 999, None);
    assert_venue_err(result, auction_venue::error::VenueError::BidTooLow);

    // mm1 bids the reserve; mm2 must beat it by the 5% increment.
    ctx.bid(&mm1, &keys, 1_000, None).unwrap();
    let result = ctx.bid(&mm2, &keys, 1_049, Some(mm1.pubkey()));
    assert_venue_err(result, auction_venue::error::VenueError::BidTooLow);
    ctx.bid(&mm2, &keys, 1_050, Some(mm1.pubkey())).unwrap();
    // mm1 was refunded in full.
    assert_eq!(ctx.balance(&ata(&mm1.pubkey(), &ctx.settlement)), 100_000);
    assert_eq!(ctx.balance(&keys.bid_vault), 1_050);

    // Settle before the deadline is rejected.
    let admin = ctx.admin.insecure_clone();
    let result = ctx
        .settle_call(&admin, &keys, bucket, call_mint, None)
        .map(|_| ());
    assert_venue_err(result, auction_venue::error::VenueError::AuctionNotClosed);

    // Past the deadline anyone settles.
    let auction: Auction = ctx.read(&keys.auction);
    ctx.warp_to_ms(auction.deadline_ms);
    let position = ctx
        .settle_call(&admin, &keys, bucket, call_mint, None)
        .unwrap();

    // Winner got the calls; seller owns the Position over [0, 100).
    assert_eq!(ctx.balance(&ata(&mm2.pubkey(), &call_mint)), 100);
    let pos: options_core::state::Position = ctx.read(&position);
    assert_eq!(pos.owner, seller.pubkey());
    assert_eq!(pos.bucket, bucket);
    assert_eq!(pos.range_start, 0);
    assert_eq!(pos.range_end, 100);

    // Premium: 1_050 gross, 5 fee (0.5% floor), 1_045 net to seller.
    assert_eq!(
        ctx.balance(&ata(&ctx.core_treasury, &ctx.settlement)),
        5
    );
    assert_eq!(ctx.balance(&ata(&seller.pubkey(), &ctx.settlement)), 1_045);

    // Bucket escrowed the write; auction fully closed.
    let core_bucket: options_core::state::Bucket = ctx.read(&bucket);
    assert_eq!(core_bucket.total_written, 100);
    assert!(!ctx.exists(&keys.auction));
    assert!(!ctx.exists(&keys.escrow_vault));
    assert!(!ctx.exists(&keys.bid_vault));
}

#[test]
fn no_bid_settle_refunds_escrow() {
    let mut ctx = Ctx::setup();
    let (bucket, call_mint) = ctx.new_call_bucket(0);
    let seller = ctx.seller.insecure_clone();
    ctx.fund(&seller.pubkey(), &ctx.underlying.clone(), 1_000);
    let params = default_params(seller.pubkey(), 1_000);
    let keys = ctx.create_call_auction(0, bucket, 100, params).unwrap();

    let auction: Auction = ctx.read(&keys.auction);
    ctx.warp_to_ms(auction.max_deadline_ms);
    let admin = ctx.admin.insecure_clone();
    ctx.settle_call(&admin, &keys, bucket, call_mint, None)
        .unwrap();
    // Escrow returned to the seller's refund account; nothing written.
    assert_eq!(ctx.balance(&ata(&seller.pubkey(), &ctx.underlying)), 1_000);
    let core_bucket: options_core::state::Bucket = ctx.read(&bucket);
    assert_eq!(core_bucket.total_written, 0);
    assert!(!ctx.exists(&keys.auction));
}

#[test]
fn anti_snipe_extends_deadline_capped() {
    let mut ctx = Ctx::setup();
    let (bucket, _) = ctx.new_call_bucket(0);
    let seller = ctx.seller.insecure_clone();
    let mm1 = ctx.mm1.insecure_clone();
    ctx.fund(&seller.pubkey(), &ctx.underlying.clone(), 1_000);
    ctx.fund(&mm1.pubkey(), &ctx.settlement.clone(), 100_000);
    let params = default_params(seller.pubkey(), 1_000);
    let keys = ctx.create_call_auction(0, bucket, 100, params).unwrap();
    let before: Auction = ctx.read(&keys.auction);

    // Bid 30s before the deadline (inside the 60s snipe window): the
    // deadline extends by the 2-minute snipe extension.
    let bid_time = before.deadline_ms - 30_000;
    ctx.warp_to_ms(bid_time);
    ctx.bid(&mm1, &keys, 1_000, None).unwrap();
    let after: Auction = ctx.read(&keys.auction);
    assert_eq!(after.deadline_ms, bid_time + 120_000);
    assert!(after.deadline_ms <= after.max_deadline_ms);
}

#[test]
fn settle_expired_recovers_both_escrows_after_invalidation() {
    let mut ctx = Ctx::setup();
    let (bucket, _) = ctx.new_call_bucket(0);
    let seller = ctx.seller.insecure_clone();
    let mm1 = ctx.mm1.insecure_clone();
    ctx.fund(&seller.pubkey(), &ctx.underlying.clone(), 1_000);
    ctx.fund(&mm1.pubkey(), &ctx.settlement.clone(), 100_000);
    let params = default_params(seller.pubkey(), 1_000);
    let keys = ctx.create_call_auction(0, bucket, 100, params).unwrap();
    ctx.bid(&mm1, &keys, 2_000, None).unwrap();

    // Recovery path unavailable while the bucket is live.
    let admin = ctx.admin.insecure_clone();
    let auction: Auction = ctx.read(&keys.auction);
    let (settlement, core_config) = (ctx.settlement, ctx.core_config);
    let settle_expired_ix = |auction_state: &Auction| {
        Instruction::new_with_bytes(
            venue_id(),
            &auction_venue::instruction::SettleExpired {}.data(),
            auction_venue::accounts::SettleExpired {
                cranker: admin.pubkey(),
                creator_wallet: auction_state.creator,
                auction: keys.auction,
                escrow_vault: keys.escrow_vault,
                bid_vault: keys.bid_vault,
                authority: None,
                bucket,
                bidder_refund: Some(ata(&mm1.pubkey(), &settlement)),
                refund_token: auction_state.refund_token,
                token_program: anchor_spl::token::ID,
                event_authority: event_authority(&venue_id()),
                program: venue_id(),
            }
            .to_account_metas(None),
        )
    };
    let ix = settle_expired_ix(&auction);
    let result = ctx.send(&admin, &[ix], &[]);
    assert_venue_err(result, auction_venue::error::VenueError::BucketStillLive);

    // Invalidate the bucket mid-auction — the write can never execute.
    let ix = Instruction::new_with_bytes(
        core_id(),
        &options_core::instruction::InvalidateBucket {
            reason: "incident".into(),
        }
        .data(),
        options_core::accounts::ToggleBucketValidity {
            admin: admin.pubkey(),
            config: core_config,
            bucket,
            event_authority: event_authority(&core_id()),
            program: core_id(),
        }
        .to_account_metas(None),
    );
    ctx.send(&admin, &[ix], &[]).unwrap();

    // Now the recovery path refunds both sides, no deadline needed.
    let ix = settle_expired_ix(&auction);
    ctx.send(&admin, &[ix], &[]).unwrap();
    assert_eq!(ctx.balance(&ata(&mm1.pubkey(), &ctx.settlement)), 100_000);
    assert_eq!(ctx.balance(&ata(&seller.pubkey(), &ctx.underlying)), 1_000);
    assert!(!ctx.exists(&keys.auction));
}

#[test]
fn coupled_auction_requires_settle_authority() {
    let mut ctx = Ctx::setup();
    let (bucket, call_mint) = ctx.new_call_bucket(0);
    let seller = ctx.seller.insecure_clone();
    let mm1 = ctx.mm1.insecure_clone();
    let vault_authority = Keypair::new();
    ctx.svm.airdrop(&vault_authority.pubkey(), 1_000_000_000).unwrap();
    ctx.fund(&seller.pubkey(), &ctx.underlying.clone(), 1_000);
    ctx.fund(&mm1.pubkey(), &ctx.settlement.clone(), 100_000);

    let mut params = default_params(seller.pubkey(), 1_000);
    params.settle_authority = Some(vault_authority.pubkey());
    let keys = ctx.create_call_auction(0, bucket, 100, params).unwrap();
    ctx.bid(&mm1, &keys, 1_000, None).unwrap();
    let auction: Auction = ctx.read(&keys.auction);
    ctx.warp_to_ms(auction.deadline_ms);

    // Permissionless settle is refused for coupled auctions…
    let admin = ctx.admin.insecure_clone();
    let result = ctx
        .settle_call(&admin, &keys, bucket, call_mint, None)
        .map(|_| ());
    assert_venue_err(result, auction_venue::error::VenueError::WrongSettleAuthority);

    // …and succeeds with the authority co-signing.
    ctx.settle_call(&admin, &keys, bucket, call_mint, Some(&vault_authority))
        .unwrap();
}

#[test]
fn put_auction_escrows_ceil_collateral_and_settles() {
    let mut ctx = Ctx::setup();
    let (bucket, put_mint) = ctx.new_put_bucket(0);
    let seller = ctx.seller.insecure_clone();
    let mm1 = ctx.mm1.insecure_clone();
    ctx.fund(&seller.pubkey(), &ctx.settlement.clone(), 1_000);
    ctx.fund(&mm1.pubkey(), &ctx.settlement.clone(), 100_000);

    // notional 101 at strike 0.5 → ceil(50.5) = 51 collateral.
    let params = default_params(seller.pubkey(), 500);
    let seller_kp = seller.insecure_clone();
    let keys = ctx.auction_keys(&seller_kp.pubkey(), 0);
    let proceeds = ata(&seller.pubkey(), &ctx.settlement);
    // Both put legs are settlement-mint; the refund destination must be a
    // distinct token account (duplicate-mutable-account guard).
    let admin_kp = ctx.admin.insecure_clone();
    let refund = litesvm_token::CreateAccount::new(&mut ctx.svm, &admin_kp, &ctx.settlement.clone())
        .owner(&seller.pubkey())
        .send()
        .unwrap();
    let ix = Instruction::new_with_bytes(
        venue_id(),
        &auction_venue::instruction::CreatePutAuction {
            salt: 0,
            notional: 101,
            params,
        }
        .data(),
        auction_venue::accounts::CreateAuction {
            creator: seller.pubkey(),
            escrow_mint: ctx.settlement,
            bid_mint: ctx.settlement,
            auction: keys.auction,
            escrow_vault: keys.escrow_vault,
            bid_vault: keys.bid_vault,
            escrow_source: proceeds,
            proceeds_token: proceeds,
            refund_token: refund,
            bucket,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: event_authority(&venue_id()),
            program: venue_id(),
        }
        .to_account_metas(None),
    );
    ctx.send(&seller_kp, &[ix], &[]).unwrap();
    assert_eq!(ctx.balance(&keys.escrow_vault), 51);

    ctx.bid(&mm1, &keys, 500, None).unwrap();
    let auction: Auction = ctx.read(&keys.auction);
    ctx.warp_to_ms(auction.deadline_ms);

    // Settle: winner gets 101 put coins, seller gets Position + premium.
    let admin = ctx.admin.insecure_clone();
    let position = Keypair::new();
    let put_dest = ctx.fund(&mm1.pubkey(), &put_mint, 0);
    let treasury_token = ctx.fund(&ctx.core_treasury.clone(), &ctx.settlement.clone(), 0);
    let ix = Instruction::new_with_bytes(
        venue_id(),
        &auction_venue::instruction::SettlePut {}.data(),
        auction_venue::accounts::SettlePut {
            cranker: admin.pubkey(),
            creator_wallet: auction.creator,
            auction: keys.auction,
            escrow_vault: keys.escrow_vault,
            bid_vault: keys.bid_vault,
            authority: None,
            proceeds_token: auction.proceeds_token,
            refund_token: auction.refund_token,
            bucket,
            position: position.pubkey(),
            settlement_vault: ata(&bucket, &ctx.settlement),
            put_mint,
            put_dest,
            core_config: ctx.core_config,
            core_treasury_token: treasury_token,
            core_event_authority_acc: event_authority(&core_id()),
            core_program: core_id(),
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: event_authority(&venue_id()),
            program: venue_id(),
        }
        .to_account_metas(None),
    );
    ctx.send(&admin, &[ix], &[&position]).unwrap();

    assert_eq!(ctx.balance(&put_dest), 101);
    let pos: options_core::state::Position = ctx.read(&position.pubkey());
    assert_eq!(pos.owner, seller.pubkey());
    assert_eq!(pos.range_end, 101);
    let core_bucket: options_core::state::PutBucket = ctx.read(&bucket);
    assert_eq!(core_bucket.total_written, 101);
    // Seller: paid 51 collateral, got 500 premium back (fee 0).
    assert_eq!(
        ctx.balance(&ata(&seller.pubkey(), &ctx.settlement)),
        1_000 - 51 + 500
    );
}

#[test]
fn swap_auction_standalone_and_force_refund_gating() {
    let mut ctx = Ctx::setup();
    let seller = ctx.seller.insecure_clone();
    let mm1 = ctx.mm1.insecure_clone();
    // Seller swaps 1_000 settlement for underlying bids.
    ctx.fund(&seller.pubkey(), &ctx.settlement.clone(), 1_000);
    ctx.fund(&seller.pubkey(), &ctx.underlying.clone(), 0);
    ctx.fund(&mm1.pubkey(), &ctx.underlying.clone(), 10_000);

    let params = default_params(seller.pubkey(), 400);
    let keys = ctx.auction_keys(&seller.pubkey(), 9);
    let ix = Instruction::new_with_bytes(
        venue_id(),
        &auction_venue::instruction::CreateSwapAuction {
            salt: 9,
            escrow_amount: 1_000,
            params,
        }
        .data(),
        auction_venue::accounts::CreateAuction {
            creator: seller.pubkey(),
            escrow_mint: ctx.settlement,
            bid_mint: ctx.underlying,
            auction: keys.auction,
            escrow_vault: keys.escrow_vault,
            bid_vault: keys.bid_vault,
            escrow_source: ata(&seller.pubkey(), &ctx.settlement),
            proceeds_token: ata(&seller.pubkey(), &ctx.underlying),
            refund_token: ata(&seller.pubkey(), &ctx.settlement),
            // Pure swaps have no bucket; any account works as placeholder.
            bucket: keys.auction,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: event_authority(&venue_id()),
            program: venue_id(),
        }
        .to_account_metas(None),
    );
    ctx.send(&seller, &[ix], &[]).unwrap();

    ctx.bid(&mm1, &keys, 500, None).unwrap();
    let auction: Auction = ctx.read(&keys.auction);
    ctx.warp_to_ms(auction.deadline_ms);

    let admin = ctx.admin.insecure_clone();
    let (settlement, underlying) = (ctx.settlement, ctx.underlying);
    let settle = |force_refund: bool| {
        Instruction::new_with_bytes(
            venue_id(),
            &auction_venue::instruction::SettleSwap { force_refund }.data(),
            auction_venue::accounts::SettleSwap {
                cranker: admin.pubkey(),
                creator_wallet: auction.creator,
                auction: keys.auction,
                escrow_vault: keys.escrow_vault,
                bid_vault: keys.bid_vault,
                authority: None,
                winner_dest: Some(ata(&mm1.pubkey(), &settlement)),
                bidder_refund: Some(ata(&mm1.pubkey(), &underlying)),
                proceeds_token: auction.proceeds_token,
                refund_token: auction.refund_token,
                token_program: anchor_spl::token::ID,
                event_authority: event_authority(&venue_id()),
                program: venue_id(),
            }
            .to_account_metas(None),
        )
    };

    // Uncoupled swaps can never be force-refunded.
    ctx.fund(&mm1.pubkey(), &ctx.settlement.clone(), 0);
    let ix = settle(true);
    let result = ctx.send(&admin, &[ix], &[]);
    assert_venue_err(
        result,
        auction_venue::error::VenueError::ForceRefundUnauthorized,
    );

    // Normal fill: winner takes the settlement escrow, seller takes the
    // underlying bid.
    let ix = settle(false);
    ctx.send(&admin, &[ix], &[]).unwrap();
    assert_eq!(ctx.balance(&ata(&mm1.pubkey(), &ctx.settlement)), 1_000);
    assert_eq!(ctx.balance(&ata(&seller.pubkey(), &ctx.underlying)), 500);
    assert!(!ctx.exists(&keys.auction));
}
