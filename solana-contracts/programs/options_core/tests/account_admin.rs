//! MM account + admin/treasury tests — ports `account_tests.move`,
//! `admin_tests.move`, `treasury_tests.move`.

mod common;

use anchor_lang::{
    solana_program::instruction::Instruction, InstructionData, ToAccountMetas,
};
use common::*;
use options_core::error::CoreError;
use options_core::state::{Config, MmAccount, SCHEME_ED25519};
use solana_signer::Signer;

/// RFC 8032 Ed25519 test-vector pubkey — the same bytes
/// `test_helpers.move::pubkey_a()` uses.
pub fn pubkey_a() -> Vec<u8> {
    hex_to_bytes("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn create_account_stores_key_and_owner() {
    let mut ctx = TestCtx::setup();
    let mm_owner = ctx.trader_mm.insecure_clone();
    let mm_account = ctx
        .create_mm_account(&mm_owner, 0, SCHEME_ED25519, pubkey_a())
        .unwrap();
    let account: MmAccount = ctx.read(&mm_account);
    assert_eq!(account.owner, mm_owner.pubkey());
    assert_eq!(account.signing_scheme, SCHEME_ED25519);
    assert_eq!(account.signing_pubkey, pubkey_a());
}

#[test]
fn create_account_rejects_bad_scheme_and_key_length() {
    let mut ctx = TestCtx::setup();
    let mm_owner = ctx.trader_mm.insecure_clone();
    let result = ctx
        .create_mm_account(&mm_owner, 0, 9, pubkey_a())
        .map(|_| ());
    assert_core_err(result, CoreError::InvalidSigningScheme);

    let result = ctx
        .create_mm_account(&mm_owner, 1, SCHEME_ED25519, vec![0u8; 31])
        .map(|_| ());
    assert_core_err(result, CoreError::InvalidPubkeyLength);
}

#[test]
fn deposit_withdraw_roundtrip() {
    let mut ctx = TestCtx::setup();
    let mm_owner = ctx.trader_mm.insecure_clone();
    let settlement = ctx.settlement_mint;
    let mm_account = ctx
        .create_mm_account(&mm_owner, 0, SCHEME_ED25519, pubkey_a())
        .unwrap();
    ctx.fund_token(&mm_owner.pubkey(), &settlement, 1_000_000);

    ctx.mm_deposit(&mm_owner, &mm_account, &settlement, 600_000)
        .unwrap();
    assert_eq!(ctx.token_balance(&ata(&mm_account, &settlement)), 600_000);

    ctx.mm_withdraw(&mm_owner, &mm_account, &settlement, 200_000)
        .unwrap();
    assert_eq!(ctx.token_balance(&ata(&mm_account, &settlement)), 400_000);
    assert_eq!(
        ctx.token_balance(&ata(&mm_owner.pubkey(), &settlement)),
        600_000
    );
}

#[test]
fn withdraw_by_non_owner_fails() {
    let mut ctx = TestCtx::setup();
    let mm_owner = ctx.trader_mm.insecure_clone();
    let stranger = ctx.stranger.insecure_clone();
    let settlement = ctx.settlement_mint;
    let mm_account = ctx
        .create_mm_account(&mm_owner, 0, SCHEME_ED25519, pubkey_a())
        .unwrap();
    ctx.fund_token(&mm_owner.pubkey(), &settlement, 1_000);
    ctx.mm_deposit(&mm_owner, &mm_account, &settlement, 1_000)
        .unwrap();

    let result = ctx.mm_withdraw(&stranger, &mm_account, &settlement, 500);
    assert_core_err(result, CoreError::NotOwner);
}

#[test]
fn withdraw_more_than_balance_fails() {
    let mut ctx = TestCtx::setup();
    let mm_owner = ctx.trader_mm.insecure_clone();
    let settlement = ctx.settlement_mint;
    let mm_account = ctx
        .create_mm_account(&mm_owner, 0, SCHEME_ED25519, pubkey_a())
        .unwrap();
    ctx.fund_token(&mm_owner.pubkey(), &settlement, 1_000);
    ctx.mm_deposit(&mm_owner, &mm_account, &settlement, 1_000)
        .unwrap();

    let result = ctx.mm_withdraw(&mm_owner, &mm_account, &settlement, 1_001);
    assert_core_err(result, CoreError::InsufficientAccountBalance);
}

#[test]
fn rotate_signing_key_owner_only() {
    let mut ctx = TestCtx::setup();
    let mm_owner = ctx.trader_mm.insecure_clone();
    let stranger = ctx.stranger.insecure_clone();
    let mm_account = ctx
        .create_mm_account(&mm_owner, 0, SCHEME_ED25519, pubkey_a())
        .unwrap();

    let new_key = vec![7u8; 32];
    let (event_authority, prog) = (ctx.event_authority, program_id());
    let rotate = |signer: &solana_keypair::Keypair, key: Vec<u8>| {
        Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::RotateSigningKey {
                new_scheme: SCHEME_ED25519,
                new_pubkey: key,
            }
            .data(),
            options_core::accounts::RotateSigningKey {
                owner: signer.pubkey(),
                mm_account,
                event_authority,
                program: prog,
            }
            .to_account_metas(None),
        )
    };

    let ix = rotate(&stranger, new_key.clone());
    let result = ctx.send(&stranger, &[ix], &[]);
    assert_core_err(result, CoreError::NotOwner);

    let ix = rotate(&mm_owner, new_key.clone());
    ctx.send(&mm_owner, &[ix], &[]).unwrap();
    let account: MmAccount = ctx.read(&mm_account);
    assert_eq!(account.signing_pubkey, new_key);
}

