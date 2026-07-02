#[test_only]
module sui_bridge::messaging_tests;

use sui::test_scenario::{Self as ts, Scenario};
use sui_bridge::chain_id;
use sui_bridge::envelope;
use sui_bridge::inbox::{Self, Inbox};
use sui_bridge::message;
use sui_bridge::outbox::{Self, Outbox};
use sui_bridge::registry::{Self, ChainRegistry, GroupKeyRegistry, GovernanceCap, GuardianCap};

const ADMIN: address = @0xA;

// Internal chain ids = (family << 27) | local (see `chain_id`).
// Sui   = (1 << 27) | 0   = 134217728
// Hyper = (2 << 27) | 998 = 268436454
const SUI_ID: u32 = 134217728;
const HYPER_ID: u32 = 268436454;

/// Shared per-deployment test salt (matches bridge-types TEST_SALT = 0x01*32).
fun test_salt(): vector<u8> { filled(0x01, 32) }

/// A stand-in for a Layer 2 app (e.g. a Locker). Its object id is the 32-byte
/// `dst_app` / `src_app` identity used by `consume` / `send`.
public struct TestApp has key { id: UID }

fun filled(b: u8, n: u64): vector<u8> {
    let mut v = vector<u8>[];
    let mut i = 0;
    while (i < n) { v.push_back(b); i = i + 1; };
    v
}

// Ed25519 group key + a valid signature over the DOMAIN-SEPARATED digest (salt
// = 0x01*32) of the message {src=2, dst=1, nonce=7, src_app=0xab*32,
// dst_app=0xcd*32, payload="hello-bridge"} — regenerate via
// `cargo run -p bridge-signer --example group_keys`.
fun group_pubkey(): vector<u8> {
    x"2152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db12"
}
fun valid_signature(): vector<u8> {
    x"12bc85a949906a86bdea305aa6bc32ef704e77de62ea5fb65a3df3a39902e53398ca95da28a3c34aa8187edcf8f6936330c94016e1e1c4d3f2f7b80027190001"
}

/// Tx 1: publish/init. Tx 2: register both chains + the Ed25519 group key, and
/// create the Inbox (dst=Sui) + Outbox (src=Sui). Leaves the scenario at the
/// start of tx 3 with everything shared.
fun setup(s: &mut Scenario) {
    registry::init_for_testing(s.ctx());
    s.next_tx(ADMIN);
    {
        let gov = s.take_from_sender<GovernanceCap>();
        let guardian = s.take_from_sender<GuardianCap>();
        let mut chains = s.take_shared<ChainRegistry>();
        let mut keys = s.take_shared<GroupKeyRegistry>();

        registry::register_chain(
            &gov, &mut chains, SUI_ID, b"sui-testnet",
            filled(0x01, 32), filled(0x02, 32), 1, 0,
        );
        registry::register_chain(
            &gov, &mut chains, HYPER_ID, b"hyperevm-testnet",
            filled(0x03, 32), filled(0x04, 32), 0, 12,
        );
        registry::register_group_key(&gov, &mut keys, 1, envelope::scheme_ed25519(), group_pubkey());

        inbox::create(&gov, SUI_ID, test_salt(), s.ctx());
        outbox::create(&gov, SUI_ID, test_salt(), s.ctx());

        ts::return_shared(chains);
        ts::return_shared(keys);
        s.return_to_sender(gov);
        s.return_to_sender(guardian);
    };
    s.next_tx(ADMIN);
}

/// The exact message the offline vector signed.
fun vector_message(): message::CrossChainMessage {
    message::new(HYPER_ID, SUI_ID, 7, filled(0xab, 32), filled(0xcd, 32), b"hello-bridge")
}

#[test]
fun chain_id_encoding_round_trips() {
    // The hardcoded test constants match the (family << 27 | local) scheme.
    assert!(SUI_ID == chain_id::new(chain_id::family_sui(), 0), 0);
    assert!(HYPER_ID == chain_id::new(chain_id::family_evm(), 998), 1);
    // Family + local are recoverable by shift/mask, no registry needed.
    assert!(chain_id::family(HYPER_ID) == chain_id::family_evm(), 2);
    assert!(chain_id::local(HYPER_ID) == 998, 3);
    assert!(chain_id::family(SUI_ID) == chain_id::family_sui(), 4);
    assert!(chain_id::local(SUI_ID) == 0, 5);
}

