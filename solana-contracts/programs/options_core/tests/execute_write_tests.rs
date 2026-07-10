//! Quote verification + execute_write tests — ports `quote_tests.move` and
//! the execute_write cases of `bucket_tests.move`, with REAL Ed25519
//! signatures through the native precompile (no `verify_skip_sig` analog
//! needed on Solana).

mod common;

use common::*;
use options_core::error::CoreError;
use options_core::quote::{FlowKind, Quote};
use options_core::state::{Bucket, Position, SCHEME_ED25519};
use solana_signer::Signer;

const DAY_MS: u64 = 86_400_000;
const STRIKE: u128 = 5;
const SCALE: u8 = 1;

fn default_expiry() -> u64 {
    GENESIS_MS + DAY_MS
}

struct QuoteSetup {
    keys: BucketKeys,
    mm_account: anchor_lang::prelude::Pubkey,
    sk: ed25519_dalek::SigningKey,
}

/// Shared fixture: bucket + trader-MM account (signing key registered),
/// funded on both sides.
fn setup_writer_flow(ctx: &mut TestCtx) -> QuoteSetup {
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let sk = mm_signing_key();
    let mm_owner = ctx.trader_mm.insecure_clone();
    let mm_account = ctx
        .create_mm_account(&mm_owner, 0, SCHEME_ED25519, signing_pubkey(&sk))
        .unwrap();
    // MM funds its account with settlement (to pay premiums).
    let settlement = ctx.settlement_mint;
    ctx.fund_token(&mm_owner.pubkey(), &settlement, 1_000_000);
    ctx.mm_deposit(&mm_owner, &mm_account, &settlement, 1_000_000)
        .unwrap();
    // Writer holds underlying.
    let writer = ctx.writer.pubkey();
    ctx.fund_token(&writer, &ctx.underlying_mint.clone(), 1_000);
    QuoteSetup {
        keys,
        mm_account,
        sk,
    }
}

fn writer_quote(ctx: &TestCtx, s: &QuoteSetup, nonce: u64) -> Quote {
    Quote {
        protocol_id: ctx.config,
        signer_account: s.mm_account,
        // Trader MM receives the call coins at their wallet.
        signer_token_recipient: ctx.trader_mm.pubkey(),
        bucket: s.keys.bucket,
        write_amount: 100,
        premium: 10_000,
        valid_until_ms: GENESIS_MS + 60_000,
        nonce,
    }
}

#[test]
fn writer_flow_executes_signed_quote_with_fee() {
    let mut ctx = TestCtx::setup();
    ctx.set_fee_bps(50); // 0.5%
    let s = setup_writer_flow(&mut ctx);
    let writer = ctx.writer.insecure_clone();
    let quote = writer_quote(&ctx, &s, 1);

    let position = ctx
        .execute_write(
            &writer,
            &s.keys,
            &s.mm_account,
            &quote,
            FlowKind::Writer,
            writer.pubkey(),
            &ctx.trader_mm.pubkey().clone(),
            &s.sk,
        )
        .unwrap();

    // Cursor advanced, position minted to the writer over [0, 100).
    let bucket: Bucket = ctx.read(&s.keys.bucket);
    assert_eq!(bucket.total_written, 100);
    let pos: Position = ctx.read(&position);
    assert_eq!(pos.owner, writer.pubkey());
    assert_eq!(pos.range_start, 0);
    assert_eq!(pos.range_end, 100);

    // Underlying escrowed; call coins to the MM's wallet ATA.
    assert_eq!(ctx.token_balance(&s.keys.underlying_vault), 100);
    assert_eq!(
        ctx.token_balance(&ata(&ctx.trader_mm.pubkey(), &s.keys.call_mint)),
        100
    );

    // Premium: 10_000 gross, 50 fee (0.5%), 9_950 net to the writer;
    // MM account debited the gross.
    assert_eq!(
        ctx.token_balance(&ata(&writer.pubkey(), &ctx.settlement_mint)),
        9_950
    );
    assert_eq!(
        ctx.token_balance(&ata(&ctx.treasury, &ctx.settlement_mint)),
        50
    );
    assert_eq!(
        ctx.token_balance(&ata(&s.mm_account, &ctx.settlement_mint)),
        990_000
    );
}

