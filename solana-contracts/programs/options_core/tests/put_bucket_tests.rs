//! Cash-secured put tests — ports the core of
//! `contracts/tests/put_bucket_tests.move`: ceil-in/floor-out rounding,
//! the flipped asset legs, the solvency/dust property, and the
//! total_redeemed cleanup gate.

mod common;

use common::*;
use options_core::error::CoreError;
use options_core::quote::{FlowKind, Quote};
use options_core::state::{Position, PutBucket, SCHEME_ED25519};
use solana_signer::Signer;

const DAY_MS: u64 = 86_400_000;
/// strike 5 / scale 1 = 0.5 cash units per underlying unit — every
/// fractional-strike rounding path is live.
const STRIKE: u128 = 5;
const SCALE: u8 = 1;

fn default_expiry() -> u64 {
    GENESIS_MS + DAY_MS
}

#[test]
fn put_write_escrows_ceil_collateral_and_cursor_counts_underlying() {
    let mut ctx = TestCtx::setup();
    let keys = ctx
        .new_put_bucket(0, default_expiry(), STRIKE, SCALE)
        .unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);

    // 3 underlying-units at strike 0.5 → ceil(1.5) = 2 cash collateral.
    let pos = ctx.write_put(&writer, &keys, 3).unwrap();
    assert_eq!(ctx.token_balance(&keys.settlement_vault), 2);
    // Cursor is denominated in UNDERLYING units, not collateral.
    let bucket: PutBucket = ctx.read(&keys.bucket);
    assert_eq!(bucket.total_written, 3);
    let position: Position = ctx.read(&pos);
    assert_eq!(position.range_start, 0);
    assert_eq!(position.range_end, 3);
    // Put coins minted 1:1 with the notional.
    assert_eq!(ctx.token_balance(&ata(&writer.pubkey(), &keys.put_mint)), 3);
}

#[test]
fn put_exercise_delivers_underlying_for_floored_cash() {
    let mut ctx = TestCtx::setup();
    let keys = ctx
        .new_put_bucket(0, default_expiry(), STRIKE, SCALE)
        .unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 100);
    ctx.write_put(&writer, &keys, 100).unwrap(); // collateral = 50

    let cash_before = ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint));

    // Exercise 1 unit: deliver 1 underlying, floor(0.5) = 0 cash out —
    // the dust-favoring direction that keeps the bucket solvent.
    ctx.exercise_put(&writer, &keys, 1).unwrap();
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint)),
        cash_before
    );
    assert_eq!(ctx.token_balance(&keys.underlying_vault), 1);

    // Exercise 10 more: floor(5.0) = 5 cash out.
    ctx.exercise_put(&writer, &keys, 10).unwrap();
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint)),
        cash_before + 5
    );

    let bucket: PutBucket = ctx.read(&keys.bucket);
    assert_eq!(bucket.exercise_cursor, 11);
    // Puts burned with the cursor.
    let mint: anchor_spl::token::Mint = ctx.read(&keys.put_mint);
    assert_eq!(mint.supply, 89);
}

#[test]
fn put_exercise_cursor_overflow_fails() {
    let mut ctx = TestCtx::setup();
    let keys = ctx
        .new_put_bucket(0, default_expiry(), STRIKE, SCALE)
        .unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 200);
    ctx.write_put(&writer, &keys, 100).unwrap();

    ctx.exercise_put(&writer, &keys, 100).unwrap();
    let result = ctx.exercise_put(&writer, &keys, 1);
    assert_core_err(result, CoreError::CursorOverflow);
}