#[test]
fun receive_accepts_valid_threshold_signature() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let inbox = s.take_shared<Inbox>();
    let keys = s.take_shared<GroupKeyRegistry>();

    let env = envelope::new(envelope::scheme_ed25519(), 1, valid_signature());
    let delivered = inbox::receive(&inbox, &keys, vector_message(), env);

    assert!(inbox::delivered_src_chain_id(&delivered) == HYPER_ID, 0);
    assert!(inbox::delivered_payload(&delivered) == b"hello-bridge", 1);
    inbox::destroy_delivered_for_testing(delivered);

    ts::return_shared(inbox);
    ts::return_shared(keys);
    s.end();
}

#[test]
#[expected_failure]
fun receive_rejects_tampered_signature() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let inbox = s.take_shared<Inbox>();
    let keys = s.take_shared<GroupKeyRegistry>();

    let mut bad = valid_signature();
    *bad.borrow_mut(0) = *bad.borrow(0) ^ 0xff; // flip a byte
    let env = envelope::new(envelope::scheme_ed25519(), 1, bad);
    let delivered = inbox::receive(&inbox, &keys, vector_message(), env);

    inbox::destroy_delivered_for_testing(delivered); // unreachable
    ts::return_shared(inbox);
    ts::return_shared(keys);
    s.end();
}

#[test]
#[expected_failure]
fun receive_rejects_wrong_dst_chain() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let inbox = s.take_shared<Inbox>();
    let keys = s.take_shared<GroupKeyRegistry>();

    // dst = HyperEVM, but this Inbox is the Sui inbox → wrong_dst_chain (checked
    // before signature, so the signature need not be valid).
    let wrong = message::new(SUI_ID, HYPER_ID, 1, filled(0xab, 32), filled(0xcd, 32), b"x");
    let env = envelope::new(envelope::scheme_ed25519(), 1, valid_signature());
    let delivered = inbox::receive(&inbox, &keys, wrong, env);

    inbox::destroy_delivered_for_testing(delivered); // unreachable
    ts::return_shared(inbox);
    ts::return_shared(keys);
    s.end();
}

#[test]
fun consume_delivers_and_marks_replay() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let mut inbox = s.take_shared<Inbox>();

    let app = TestApp { id: object::new(s.ctx()) };
    let dst_app = object::uid_to_bytes(&app.id);
    let ds = message::derive_domain_sep(test_salt());
    let m = message::new(HYPER_ID, SUI_ID, 9, filled(0xab, 32), dst_app, b"payload-bytes");
    let hash = message::hash(&m, ds);

    let delivered = inbox::deliver_for_testing(&m, ds);
    let (src_chain, src_app, payload) = inbox::consume(&mut inbox, delivered, &app.id);

    assert!(src_chain == HYPER_ID, 0);
    assert!(src_app == filled(0xab, 32), 1);
    assert!(payload == b"payload-bytes", 2);
    assert!(inbox::is_consumed(&inbox, hash), 3);

    let TestApp { id } = app;
    id.delete();
    ts::return_shared(inbox);
    s.end();
}

#[test]
#[expected_failure]
fun consume_rejects_replay() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let mut inbox = s.take_shared<Inbox>();

    let app = TestApp { id: object::new(s.ctx()) };
    let dst_app = object::uid_to_bytes(&app.id);
    let m = message::new(HYPER_ID, SUI_ID, 9, filled(0xab, 32), dst_app, b"p");
    let ds = message::derive_domain_sep(test_salt());

    inbox::consume(&mut inbox, inbox::deliver_for_testing(&m, ds), &app.id);
    // Same message hash a second time → message_already_consumed.
    inbox::consume(&mut inbox, inbox::deliver_for_testing(&m, ds), &app.id);

    let TestApp { id } = app;
    id.delete();
    ts::return_shared(inbox);
    s.end();
}

#[test]
#[expected_failure]
fun consume_rejects_wrong_app_identity() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let mut inbox = s.take_shared<Inbox>();

    let app = TestApp { id: object::new(s.ctx()) };
    // dst_app deliberately not the app's id.
    let m = message::new(HYPER_ID, SUI_ID, 9, filled(0xab, 32), filled(0xee, 32), b"p");
    let ds = message::derive_domain_sep(test_salt());
    inbox::consume(&mut inbox, inbox::deliver_for_testing(&m, ds), &app.id);

    let TestApp { id } = app;
    id.delete();
    ts::return_shared(inbox);
    s.end();
}