#[test]
fn set_fee_bps_caps_and_gates_on_admin() {
    let mut ctx = TestCtx::setup();
    let admin = ctx.admin.insecure_clone();
    let stranger = ctx.stranger.insecure_clone();

    let (config, event_authority) = (ctx.config, ctx.event_authority);
    let set_fee = |signer: &solana_keypair::Keypair, bps: u64| {
        Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::SetFeeBps { new_bps: bps }.data(),
            options_core::accounts::AdminConfig {
                admin: signer.pubkey(),
                config,
                event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        )
    };

    // Non-admin rejected.
    let ix = set_fee(&stranger, 50);
    let result = ctx.send(&stranger, &[ix], &[]);
    assert_core_err(result, CoreError::NotOwner);

    // Over the cap rejected (MAX_FEE_BPS = 1000, mirrors admin.move).
    let ix = set_fee(&admin, 1_001);
    let result = ctx.send(&admin, &[ix], &[]);
    assert_core_err(result, CoreError::FeeTooHigh);

    let ix = set_fee(&admin, 50);
    ctx.send(&admin, &[ix], &[]).unwrap();
    let config: Config = ctx.read(&ctx.config.clone());
    assert_eq!(config.fee_bps, 50);
}

#[test]
fn treasury_deposit_and_admin_withdraw() {
    let mut ctx = TestCtx::setup();
    let admin = ctx.admin.insecure_clone();
    let stranger = ctx.stranger.insecure_clone();
    let settlement = ctx.settlement_mint;
    ctx.fund_token(&stranger.pubkey(), &settlement, 10_000);

    // Anyone may pay the treasury (the venue fee-routing surface).
    let ix = Instruction::new_with_bytes(
        program_id(),
        &options_core::instruction::DepositProtocolFee { amount: 4_000 }.data(),
        options_core::accounts::DepositProtocolFee {
            payer: stranger.pubkey(),
            treasury: ctx.treasury,
            mint: settlement,
            from_token: ata(&stranger.pubkey(), &settlement),
            treasury_token: ata(&ctx.treasury, &settlement),
            token_program: anchor_spl::token::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: ctx.event_authority,
            program: program_id(),
        }
        .to_account_metas(None),
    );
    ctx.send(&stranger, &[ix], &[]).unwrap();
    assert_eq!(ctx.token_balance(&ata(&ctx.treasury, &settlement)), 4_000);

    let (config, treasury, event_authority) = (ctx.config, ctx.treasury, ctx.event_authority);
    let withdraw = |signer: &solana_keypair::Keypair, amount: u64, to: anchor_lang::prelude::Pubkey| {
        Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::WithdrawTreasury { amount }.data(),
            options_core::accounts::WithdrawTreasury {
                admin: signer.pubkey(),
                config,
                treasury,
                mint: settlement,
                treasury_token: ata(&treasury, &settlement),
                recipient_token: to,
                token_program: anchor_spl::token::ID,
                event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        )
    };

    // Only the admin may withdraw.
    let admin_settlement = ctx.ensure_ata(&admin.pubkey(), &settlement);
    let ix = withdraw(&stranger, 1_000, admin_settlement);
    let result = ctx.send(&stranger, &[ix], &[]);
    assert_core_err(result, CoreError::NotOwner);

    // More than the balance fails.
    let ix = withdraw(&admin, 4_001, admin_settlement);
    let result = ctx.send(&admin, &[ix], &[]);
    assert_core_err(result, CoreError::InsufficientTreasuryBalance);

    let ix = withdraw(&admin, 4_000, admin_settlement);
    ctx.send(&admin, &[ix], &[]).unwrap();
    assert_eq!(ctx.token_balance(&admin_settlement), 4_000);
}
