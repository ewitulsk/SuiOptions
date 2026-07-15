//! LiteSVM test for the deposit_for_burn CPI wiring, using Circle's real
//! devnet programs + state accounts as fixtures (see
//! scripts/dump-cctp-fixtures.sh to refresh them).

use anchor_lang::{
    prelude::Pubkey,
    solana_program::instruction::Instruction,
    AnchorDeserialize, Discriminator, InstructionData, ToAccountMetas,
};
use litesvm::LiteSVM;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use cctp_bridge::{BridgeInitiated, MESSAGE_TRANSMITTER_ID, TOKEN_MESSENGER_MINTER_ID};

/// Devnet USDC mint (Circle).
const USDC_MINT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
/// CCTP v1 domain for Sui.
const SUI_DOMAIN: u32 = 8;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn usdc_mint() -> Pubkey {
    USDC_MINT.parse().unwrap()
}

struct Pdas {
    sender_authority: Pubkey,
    message_transmitter: Pubkey,
    token_messenger: Pubkey,
    remote_token_messenger: Pubkey,
    token_minter: Pubkey,
    local_token: Pubkey,
    tmm_event_authority: Pubkey,
}

fn pdas() -> Pdas {
    let tmm = TOKEN_MESSENGER_MINTER_ID;
    let mt = MESSAGE_TRANSMITTER_ID;
    let usdc = usdc_mint();
    Pdas {
        sender_authority: Pubkey::find_program_address(&[b"sender_authority"], &tmm).0,
        message_transmitter: Pubkey::find_program_address(&[b"message_transmitter"], &mt).0,
        token_messenger: Pubkey::find_program_address(&[b"token_messenger"], &tmm).0,
        remote_token_messenger: Pubkey::find_program_address(
            &[b"remote_token_messenger", SUI_DOMAIN.to_string().as_bytes()],
            &tmm,
        )
        .0,
        token_minter: Pubkey::find_program_address(&[b"token_minter"], &tmm).0,
        local_token: Pubkey::find_program_address(&[b"local_token", usdc.as_ref()], &tmm).0,
        tmm_event_authority: Pubkey::find_program_address(&[b"__event_authority"], &tmm).0,
    }
}

/// Prints the addresses scripts/dump-cctp-fixtures.sh needs to dump.
/// `cargo test -p cctp_bridge print_fixture_addresses -- --ignored --nocapture`
#[test]
#[ignore]
fn print_fixture_addresses() {
    let p = pdas();
    println!("message_transmitter {}", p.message_transmitter);
    println!("token_messenger {}", p.token_messenger);
    println!("remote_token_messenger {}", p.remote_token_messenger);
    println!("token_minter {}", p.token_minter);
    println!("local_token {}", p.local_token);
    println!("usdc_mint {}", usdc_mint());
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

    let account = solana_account::Account {
        lamports: 10_000_000,
        data,
        owner: spl_token::id(),
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(address, account).unwrap();
    address
}

#[test]
fn test_deposit_for_burn_cpi() {
    let mut svm = LiteSVM::new();
    let p = pdas();

    // Circle programs (dumped from devnet).
    svm.add_program_from_file(TOKEN_MESSENGER_MINTER_ID, format!("{FIXTURES}/token_messenger_minter.so"))
        .expect("missing token_messenger_minter.so fixture; run scripts/dump-cctp-fixtures.sh");
    svm.add_program_from_file(MESSAGE_TRANSMITTER_ID, format!("{FIXTURES}/message_transmitter.so"))
        .expect("missing message_transmitter.so fixture");
    // Our program.
    svm.add_program_from_file(
        cctp_bridge::id(),
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/cctp_bridge.so"),
    )
    .expect("missing cctp_bridge.so; run anchor build first");

    // Circle state (dumped from devnet).
    load_account_fixture(&mut svm, &p.message_transmitter, "message_transmitter");
    load_account_fixture(&mut svm, &p.token_messenger, "token_messenger");
    load_account_fixture(&mut svm, &p.remote_token_messenger, "remote_token_messenger");
    load_account_fixture(&mut svm, &p.token_minter, "token_minter");
    load_account_fixture(&mut svm, &p.local_token, "local_token");
    load_account_fixture(&mut svm, &usdc_mint(), "usdc_mint");

    // User with 5 USDC.
    let user = Keypair::new();
    svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    let burn_token_account = set_usdc_token_account(&mut svm, &user.pubkey(), 5_000_000);

    let message_sent_event_data = Keypair::new();
    let amount: u64 = 1_000_000; // 1 USDC
    let mint_recipient = Pubkey::new_unique(); // Sui recipient address as bytes32

    let bridge_event_authority =
        Pubkey::find_program_address(&[b"__event_authority"], &cctp_bridge::id()).0;

    let accounts = cctp_bridge::accounts::DepositForBurn {
        owner: user.pubkey(),
        sender_authority_pda: p.sender_authority,
        burn_token_account,
        message_transmitter: p.message_transmitter,
        token_messenger: p.token_messenger,
        remote_token_messenger: p.remote_token_messenger,
        token_minter: p.token_minter,
        local_token: p.local_token,
        burn_token_mint: usdc_mint(),
        message_sent_event_data: message_sent_event_data.pubkey(),
        message_transmitter_program: MESSAGE_TRANSMITTER_ID,
        token_messenger_minter_program: TOKEN_MESSENGER_MINTER_ID,
        token_messenger_minter_event_authority: p.tmm_event_authority,
        token_program: anchor_spl::token::ID,
        system_program: anchor_lang::system_program::ID,
        event_authority: bridge_event_authority,
        program: cctp_bridge::id(),
    };

    let ix = Instruction {
        program_id: cctp_bridge::id(),
        accounts: accounts.to_account_metas(None),
        data: cctp_bridge::instruction::DepositForBurn {
            amount,
            destination_domain: SUI_DOMAIN,
            mint_recipient,
        }
        .data(),
    };

    let msg = Message::new_with_blockhash(&[ix], Some(&user.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(
        VersionedMessage::Legacy(msg),
        &[&user, &message_sent_event_data],
    )
    .unwrap();

    let result = svm.send_transaction(tx);
    let meta = match result {
        Ok(meta) => meta,
        Err(e) => panic!("deposit_for_burn failed: {:?}\nlogs: {:#?}", e.err, e.meta.logs),
    };

    // USDC burned from the user's token account.
    let token_acc = svm.get_account(&burn_token_account).unwrap();
    use solana_program_pack::Pack;
    let parsed =
        anchor_spl::token::spl_token::state::Account::unpack_from_slice(&token_acc.data).unwrap();
    assert_eq!(parsed.amount, 4_000_000);

    // Our BridgeInitiated event landed via event-CPI (inner ix data on our
    // program: 8-byte anchor event-ix tag + 8-byte event discriminator + body).
    let event = meta
        .inner_instructions
        .iter()
        .flatten()
        .filter_map(|inner| {
            let ix = &inner.instruction;
            let data: &[u8] = &ix.data;
            data.get(8..16)
                .filter(|disc| *disc == BridgeInitiated::DISCRIMINATOR)
                .map(|_| BridgeInitiated::deserialize(&mut &data[16..]).unwrap())
        })
        .next()
        .expect("BridgeInitiated event not found in inner instructions");

    assert_eq!(event.sender, user.pubkey());
    assert_eq!(event.amount, amount);
    assert_eq!(event.destination_domain, SUI_DOMAIN);
    assert_eq!(event.mint_recipient, mint_recipient);
    assert_eq!(event.burn_token, usdc_mint());
}
