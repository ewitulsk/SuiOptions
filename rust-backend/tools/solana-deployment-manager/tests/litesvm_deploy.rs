//! Integration test: the tool's initialize + test-mint building blocks
//! against the real program binary under LiteSVM. Skips gracefully (with a
//! note) when the `.so` hasn't been built (`anchor build` in
//! solana-contracts).

use anchor_lang::AccountDeserialize;
use litesvm::LiteSVM;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;

use solana_deployment_manager::plan::{plan_initialize, plan_token, InitAction, TokenAction};
use solana_deployment_manager::tokens;

const DEPLOY_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../solana-contracts/target/deploy"
);

fn send(svm: &mut LiteSVM, payer: &Keypair, ixs: &[solana_sdk::instruction::Instruction], extra: &[&Keypair]) {
    svm.expire_blockhash();
    let msg = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &svm.latest_blockhash());
    let mut signers: Vec<&Keypair> = vec![payer];
    signers.extend_from_slice(extra);
    let tx = Transaction::new(&signers, msg, svm.latest_blockhash());
    if let Err(e) = svm.send_transaction(tx) {
        panic!("transaction failed: {:?}\nlogs: {:#?}", e.err, e.meta.logs);
    }
}

#[test]
fn initialize_and_test_mints_end_to_end() {
    let mut svm = LiteSVM::new();
    let so_path = format!("{DEPLOY_DIR}/options_core.so");
    let bytes = match std::fs::read(&so_path) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("skipping litesvm test: {so_path} not built");
            return;
        }
    };
    svm.add_program(options_core::ID, &bytes).unwrap();

    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();

    // ── idempotent initialize ──
    let config_pda = solana_tx::pda::config(&options_core::ID);
    assert!(svm.get_account(&config_pda).is_none());
    assert_eq!(
        plan_initialize(None, &admin.pubkey()).unwrap(),
        InitAction::Initialize
    );

    send(
        &mut svm,
        &admin,
        &[solana_tx::ix::initialize(&admin.pubkey())],
        &[],
    );

    let account = svm.get_account(&config_pda).expect("config exists");
    let config =
        options_core::state::Config::try_deserialize(&mut account.data.as_slice()).unwrap();
    assert_eq!(config.admin, admin.pubkey());
    // A re-run would see this Config and skip.
    assert_eq!(
        plan_initialize(Some(&config), &admin.pubkey()).unwrap(),
        InitAction::AlreadyInitialized
    );
    // A foreign key would refuse.
    assert!(plan_initialize(Some(&config), &Keypair::new().pubkey()).is_err());

    // ── test mint creation (faucet ≠ payer: authority handover path) ──
    let faucet = Pubkey::new_unique();
    let rent = svm.minimum_balance_for_rent_exemption(tokens::MINT_SPACE);
    let mint_kp = Keypair::new();
    let decimals = 6u8;
    let ixs =
        tokens::create_mint_ixs(&admin.pubkey(), &mint_kp.pubkey(), decimals, &faucet, rent)
            .unwrap();
    send(&mut svm, &admin, &ixs, &[&mint_kp]);

    let mint_acc = svm.get_account(&mint_kp.pubkey()).expect("mint exists");
    let mint =
        anchor_spl::token::Mint::try_deserialize(&mut mint_acc.data.as_slice()).unwrap();
    assert_eq!(mint.decimals, decimals);
    assert_eq!(mint.supply, tokens::initial_supply(decimals));
    assert_eq!(mint.mint_authority, Some(faucet).into());
    assert!(mint.freeze_authority.is_none());
    // The tool's on-chain probe sees the mint as alive with the right shape.
    assert_eq!(
        tokens::mint_decimals(&mint_acc.owner, &mint_acc.data),
        Some(decimals)
    );

    // Seed supply landed in the deployer's ATA.
    let ata = solana_tx::pda::ata(&admin.pubkey(), &mint_kp.pubkey());
    let ata_acc = svm.get_account(&ata).expect("payer ata exists");
    let token =
        anchor_spl::token::TokenAccount::try_deserialize(&mut ata_acc.data.as_slice()).unwrap();
    assert_eq!(token.amount, tokens::initial_supply(decimals));
    assert_eq!(token.owner, admin.pubkey());

    // plan_token: the record + live mint → Keep; a vanished mint → Create.
    let record = solana_deployments::TestToken {
        mint: mint_kp.pubkey().to_string(),
        decimals,
        mint_authority: faucet.to_string(),
    };
    assert_eq!(
        plan_token(Some(&record), Some(decimals), decimals),
        TokenAction::Keep
    );
    assert_eq!(plan_token(Some(&record), None, decimals), TokenAction::Create);
}
