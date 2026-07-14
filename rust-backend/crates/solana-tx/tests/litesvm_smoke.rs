//! End-to-end smoke test: our instruction builders against the real
//! program binaries under LiteSVM. Skips gracefully (with a note) when the
//! `.so` files haven't been built (`anchor build` in solana-contracts).

use anchor_lang::AccountDeserialize;
use litesvm::LiteSVM;
use litesvm_token::CreateMint;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;

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
            eprintln!("skipping litesvm smoke test: {path} not built");
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

#[test]
fn initialize_core_and_create_bucket() {
    let mut svm = LiteSVM::new();
    for (id, name) in [
        (options_core::ID, "options_core"),
        (auction_venue::ID, "auction_venue"),
        (options_vault::ID, "options_vault"),
    ] {
        if !load_program(&mut svm, id, name) {
            return;
        }
    }

    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();

    // initialize: Config + Treasury.
    send(&mut svm, &admin, &[solana_tx::ix::initialize(&admin.pubkey())]);

    let config_pda = solana_tx::pda::config(&options_core::ID);
    let config_acc = svm.get_account(&config_pda).expect("config exists");
    let config =
        options_core::state::Config::try_deserialize(&mut config_acc.data.as_slice()).unwrap();
    assert_eq!(config.admin, admin.pubkey());
    assert_eq!(config.fee_bps, 0);

    // create_bucket over two fresh mints.
    let underlying = CreateMint::new(&mut svm, &admin).decimals(8).send().unwrap();
    let settlement = CreateMint::new(&mut svm, &admin).decimals(6).send().unwrap();
    let salt = 1u64;
    let expiry_ms = 4_000_000_000_000u64; // far future vs litesvm's clock
    send(
        &mut svm,
        &admin,
        &[solana_tx::ix::create_bucket(
            &admin.pubkey(),
            &underlying,
            &settlement,
            salt,
            expiry_ms,
            65_000_000_000u128,
            8,
        )],
    );

    let bucket_pda = solana_tx::pda::bucket(&options_core::ID, &underlying, &settlement, salt);
    let bucket_acc = svm.get_account(&bucket_pda).expect("bucket exists");
    let bucket =
        options_core::state::Bucket::try_deserialize(&mut bucket_acc.data.as_slice()).unwrap();
    assert_eq!(bucket.underlying_mint, underlying);
    assert_eq!(bucket.settlement_mint, settlement);
    assert_eq!(bucket.expiry_ms, expiry_ms);
    assert_eq!(bucket.strike, 65_000_000_000u128);
    assert_eq!(
        bucket.call_mint,
        solana_tx::pda::call_mint(&options_core::ID, &bucket_pda)
    );
    assert_eq!(bucket.total_written, 0);
    assert!(!bucket.invalidated);
}
