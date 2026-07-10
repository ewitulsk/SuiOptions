//! Call-bucket lifecycle tests — ports the core of
//! `contracts/tests/bucket_tests.move` (cursor math, strike rounding,
//! expiry gates, invalidation, cleanup).

mod common;

use common::*;
use options_core::error::CoreError;
use options_core::state::{Bucket, Position};
use solana_signer::Signer;

const DAY_MS: u64 = 86_400_000;

fn default_expiry() -> u64 {
    GENESIS_MS + DAY_MS
}

/// strike 5 / scale 1 = 0.5 settlement smallest-units per underlying
/// smallest-unit — exercises the round-half-up path.
const STRIKE: u128 = 5;
const SCALE: u8 = 1;

#[test]
fn create_bucket_initializes_state() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();

    let bucket: Bucket = ctx.read(&keys.bucket);
    assert_eq!(bucket.underlying_mint, ctx.underlying_mint);
    assert_eq!(bucket.settlement_mint, ctx.settlement_mint);
    assert_eq!(bucket.call_mint, keys.call_mint);
    assert_eq!(bucket.expiry_ms, default_expiry());
    assert_eq!(bucket.strike, STRIKE);
    assert_eq!(bucket.strike_scale, SCALE);
    assert_eq!(bucket.total_written, 0);
    assert_eq!(bucket.exercise_cursor, 0);
    assert!(!bucket.invalidated);

    // Fresh mint: zero supply, bucket is the authority, decimals mirror
    // the underlying (the Sui fresh-TreasuryCap invariant by construction).
    let mint: anchor_spl::token::Mint = ctx.read(&keys.call_mint);
    assert_eq!(mint.supply, 0);
    assert_eq!(mint.decimals, UNDERLYING_DECIMALS);
    assert_eq!(mint.mint_authority.unwrap(), keys.bucket);
}

#[test]
fn create_bucket_rejects_oversized_strike_scale() {
    let mut ctx = TestCtx::setup();
    let result = ctx.new_bucket(0, default_expiry(), STRIKE, 39).map(|_| ());
    assert_core_err(result, CoreError::StrikeScaleTooLarge);
}

#[test]
fn create_bucket_requires_admin() {
    let mut ctx = TestCtx::setup();
    // Rebuild the create ix but signed by a stranger.
    let stranger = ctx.stranger.insecure_clone();
    let bucket = bucket_pda(&ctx.underlying_mint, &ctx.settlement_mint, 7);
    let call_mint = call_mint_pda(&bucket);
    let ix = anchor_lang::solana_program::instruction::Instruction::new_with_bytes(
        program_id(),
        &anchor_lang::InstructionData::data(&options_core::instruction::CreateBucket {
            salt: 7,
            expiry_ms: default_expiry(),
            strike: STRIKE,
            strike_scale: SCALE,
        }),
        anchor_lang::ToAccountMetas::to_account_metas(
            &options_core::accounts::CreateBucket {
                admin: stranger.pubkey(),
                config: ctx.config,
                underlying_mint: ctx.underlying_mint,
                settlement_mint: ctx.settlement_mint,
                bucket,
                call_mint,
                underlying_vault: ata(&bucket, &ctx.underlying_mint),
                settlement_vault: ata(&bucket, &ctx.settlement_mint),
                token_program: anchor_spl::token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: ctx.event_authority,
                program: program_id(),
            },
            None,
        ),
    );
    let result = ctx.send(&stranger, &[ix], &[]);
    assert_core_err(result, CoreError::NotOwner);
}

#[test]
fn write_advances_cursor_and_mints_position_and_calls() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);

    let pos1 = ctx.write_collateralized(&writer, &keys, 100).unwrap();
    let bucket: Bucket = ctx.read(&keys.bucket);
    assert_eq!(bucket.total_written, 100);
    let position: Position = ctx.read(&pos1);
    assert_eq!(position.owner, writer.pubkey());
    assert_eq!(position.bucket, keys.bucket);
    assert_eq!(position.range_start, 0);
    assert_eq!(position.range_end, 100);
    assert_eq!(ctx.token_balance(&keys.underlying_vault), 100);
    assert_eq!(ctx.token_balance(&ata(&writer.pubkey(), &keys.call_mint)), 100);

    // Second write occupies the next contiguous range.
    let pos2 = ctx.write_collateralized(&writer, &keys, 50).unwrap();
    let position2: Position = ctx.read(&pos2);
    assert_eq!(position2.range_start, 100);
    assert_eq!(position2.range_end, 150);
    let bucket: Bucket = ctx.read(&keys.bucket);
    assert_eq!(bucket.total_written, 150);

    // Coin supply == outstanding options.
    let mint: anchor_spl::token::Mint = ctx.read(&keys.call_mint);
    assert_eq!(mint.supply, 150);
}

