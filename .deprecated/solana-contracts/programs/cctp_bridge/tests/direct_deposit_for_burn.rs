//! LiteSVM test for the **frontend's** Solana burn instruction, which calls
//! Circle's TokenMessengerMinter directly rather than through this program
//! (see frontend/src/solana/bridge.ts).
//!
//! This is the shape that actually bridges in production, and nothing else
//! covers it: the account list is hand-built in TypeScript, so a reorder or a
//! wrong signer/writable flag would only surface as a failed user tx. Here it
//! runs against Circle's real devnet program + state fixtures (refresh them
//! with scripts/dump-cctp-fixtures.sh) and must actually burn the USDC.
//!
//! Keep the `keys` list below in lockstep with `sendSolanaDepositForBurn`.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use litesvm::LiteSVM;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use cctp_bridge::{MESSAGE_TRANSMITTER_ID, TOKEN_MESSENGER_MINTER_ID};

/// Devnet USDC mint (Circle).
const USDC_MINT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
/// CCTP v1 domain for Sui.
const SUI_DOMAIN: u32 = 8;
/// sha256("global:deposit_for_burn")[..8] — mirrors the frontend constant.
const DEPOSIT_FOR_BURN_DISCRIMINATOR: [u8; 8] = [215, 60, 61, 46, 114, 55, 128, 176];

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn usdc_mint() -> Pubkey {
    USDC_MINT.parse().unwrap()
}

fn load_account_fixture(svm: &mut LiteSVM, address: &Pubkey, name: &str) {
    let path = format!("{FIXTURES}/{name}.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing fixture {path}; run scripts/dump-cctp-fixtures.sh"));
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let acc = &v["account"];
    let data_b64 = acc["data"][0].as_str().unwrap();
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .unwrap();
    let account = solana_account::Account {
        lamports: acc["lamports"].as_u64().unwrap(),
        data,
        owner: acc["owner"].as_str().unwrap().parse().unwrap(),
        executable: acc["executable"].as_bool().unwrap(),
        rent_epoch: acc["rentEpoch"].as_u64().unwrap_or(0),
    };
    svm.set_account(*address, account).unwrap();
}

/// Crafts an SPL token account holding `amount` USDC for `owner`.
fn set_usdc_token_account(svm: &mut LiteSVM, owner: &Pubkey, amount: u64) -> Pubkey {
    use anchor_spl::token::spl_token::{self, state::AccountState};
    use solana_program_pack::Pack;

    let address = Pubkey::new_unique();
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account {
        mint: usdc_mint(),
        owner: *owner,
        amount,
        delegate: None.into(),
        state: AccountState::Initialized,
        is_native: None.into(),
        delegated_amount: 0,
        close_authority: None.into(),
    }
    .pack_into_slice(&mut data);

    svm.set_account(
        address,
        solana_account::Account {
            lamports: 10_000_000,
            data,
            owner: spl_token::id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    address
}

#[test]
fn frontend_direct_deposit_for_burn_burns_usdc() {
    let mut svm = LiteSVM::new();
    let tmm = TOKEN_MESSENGER_MINTER_ID;
    let mt = MESSAGE_TRANSMITTER_ID;
    let usdc = usdc_mint();

    // Same PDA derivations the frontend does.
    let pda = |seeds: &[&[u8]], program: &Pubkey| Pubkey::find_program_address(seeds, program).0;
    let sender_authority = pda(&[b"sender_authority"], &tmm);
    let message_transmitter = pda(&[b"message_transmitter"], &mt);
    let token_messenger = pda(&[b"token_messenger"], &tmm);
    let remote_token_messenger = pda(
        &[b"remote_token_messenger", SUI_DOMAIN.to_string().as_bytes()],
        &tmm,
    );
    let token_minter = pda(&[b"token_minter"], &tmm);
    let local_token = pda(&[b"local_token", usdc.as_ref()], &tmm);
    let tmm_event_authority = pda(&[b"__event_authority"], &tmm);

    // Circle programs + state, dumped from devnet. Our program is NOT loaded:
    // the point is that the burn no longer routes through it.
    svm.add_program_from_file(tmm, format!("{FIXTURES}/token_messenger_minter.so"))
        .expect("missing token_messenger_minter.so fixture; run scripts/dump-cctp-fixtures.sh");
    svm.add_program_from_file(mt, format!("{FIXTURES}/message_transmitter.so"))
        .expect("missing message_transmitter.so fixture");
    load_account_fixture(&mut svm, &message_transmitter, "message_transmitter");
    load_account_fixture(&mut svm, &token_messenger, "token_messenger");
    load_account_fixture(&mut svm, &remote_token_messenger, "remote_token_messenger");
    load_account_fixture(&mut svm, &token_minter, "token_minter");
    load_account_fixture(&mut svm, &local_token, "local_token");
    load_account_fixture(&mut svm, &usdc, "usdc_mint");

    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let burn_token_account = set_usdc_token_account(&mut svm, &user.pubkey(), 5_000_000);

    let message_sent_event_data = Keypair::new();
    let amount: u64 = 1_000_000; // 1 USDC
    let mint_recipient = Pubkey::new_unique(); // Sui recipient address as bytes32

    // Borsh args: amount u64 LE | destination_domain u32 LE | mint_recipient 32B.
    let mut data = Vec::with_capacity(8 + 8 + 4 + 32);
    data.extend_from_slice(&DEPOSIT_FOR_BURN_DISCRIMINATOR);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&SUI_DOMAIN.to_le_bytes());
    data.extend_from_slice(mint_recipient.as_ref());

    // Mirrors frontend/src/solana/bridge.ts `keys` exactly, including the
    // duplicated owner (burn authority, then event_rent_payer).
    let ix = Instruction {
        program_id: tmm,
        accounts: vec![
            AccountMeta::new_readonly(user.pubkey(), true),
            AccountMeta::new(user.pubkey(), true), // event_rent_payer
            AccountMeta::new_readonly(sender_authority, false),
            AccountMeta::new(burn_token_account, false),
            AccountMeta::new(message_transmitter, false),
            AccountMeta::new_readonly(token_messenger, false),
            AccountMeta::new_readonly(remote_token_messenger, false),
            AccountMeta::new_readonly(token_minter, false),
            AccountMeta::new(local_token, false),
            AccountMeta::new(usdc, false),
            AccountMeta::new(message_sent_event_data.pubkey(), true),
            AccountMeta::new_readonly(mt, false),
            AccountMeta::new_readonly(tmm, false),
            AccountMeta::new_readonly(anchor_spl::token::ID, false),
            AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
            AccountMeta::new_readonly(tmm_event_authority, false),
            AccountMeta::new_readonly(tmm, false),
        ],
        data,
    };

    let msg = Message::new_with_blockhash(&[ix], Some(&user.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(
        VersionedMessage::Legacy(msg),
        &[&user, &message_sent_event_data],
    )
    .unwrap();

    if let Err(e) = svm.send_transaction(tx) {
        panic!("direct deposit_for_burn failed: {:?}\nlogs: {:#?}", e.err, e.meta.logs);
    }

    // Circle burned the user's USDC: 5 → 4.
    use solana_program_pack::Pack;
    let token_acc = svm.get_account(&burn_token_account).unwrap();
    let parsed =
        anchor_spl::token::spl_token::state::Account::unpack_from_slice(&token_acc.data).unwrap();
    assert_eq!(parsed.amount, 4_000_000);
}
