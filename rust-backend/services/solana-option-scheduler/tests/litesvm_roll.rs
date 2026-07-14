//! Integration test: the scheduler's roll/vault building blocks against the
//! real program binaries under LiteSVM (same harness as solana-tx's smoke
//! test). Skips gracefully when the `.so` files haven't been built
//! (`anchor build` in solana-contracts).

use anchor_lang::AccountDeserialize;
use litesvm::LiteSVM;
use litesvm_token::CreateMint;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;

use solana_option_scheduler::config::VaultTemplate;
use solana_option_scheduler::roller::{self, ProductType, RollPlan};
use solana_option_scheduler::salt;
use solana_option_scheduler::vault_roller::{build_vault_config, VaultPairSpec};

const DEPLOY_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../solana-contracts/target/deploy"
);

fn load_program(svm: &mut LiteSVM, id: Pubkey, name: &str) -> bool {
    let path = format!("{DEPLOY_DIR}/{name}.so");
    match std::fs::read(&path) {
        Ok(bytes) => {
            svm.add_program(id, &bytes).unwrap();
            true
        }
        Err(_) => {
            eprintln!("skipping litesvm roll test: {path} not built");
            false
        }
    }
}

fn send(svm: &mut LiteSVM, payer: &Keypair, ixs: &[solana_sdk::instruction::Instruction]) {
    svm.expire_blockhash();
    let msg = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx = Transaction::new(&[payer], msg, svm.latest_blockhash());
    if let Err(e) = svm.send_transaction(tx) {
        panic!("transaction failed: {:?}\nlogs: {:#?}", e.err, e.meta.logs);
    }
}

/// Send expecting failure; return the joined logs + error debug string.
fn send_expect_err(
    svm: &mut LiteSVM,
    payer: &Keypair,
    ixs: &[solana_sdk::instruction::Instruction],
) -> String {
    svm.expire_blockhash();
    let msg = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx = Transaction::new(&[payer], msg, svm.latest_blockhash());
    match svm.send_transaction(tx) {
        Ok(_) => panic!("expected the transaction to fail"),
        Err(e) => format!("{:?} logs: {:?}", e.err, e.meta.logs),
    }
}