#[test]
fn trader_flow_executes_signed_quote() {
    let mut ctx = TestCtx::setup();
    ctx.set_fee_bps(50);
    let keys = ctx.new_bucket(0, default_expiry(), STRIKE, SCALE).unwrap();
    let sk = mm_signing_key();
    // Writer MM: provides underlying from its account, receives premium +
    // the Position.
    let mm_owner = ctx.writer_mm.insecure_clone();
    let mm_account = ctx
        .create_mm_account(&mm_owner, 0, SCHEME_ED25519, signing_pubkey(&sk))
        .unwrap();
    let underlying = ctx.underlying_mint;
    ctx.fund_token(&mm_owner.pubkey(), &underlying, 1_000);
    ctx.mm_deposit(&mm_owner, &mm_account, &underlying, 1_000)
        .unwrap();
    // The MM's settlement ATA must exist to receive the net premium.
    ctx.ensure_ata(&mm_account, &ctx.settlement_mint.clone());
    // Trader holds settlement to pay the premium.
    let trader = ctx.trader.insecure_clone();
    ctx.fund_token(&trader.pubkey(), &ctx.settlement_mint.clone(), 100_000);

    let quote = Quote {
        protocol_id: ctx.config,
        signer_account: mm_account,
        // Writer MM receives the Position.
        signer_token_recipient: ctx.writer_mm.pubkey(),
        bucket: keys.bucket,
        write_amount: 100,
        premium: 10_000,
        valid_until_ms: GENESIS_MS + 60_000,
        nonce: 7,
    };

    let position = ctx
        .execute_write(
            &trader,
            &keys,
            &mm_account,
            &quote,
            FlowKind::Trader,
            ctx.writer_mm.pubkey(),
            &trader.pubkey(),
            &sk,
        )
        .unwrap();

    // MM's underlying debited into the vault; position owned by the MM.
    assert_eq!(ctx.token_balance(&keys.underlying_vault), 100);
    assert_eq!(ctx.token_balance(&ata(&mm_account, &underlying)), 900);
    let pos: Position = ctx.read(&position);
    assert_eq!(pos.owner, ctx.writer_mm.pubkey());

    // Trader paid gross, got the calls; MM account got net premium.
    assert_eq!(
        ctx.token_balance(&ata(&trader.pubkey(), &keys.call_mint)),
        100
    );
    assert_eq!(
        ctx.token_balance(&ata(&trader.pubkey(), &ctx.settlement_mint)),
        90_000
    );
    assert_eq!(
        ctx.token_balance(&ata(&mm_account, &ctx.settlement_mint)),
        9_950
    );
    assert_eq!(
        ctx.token_balance(&ata(&ctx.treasury, &ctx.settlement_mint)),
        50
    );
}

#[test]
fn nonce_replay_fails() {
    let mut ctx = TestCtx::setup();
    let s = setup_writer_flow(&mut ctx);
    let writer = ctx.writer.insecure_clone();
    let quote = writer_quote(&ctx, &s, 1);

    ctx.execute_write(
        &writer,
        &s.keys,
        &s.mm_account,
        &quote,
        FlowKind::Writer,
        writer.pubkey(),
        &ctx.trader_mm.pubkey().clone(),
        &s.sk,
    )
    .unwrap();

    // Same nonce again: the NonceRecord PDA already exists, so the init
    // fails before the handler runs (Sui's E_QUOTE_NONCE_USED analog).
    let result = ctx
        .execute_write(
            &writer,
            &s.keys,
            &s.mm_account,
            &quote,
            FlowKind::Writer,
            writer.pubkey(),
            &ctx.trader_mm.pubkey().clone(),
            &s.sk,
        )
        .map(|_| ());
    assert!(result.is_err(), "nonce replay must fail");

    // A fresh nonce still works.
    let quote2 = writer_quote(&ctx, &s, 2);
    ctx.execute_write(
        &writer,
        &s.keys,
        &s.mm_account,
        &quote2,
        FlowKind::Writer,
        writer.pubkey(),
        &ctx.trader_mm.pubkey().clone(),
        &s.sk,
    )
    .unwrap();
}

#[test]
fn expired_quote_fails() {
    let mut ctx = TestCtx::setup();
    let s = setup_writer_flow(&mut ctx);
    let writer = ctx.writer.insecure_clone();
    let quote = writer_quote(&ctx, &s, 1);
    ctx.warp_to_ms(quote.valid_until_ms);
    let result = ctx
        .execute_write(
            &writer,
            &s.keys,
            &s.mm_account,
            &quote,
            FlowKind::Writer,
            writer.pubkey(),
            &ctx.trader_mm.pubkey().clone(),
            &s.sk,
        )
        .map(|_| ());
    assert_core_err(result, CoreError::QuoteExpired);
}

#[test]
fn wrong_signing_key_fails() {
    let mut ctx = TestCtx::setup();
    let s = setup_writer_flow(&mut ctx);
    let writer = ctx.writer.insecure_clone();
    let quote = writer_quote(&ctx, &s, 1);

    // Signed by a key that is NOT the account's registered pubkey: the
    // precompile passes (valid signature!) but the pubkey pinning fails.
    let rogue = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
    let result = ctx
        .execute_write(
            &writer,
            &s.keys,
            &s.mm_account,
            &quote,
            FlowKind::Writer,
            writer.pubkey(),
            &ctx.trader_mm.pubkey().clone(),
            &rogue,
        )
        .map(|_| ());
    assert_core_err(result, CoreError::QuoteSignatureInvalid);
}