#[test]
fn write_zero_amount_fails() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    let result = ctx.write_collateralized(&writer, &keys, 0).map(|_| ());
    assert_core_err(result, CoreError::ZeroAmount);
}

#[test]
fn write_after_expiry_fails() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    ctx.warp_to_ms(default_expiry());
    let result = ctx.write_collateralized(&writer, &keys, 100).map(|_| ());
    assert_core_err(result, CoreError::BucketExpired);
}

#[test]
fn invalidation_blocks_writes_but_not_exercise_or_redeem() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    let pos = ctx.write_collateralized(&writer, &keys, 100).unwrap();

    ctx.toggle_validity(&keys, true, "test freeze").unwrap();
    let bucket: Bucket = ctx.read(&keys.bucket);
    assert!(bucket.invalidated);

    // New writes are frozen…
    let result = ctx.write_collateralized(&writer, &keys, 10).map(|_| ());
    assert_core_err(result, CoreError::BucketInvalidated);

    // …but exercise still works (invalidation only blocks writes).
    ctx.exercise(&writer, &keys, 10).unwrap();

    // Revalidate unblocks writes.
    ctx.toggle_validity(&keys, false, "resolved").unwrap();
    ctx.write_collateralized(&writer, &keys, 10).unwrap();

    // Redeem after expiry works regardless.
    ctx.warp_to_ms(default_expiry());
    ctx.redeem(&writer, &keys, &pos).unwrap();
}

#[test]
fn exercise_rounds_half_up_and_advances_cursor() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    ctx.write_collateralized(&writer, &keys, 100).unwrap();

    let settlement_before = ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint));
    let underlying_before = ctx.token_balance(&ata(&writer.pubkey(), &ctx.underlying_mint));

    // 1 unit at strike 0.5 → round_half_up(0.5) = 1 settlement unit.
    ctx.exercise(&writer, &keys, 1).unwrap();
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint)),
        settlement_before - 1
    );
    // 10 units → 5 settlement units exactly.
    ctx.exercise(&writer, &keys, 10).unwrap();
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint)),
        settlement_before - 6
    );
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.underlying_mint)),
        underlying_before + 11
    );

    let bucket: Bucket = ctx.read(&keys.bucket);
    assert_eq!(bucket.exercise_cursor, 11);
    assert_eq!(ctx.token_balance(&keys.settlement_vault), 6);
    assert_eq!(ctx.token_balance(&keys.underlying_vault), 89);
    // Burned: supply drops with the cursor.
    let mint: anchor_spl::token::Mint = ctx.read(&keys.call_mint);
    assert_eq!(mint.supply, 89);
}

#[test]
fn exercise_past_total_written_fails() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    ctx.write_collateralized(&writer, &keys, 100).unwrap();

    // Give the writer more call tokens than the bucket has written by
    // writing into a second bucket — impossible; instead try to exercise
    // more than held (token burn would fail first) vs. more than written.
    // Simplest cursor-overflow proof: exercise 100 twice.
    ctx.exercise(&writer, &keys, 100).unwrap();
    let result = ctx.exercise(&writer, &keys, 1);
    assert_core_err(result, CoreError::CursorOverflow);
}

#[test]
fn exercise_after_expiry_fails() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    ctx.write_collateralized(&writer, &keys, 100).unwrap();
    ctx.warp_to_ms(default_expiry());
    let result = ctx.exercise(&writer, &keys, 10);
    assert_core_err(result, CoreError::BucketExpired);
}