#[test]
fn put_redeem_splits_exercised_underlying_and_floored_cash() {
    let mut ctx = TestCtx::setup();
    let keys = ctx
        .new_put_bucket(0, default_expiry(), STRIKE, SCALE)
        .unwrap();
    let writer = ctx.writer.insecure_clone();
    let trader = ctx.trader.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    ctx.fund_token(&trader.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 200);
    ctx.fund_token(&trader.pubkey(), &ctx.underlying_mint.clone(), 200);

    // writer [0, 100), trader [100, 200); each posted 50 collateral.
    let pos_writer = ctx.write_put(&writer, &keys, 100).unwrap();
    let pos_trader = ctx.write_put(&trader, &keys, 100).unwrap();

    // Cursor to 150: writer's range fully assigned, trader's half.
    ctx.exercise_put(&writer, &keys, 100).unwrap(); // pays out 50 cash
    ctx.exercise_put(&trader, &keys, 50).unwrap(); // pays out 25 cash

    ctx.warp_to_ms(default_expiry());

    // Writer fully exercised: 100 delivered underlying, no cash back.
    let w_u = ctx.token_balance(&ata(&writer.pubkey(), &ctx.underlying_mint));
    let w_s = ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint));
    ctx.redeem_put(&writer, &keys, &pos_writer).unwrap();
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.underlying_mint)),
        w_u + 100
    );
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint)),
        w_s
    );

    // Trader half exercised: 50 underlying + floor(50 × 0.5) = 25 cash.
    let t_u = ctx.token_balance(&ata(&trader.pubkey(), &ctx.underlying_mint));
    let t_s = ctx.token_balance(&ata(&trader.pubkey(), &ctx.settlement_mint));
    ctx.redeem_put(&trader, &keys, &pos_trader).unwrap();
    assert_eq!(
        ctx.token_balance(&ata(&trader.pubkey(), &ctx.underlying_mint)),
        t_u + 50
    );
    assert_eq!(
        ctx.token_balance(&ata(&trader.pubkey(), &ctx.settlement_mint)),
        t_s + 25
    );

    // Solvency held: underlying drains exactly; cash never went negative.
    assert_eq!(ctx.token_balance(&keys.underlying_vault), 0);
    let bucket: PutBucket = ctx.read(&keys.bucket);
    assert_eq!(bucket.total_redeemed, 200);
}

#[test]
fn put_cleanup_gates_on_total_redeemed_and_sweeps_dust() {
    let mut ctx = TestCtx::setup();
    // strike 0.51: write 3 → ceil(1.53) = 2 in; exercise 3 → floor(1.53)
    // = 1 out; redeem-unexercised 0 → dust = 1 stays.
    let keys = ctx.new_put_bucket(0, default_expiry(), 51, 2).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 10);
    let pos = ctx.write_put(&writer, &keys, 3).unwrap();
    assert_eq!(ctx.token_balance(&keys.settlement_vault), 2);

    ctx.exercise_put(&writer, &keys, 3).unwrap(); // floor(1.53) = 1 out
    assert_eq!(ctx.token_balance(&keys.settlement_vault), 1);

    ctx.warp_to_ms(default_expiry());

    // Cleanup before every position is redeemed is rejected.
    let result = ctx.cleanup_put_bucket(&keys);
    assert_core_err(result, CoreError::BucketNotDrained);

    // Redeem: fully exercised → 3 underlying back, 0 cash.
    ctx.redeem_put(&writer, &keys, &pos).unwrap();

    // Now cleanup succeeds and sweeps the 1-unit dust to the admin.
    let admin_settlement = ata(&ctx.admin.pubkey(), &ctx.settlement_mint);
    let admin_cash_before = ctx.token_balance(&admin_settlement);
    ctx.cleanup_put_bucket(&keys).unwrap();
    assert!(!ctx.account_exists(&keys.bucket));
    assert_eq!(ctx.token_balance(&admin_settlement), admin_cash_before + 1);
    let mint: anchor_spl::token::Mint = ctx.read(&keys.put_mint);
    assert_eq!(mint.mint_authority.unwrap(), ctx.admin.pubkey());
}

#[test]
fn put_write_gates_expiry_invalidation_zero() {
    let mut ctx = TestCtx::setup();
    let keys = ctx
        .new_put_bucket(0, default_expiry(), STRIKE, SCALE)
        .unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);

    let result = ctx.write_put(&writer, &keys, 0).map(|_| ());
    assert_core_err(result, CoreError::ZeroAmount);

    // Invalidate: writes frozen (admin path shared with calls).
    let ix = anchor_lang::solana_program::instruction::Instruction::new_with_bytes(
        program_id(),
        &anchor_lang::InstructionData::data(&options_core::instruction::InvalidatePutBucket {
            reason: "freeze".to_string(),
        }),
        anchor_lang::ToAccountMetas::to_account_metas(
            &options_core::accounts::TogglePutBucketValidity {
                admin: ctx.admin.pubkey(),
                config: ctx.config,
                bucket: keys.bucket,
                event_authority: ctx.event_authority,
                program: program_id(),
            },
            None,
        ),
    );
    let admin = ctx.admin.insecure_clone();
    ctx.send(&admin, &[ix], &[]).unwrap();
    let result = ctx.write_put(&writer, &keys, 10).map(|_| ());
    assert_core_err(result, CoreError::BucketInvalidated);

    ctx.warp_to_ms(default_expiry());
    let result = ctx.write_put(&writer, &keys, 10).map(|_| ());
    assert_core_err(result, CoreError::BucketExpired);
}