#[test]
fn tampered_quote_fails() {
    let mut ctx = TestCtx::setup();
    let s = setup_writer_flow(&mut ctx);
    let writer = ctx.writer.insecure_clone();

    // MM signed a 10_000 premium; executor submits 5_000 — the verified
    // message no longer matches the instruction's quote bytes.
    let signed = writer_quote(&ctx, &s, 1);
    let mut submitted = signed.clone();
    submitted.premium = 5_000;

    let sig_ix = ed25519_verify_ix(&signed, &s.sk);
    let result = ctx
        .execute_write_with_sig_ix(
            &writer,
            &s.keys,
            &s.mm_account,
            &submitted,
            FlowKind::Writer,
            writer.pubkey(),
            &ctx.trader_mm.pubkey().clone(),
            Some(sig_ix),
        )
        .map(|_| ());
    assert_core_err(result, CoreError::QuoteSignatureInvalid);
}

#[test]
fn missing_sig_instruction_fails() {
    let mut ctx = TestCtx::setup();
    let s = setup_writer_flow(&mut ctx);
    let writer = ctx.writer.insecure_clone();
    let quote = writer_quote(&ctx, &s, 1);

    // No precompile instruction at index 0 — index 0 is execute_write
    // itself, which is not the ed25519 program.
    let result = ctx
        .execute_write_with_sig_ix(
            &writer,
            &s.keys,
            &s.mm_account,
            &quote,
            FlowKind::Writer,
            writer.pubkey(),
            &ctx.trader_mm.pubkey().clone(),
            None,
        )
        .map(|_| ());
    assert_core_err(result, CoreError::MissingSigVerification);
}

#[test]
fn quote_for_other_bucket_fails() {
    let mut ctx = TestCtx::setup();
    let s = setup_writer_flow(&mut ctx);
    let other_keys = ctx.new_bucket(1, default_expiry(), STRIKE, SCALE).unwrap();
    let writer = ctx.writer.insecure_clone();
    // Quote pinned to bucket 0, executed against bucket 1.
    let quote = writer_quote(&ctx, &s, 1);
    let result = ctx
        .execute_write(
            &writer,
            &other_keys,
            &s.mm_account,
            &quote,
            FlowKind::Writer,
            writer.pubkey(),
            &ctx.trader_mm.pubkey().clone(),
            &s.sk,
        )
        .map(|_| ());
    assert_core_err(result, CoreError::QuoteBucketMismatch);
}

#[test]
fn writer_flow_call_recipient_must_match_quote() {
    let mut ctx = TestCtx::setup();
    let s = setup_writer_flow(&mut ctx);
    let writer = ctx.writer.insecure_clone();
    let quote = writer_quote(&ctx, &s, 1);
    // Executor tries to route the MM's call coins to themselves.
    let result = ctx
        .execute_write(
            &writer,
            &s.keys,
            &s.mm_account,
            &quote,
            FlowKind::Writer,
            writer.pubkey(),
            &writer.pubkey().clone(),
            &s.sk,
        )
        .map(|_| ());
    assert_core_err(result, CoreError::QuoteRecipientMismatch);
}

#[test]
fn prune_nonce_after_expiry_only() {
    let mut ctx = TestCtx::setup();
    let s = setup_writer_flow(&mut ctx);
    let writer = ctx.writer.insecure_clone();
    let stranger = ctx.stranger.insecure_clone();
    let quote = writer_quote(&ctx, &s, 1);
    ctx.execute_write(
        &writer,
        &s.keys,
        &s.mm_account,
        &quote,
        FlowKind::Writer,
        writer.pubkey(),
        &ctx.trader_mm.pubkey().clone(),
        &s.sk,
    )
    .unwrap();

    let nonce_record = nonce_pda(&s.mm_account, 1);
    assert!(ctx.account_exists(&nonce_record));

    let prune = |ctx: &mut TestCtx, caller: &solana_keypair::Keypair| {
        let ix = anchor_lang::solana_program::instruction::Instruction::new_with_bytes(
            program_id(),
            &anchor_lang::InstructionData::data(&options_core::instruction::PruneNonce {}),
            anchor_lang::ToAccountMetas::to_account_metas(
                &options_core::accounts::PruneNonce {
                    caller: caller.pubkey(),
                    nonce_record,
                },
                None,
            ),
        );
        ctx.send(caller, &[ix], &[])
    };

    // Still valid: anyone-may-prune is gated on expiry.
    let result = prune(&mut ctx, &stranger);
    assert_core_err(result, CoreError::NonceStillValid);

    // After valid_until_ms passes, a stranger prunes and pockets the rent.
    ctx.warp_to_ms(quote.valid_until_ms + 1_000);
    prune(&mut ctx, &stranger).unwrap();
    assert!(!ctx.account_exists(&nonce_record));
}