#[test]
fn redeem_splits_by_cursor_fifo() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    let trader = ctx.trader.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    ctx.fund_token(&trader.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    ctx.fund_token(&trader.pubkey(), &ctx.settlement_mint.clone(), 1_000);

    // writer occupies [0, 100), trader occupies [100, 200).
    let pos_writer = ctx.write_collateralized(&writer, &keys, 100).unwrap();
    let pos_trader = ctx.write_collateralized(&trader, &keys, 100).unwrap();

    // Trader exercises 150 of the 200 total (needs writer's coins too —
    // simulate a coin transfer by exercising from both accounts: writer
    // exercises 100, trader 50 → cursor 150).
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    ctx.exercise(&writer, &keys, 100).unwrap();
    ctx.exercise(&trader, &keys, 50).unwrap();

    ctx.warp_to_ms(default_expiry());

    // Writer's range [0,100) is fully exercised: 0 underlying back,
    // 100 × 0.5 = 50 settlement.
    let w_u_before = ctx.token_balance(&ata(&writer.pubkey(), &ctx.underlying_mint));
    let w_s_before = ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint));
    ctx.redeem(&writer, &keys, &pos_writer).unwrap();
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.underlying_mint)),
        w_u_before
    );
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint)),
        w_s_before + 50
    );
    // Position account is gone (rent refunded).
    assert!(!ctx.account_exists(&pos_writer));

    // Trader's range [100,200): 50 exercised, 50 unexercised →
    // 50 underlying + 25 settlement.
    let t_u_before = ctx.token_balance(&ata(&trader.pubkey(), &ctx.underlying_mint));
    let t_s_before = ctx.token_balance(&ata(&trader.pubkey(), &ctx.settlement_mint));
    ctx.redeem(&trader, &keys, &pos_trader).unwrap();
    assert_eq!(
        ctx.token_balance(&ata(&trader.pubkey(), &ctx.underlying_mint)),
        t_u_before + 50
    );
    assert_eq!(
        ctx.token_balance(&ata(&trader.pubkey(), &ctx.settlement_mint)),
        t_s_before + 25
    );

    // Bucket fully drained: 150 exercised × 0.5 = 75 in, 75 paid out.
    assert_eq!(ctx.token_balance(&keys.underlying_vault), 0);
    assert_eq!(ctx.token_balance(&keys.settlement_vault), 0);
}

#[test]
fn redeem_before_expiry_fails() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    let pos = ctx.write_collateralized(&writer, &keys, 100).unwrap();
    let result = ctx.redeem(&writer, &keys, &pos);
    assert_core_err(result, CoreError::BucketNotExpired);
}

#[test]
fn redeem_by_non_owner_fails_until_transferred() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    let stranger = ctx.stranger.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    let pos = ctx.write_collateralized(&writer, &keys, 100).unwrap();
    ctx.warp_to_ms(default_expiry());

    let result = ctx.redeem(&stranger, &keys, &pos);
    assert_core_err(result, CoreError::NotOwner);

    // Transfer preserves Sui Position transferability; then redeem works.
    ctx.transfer_position(&writer, &pos, &stranger.pubkey()).unwrap();
    ctx.redeem(&stranger, &keys, &pos).unwrap();
}

#[test]
fn burn_expired_option_gates_on_expiry() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    ctx.write_collateralized(&writer, &keys, 100).unwrap();

    let result = ctx.burn_expired(&writer, &keys, 100);
    assert_core_err(result, CoreError::BucketNotExpired);

    ctx.warp_to_ms(default_expiry());
    ctx.burn_expired(&writer, &keys, 100).unwrap();
    let mint: anchor_spl::token::Mint = ctx.read(&keys.call_mint);
    assert_eq!(mint.supply, 0);
}

#[test]
fn cleanup_requires_expiry_and_drained_vaults() {
    let mut ctx = TestCtx::setup();
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    ctx.fund_token(&writer.pubkey(), &ctx.underlying_mint.clone(), 1_000);
    ctx.fund_token(&writer.pubkey(), &ctx.settlement_mint.clone(), 1_000);
    let pos = ctx.write_collateralized(&writer, &keys, 100).unwrap();

    // Not expired yet.
    let result = ctx.cleanup_bucket(&keys);
    assert_core_err(result, CoreError::BucketNotExpired);

    ctx.warp_to_ms(default_expiry());
    // Expired but not drained.
    let result = ctx.cleanup_bucket(&keys);
    assert_core_err(result, CoreError::BucketNotDrained);

    // Drain: redeem the (fully unexercised) position, burn the coins.
    ctx.redeem(&writer, &keys, &pos).unwrap();
    ctx.burn_expired(&writer, &keys, 100).unwrap();
    ctx.cleanup_bucket(&keys).unwrap();

    // Bucket account closed; mint authority handed to the admin.
    assert!(!ctx.account_exists(&keys.bucket));
    let mint: anchor_spl::token::Mint = ctx.read(&keys.call_mint);
    assert_eq!(mint.mint_authority.unwrap(), ctx.admin.pubkey());
}