#[test]
fun outbox_send_assigns_monotonic_nonce_and_matching_hash() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let mut outbox = s.take_shared<Outbox>();

    let app = TestApp { id: object::new(s.ctx()) };
    let dst_app = filled(0xcd, 32);

    let (n0, h0) = outbox::send(&mut outbox, &app.id, HYPER_ID, dst_app, b"first");
    let (n1, _h1) = outbox::send(&mut outbox, &app.id, HYPER_ID, dst_app, b"second");
    assert!(n0 == 0 && n1 == 1, 0);

    // The emitted hash equals an independent recompute of the same message
    // under the outbox's deployment salt.
    let ds = message::derive_domain_sep(test_salt());
    let expected = message::new(SUI_ID, HYPER_ID, 0, object::uid_to_bytes(&app.id), dst_app, b"first");
    assert!(h0 == message::hash(&expected, ds), 1);

    assert!(outbox::next_nonce(&mut outbox, HYPER_ID) == 2, 2);

    let TestApp { id } = app;
    id.delete();
    ts::return_shared(outbox);
    s.end();
}

#[test]
#[expected_failure]
fun outbox_send_blocked_when_paused() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let guardian = s.take_from_sender<GuardianCap>();
    let mut outbox = s.take_shared<Outbox>();

    outbox::set_paused(&guardian, &mut outbox, true);
    let app = TestApp { id: object::new(s.ctx()) };
    outbox::send(&mut outbox, &app.id, HYPER_ID, filled(0xcd, 32), b"x");

    let TestApp { id } = app;
    id.delete();
    s.return_to_sender(guardian);
    ts::return_shared(outbox);
    s.end();
}

/// End-to-end BCS parity: the exact `vector<u8>` args a generic relayer passes
/// (Rust `to_move_bcs`) decode via `from_bcs` and verify through the real
/// `inbox::receive` — the same domain-separated Ed25519 signature the relayer
/// obtains from the signer. This is the byte-level contract the Sui submitter
/// relies on (relayer-dispatch-design §3.1).
#[test]
fun receive_accepts_bcs_decoded_relayer_args() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let inbox = s.take_shared<Inbox>();
    let keys = s.take_shared<GroupKeyRegistry>();

    let m = message::from_bcs(
        x"01e603001000000008070000000000000020abababababababababababababababababababababababababababababababab20cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd0c68656c6c6f2d627269646765",
    );
    let env = envelope::from_bcs(
        x"00010000004012bc85a949906a86bdea305aa6bc32ef704e77de62ea5fb65a3df3a39902e53398ca95da28a3c34aa8187edcf8f6936330c94016e1e1c4d3f2f7b80027190001",
    );
    let delivered = inbox::receive(&inbox, &keys, m, env);
    assert!(inbox::delivered_payload(&delivered) == b"hello-bridge", 0);
    inbox::destroy_delivered_for_testing(delivered);

    ts::return_shared(inbox);
    ts::return_shared(keys);
    s.end();
}

#[test]
fun update_chain_backfills_peer_addresses() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let gov = s.take_from_sender<GovernanceCap>();
    let mut chains = s.take_shared<ChainRegistry>();

    // HYPER_ID was registered in setup with placeholder endpoints; backfill real ones.
    let real_outbox = filled(0xaa, 32);
    let real_inbox = filled(0xbb, 32);
    registry::update_chain(&gov, &mut chains, HYPER_ID, real_outbox, real_inbox, 0, 20);

    let (o, i) = registry::chain_endpoints(&chains, HYPER_ID);
    assert!(o == real_outbox, 0);
    assert!(i == real_inbox, 1);
    let (fk, fv) = registry::finality(&chains, HYPER_ID);
    assert!(fk == 0 && fv == 20, 2);

    s.return_to_sender(gov);
    ts::return_shared(chains);
    s.end();
}

#[test]
#[expected_failure]
fun update_chain_unregistered_aborts() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let gov = s.take_from_sender<GovernanceCap>();
    let mut chains = s.take_shared<ChainRegistry>();

    // internal id 999... never registered.
    registry::update_chain(&gov, &mut chains, 201326592, filled(0xaa, 32), filled(0xbb, 32), 0, 1);

    s.return_to_sender(gov);
    ts::return_shared(chains);
    s.end();
}

#[test]
#[expected_failure]
fun duplicate_chain_registration_aborts() {
    let mut s = ts::begin(ADMIN);
    setup(&mut s);
    let gov = s.take_from_sender<GovernanceCap>();
    let mut chains = s.take_shared<ChainRegistry>();

    // SUI_ID already registered in setup.
    registry::register_chain(
        &gov, &mut chains, SUI_ID, b"dup",
        filled(0x01, 32), filled(0x02, 32), 1, 0,
    );

    s.return_to_sender(gov);
    ts::return_shared(chains);
    s.end();
}