#[test]
fn roll_and_vault_happy_path() {
    let mut svm = LiteSVM::new();
    for (id, name) in [
        (options_core::ID, "options_core"),
        (options_vault::ID, "options_vault"),
    ] {
        if !load_program(&mut svm, id, name) {
            return;
        }
    }

    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();

    // initialize options_core: Config + Treasury; admin becomes the admin.
    send(&mut svm, &admin, &[solana_tx::ix::initialize(&admin.pubkey())]);

    let underlying = CreateMint::new(&mut svm, &admin).decimals(8).send().unwrap();
    let settlement = CreateMint::new(&mut svm, &admin).decimals(6).send().unwrap();

    // ── bucket roll: one create_bucket per strike, deterministic salts ──
    let plan = RollPlan {
        underlying_symbol: "TBTC".into(),
        settlement_symbol: "TUSDC".into(),
        underlying_mint: underlying,
        settlement_mint: settlement,
        expiry_ms: 4_000_000_000_000, // far future vs litesvm's clock
        strikes: vec![61_600, 65_450, 69_300],
        strike_scale: 2,
        product_type: ProductType::Call,
    };
    let salts = plan.bucket_salts();
    let pdas = plan.bucket_pdas();
    for ((&strike, &bucket_salt), pda) in plan.strikes.iter().zip(&salts).zip(&pdas) {
        send(
            &mut svm,
            &admin,
            &[solana_tx::ix::create_bucket(
                &admin.pubkey(),
                &underlying,
                &settlement,
                bucket_salt,
                plan.expiry_ms,
                strike,
                plan.strike_scale,
            )],
        );
        // The bucket landed at exactly the PDA the plan derived up front.
        let acc = svm.get_account(pda).expect("bucket exists at derived PDA");
        let bucket =
            options_core::state::Bucket::try_deserialize(&mut acc.data.as_slice()).unwrap();
        assert_eq!(bucket.underlying_mint, underlying);
        assert_eq!(bucket.settlement_mint, settlement);
        assert_eq!(bucket.expiry_ms, plan.expiry_ms);
        assert_eq!(bucket.strike, strike);
        assert_eq!(bucket.strike_scale, plan.strike_scale);
        assert!(!bucket.invalidated);
    }

    // Re-submitting the same strike (same deterministic salt) collides
    // on-chain with "already in use" — the Benign resume signal.
    let text = send_expect_err(
        &mut svm,
        &admin,
        &[solana_tx::ix::create_bucket(
            &admin.pubkey(),
            &underlying,
            &settlement,
            salts[0],
            plan.expiry_ms,
            plan.strikes[0],
            plan.strike_scale,
        )],
    );
    assert!(
        roller::is_already_in_use_text(&text),
        "expected the already-in-use signal, got: {text}"
    );

    // ── vault create: single tx, salt from (mints, round_ms, generation) ──
    let template = VaultTemplate::default();
    let spec = VaultPairSpec {
        underlying_symbol: "TBTC".into(),
        settlement_symbol: "TUSDC".into(),
        underlying_mint: underlying.to_string(),
        settlement_mint: settlement.to_string(),
        underlying_decimals: 8,
        settlement_decimals: 6,
        underlying_feed_id: [1u8; 32],
        settlement_feed_id: [2u8; 32],
    };
    let config = build_vault_config(&spec, &template);
    assert!(options_vault::state::validate_config(&config));
    let vault_salt = salt::vault_salt(&underlying, &settlement, template.round_ms, 0);
    let vault_pda =
        solana_tx::pda::vault(&options_vault::ID, &underlying, &settlement, vault_salt);
    send(
        &mut svm,
        &admin,
        &[solana_tx::ix::create_vault(
            &admin.pubkey(),
            &underlying,
            &settlement,
            vault_salt,
            config,
        )],
    );

    let acc = svm.get_account(&vault_pda).expect("vault exists at derived PDA");
    let vault = options_vault::state::Vault::try_deserialize(&mut acc.data.as_slice()).unwrap();
    assert_eq!(vault.admin, admin.pubkey());
    assert_eq!(vault.underlying_mint, underlying);
    assert_eq!(vault.settlement_mint, settlement);
    assert_eq!(vault.config.round_ms, template.round_ms);
    assert_eq!(vault.config.underlying_feed_id, [1u8; 32]);
    assert_eq!(vault.salt, vault_salt);
    assert!(!vault.paused_deposits);

    // Duplicate create at the same generation collides — the crash-resume
    // ("already in use" → adopt) signal.
    let text = send_expect_err(
        &mut svm,
        &admin,
        &[solana_tx::ix::create_vault(
            &admin.pubkey(),
            &underlying,
            &settlement,
            vault_salt,
            build_vault_config(&spec, &template),
        )],
    );
    assert!(
        roller::is_already_in_use_text(&text),
        "expected the already-in-use signal, got: {text}"
    );

    // ── paused-vault replacement: retire bumps the generation ──
    // Decommission the gen-0 vault via set_paused (admin ix), then create
    // the replacement at generation 1: a DIFFERENT PDA that lands cleanly
    // instead of colliding with the paused vault.
    use anchor_lang::{InstructionData, ToAccountMetas};
    let set_paused = solana_sdk::instruction::Instruction::new_with_bytes(
        options_vault::ID,
        &options_vault::instruction::SetPaused { paused: true }.data(),
        options_vault::accounts::VaultAdmin {
            admin: admin.pubkey(),
            vault: vault_pda,
            event_authority: solana_tx::pda::event_authority(&options_vault::ID),
            program: options_vault::ID,
        }
        .to_account_metas(None),
    );
    send(&mut svm, &admin, &[set_paused]);
    let acc = svm.get_account(&vault_pda).unwrap();
    let paused_vault =
        options_vault::state::Vault::try_deserialize(&mut acc.data.as_slice()).unwrap();
    // This is exactly the read the adopt-on-collision path performs before
    // adopting — a paused vault must never be adopted.
    assert!(paused_vault.paused_deposits);

    let gen1_salt = salt::vault_salt(&underlying, &settlement, template.round_ms, 1);
    let gen1_pda =
        solana_tx::pda::vault(&options_vault::ID, &underlying, &settlement, gen1_salt);
    assert_ne!(gen1_pda, vault_pda, "retire→recreate must derive a NEW PDA");
    send(
        &mut svm,
        &admin,
        &[solana_tx::ix::create_vault(
            &admin.pubkey(),
            &underlying,
            &settlement,
            gen1_salt,
            build_vault_config(&spec, &template),
        )],
    );
    let acc = svm.get_account(&gen1_pda).expect("replacement vault exists");
    let replacement =
        options_vault::state::Vault::try_deserialize(&mut acc.data.as_slice()).unwrap();
    assert_eq!(replacement.salt, gen1_salt);
    assert!(!replacement.paused_deposits);
}