#[test]
fn execute_put_write_writer_flow() {
    let mut ctx = TestCtx::setup();
    ctx.set_fee_bps(50);
    let keys = ctx
        .new_put_bucket(0, default_expiry(), STRIKE, SCALE)
        .unwrap();
    let sk = mm_signing_key();
    // Trader MM buys the put, paying premium from its account.
    let mm_owner = ctx.trader_mm.insecure_clone();
    let mm_account = ctx
        .create_mm_account(&mm_owner, 0, SCHEME_ED25519, signing_pubkey(&sk))
        .unwrap();
    let settlement = ctx.settlement_mint;
    ctx.fund_token(&mm_owner.pubkey(), &settlement, 1_000_000);
    ctx.mm_deposit(&mm_owner, &mm_account, &settlement, 1_000_000)
        .unwrap();
    // Writer holds cash for the collateral.
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &settlement, 1_000);

    let quote = Quote {
        protocol_id: ctx.config,
        signer_account: mm_account,
        signer_token_recipient: ctx.trader_mm.pubkey(),
        bucket: keys.bucket,
        write_amount: 100,
        premium: 10_000,
        valid_until_ms: GENESIS_MS + 60_000,
        nonce: 1,
    };

    let position = Keypair::new();
    let put_dest = ctx.ensure_ata(&ctx.trader_mm.pubkey().clone(), &keys.put_mint);
    let executor_settlement = ata(&writer.pubkey(), &settlement);
    let sig_ix = ed25519_verify_ix(&quote, &sk);
    let ix = anchor_lang::solana_program::instruction::Instruction::new_with_bytes(
        program_id(),
        &anchor_lang::InstructionData::data(&options_core::instruction::ExecutePutWrite {
            quote: quote.clone(),
            flow: FlowKind::Writer,
            position_recipient: writer.pubkey(),
            sig_ix_index: 0,
        }),
        anchor_lang::ToAccountMetas::to_account_metas(
            &options_core::accounts::ExecutePutWrite {
                executor: writer.pubkey(),
                config: ctx.config,
                treasury: ctx.treasury,
                bucket: keys.bucket,
                settlement_mint: settlement,
                settlement_vault: keys.settlement_vault,
                put_mint: keys.put_mint,
                put_dest,
                mm_account,
                mm_settlement: ata(&mm_account, &settlement),
                executor_settlement,
                treasury_settlement: ata(&ctx.treasury, &settlement),
                position: position.pubkey(),
                nonce_record: nonce_pda(&mm_account, quote.nonce),
                instructions_sysvar: solana_sdk_ids::sysvar::instructions::ID,
                token_program: anchor_spl::token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: ctx.event_authority,
                program: program_id(),
            },
            None,
        ),
    );
    ctx.send(&writer, &[sig_ix, ix], &[&position]).unwrap();

    // Collateral escrowed: ceil(100 × 0.5) = 50; writer paid 50 and
    // received 9_950 net premium (10_000 − 0.5% fee).
    assert_eq!(ctx.token_balance(&keys.settlement_vault), 50);
    assert_eq!(ctx.token_balance(&executor_settlement), 1_000 - 50 + 9_950);
    assert_eq!(ctx.token_balance(&ata(&ctx.treasury, &settlement)), 50);
    // MM got the puts; position belongs to the writer.
    assert_eq!(ctx.token_balance(&put_dest), 100);
    let pos: Position = ctx.read(&position.pubkey());
    assert_eq!(pos.owner, writer.pubkey());
    let bucket: PutBucket = ctx.read(&keys.bucket);
    assert_eq!(bucket.total_written, 100);
}

use solana_keypair::Keypair;
