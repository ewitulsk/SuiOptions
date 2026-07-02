// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {ChainId} from "../src/libraries/ChainId.sol";
import {Envelope} from "../src/libraries/Envelope.sol";
import {Message} from "../src/libraries/Message.sol";
import {Registry} from "../src/Registry.sol";
import {Outbox} from "../src/Outbox.sol";
import {Inbox} from "../src/Inbox.sol";
import {IMessageRecipient} from "../src/interfaces/IMessageRecipient.sol";

contract MockRecipient is IMessageRecipient {
    uint32 public lastSrcChainId;
    bytes32 public lastSrcApp;
    bytes public lastPayload;
    uint256 public calls;

    function onReceive(uint32 srcChainId, bytes32 srcApp, bytes calldata payload) external {
        lastSrcChainId = srcChainId;
        lastSrcApp = srcApp;
        lastPayload = payload;
        calls++;
    }
}

contract MessagingTest is Test {
    Registry registry;
    Outbox outbox;
    Inbox inbox;
    MockRecipient recipient;

    address governance = address(this);
    address guardian = makeAddr("guardian");

    // Threshold group key (1-of-1 launch posture). vm.sign signs with `groupPk`;
    // its address is what the registry stores and `ecrecover` must match.
    uint256 constant GROUP_PK = 0xA11CE;
    uint32 constant GROUP_KEY_ID = 1;

    uint32 immutable SUI_ID = ChainId.encode(ChainId.FAMILY_SUI, 0);
    uint32 immutable HYPER_ID = ChainId.encode(ChainId.FAMILY_EVM, 998);

    bytes32 constant SUI_PEER = 0x1111111111111111111111111111111111111111111111111111111111111111;
    bytes32 constant TEST_SALT = 0x0101010101010101010101010101010101010101010101010101010101010101;
    bytes32 domainSep;

    function setUp() public {
        registry = new Registry(governance, guardian);
        // This Inbox/Outbox live on HyperEVM.
        inbox = new Inbox(registry, HYPER_ID, TEST_SALT);
        outbox = new Outbox(registry, HYPER_ID, TEST_SALT);
        domainSep = Message.deriveDomainSep(TEST_SALT);
        recipient = new MockRecipient();

        registry.registerChain(SUI_ID, bytes("sui-testnet"), bytes32(0), bytes32(0), 1, 0);
        registry.registerChain(HYPER_ID, bytes("hyperevm-testnet"), bytes32(0), bytes32(0), 0, 12);
        registry.registerGroupKey(
            GROUP_KEY_ID, Envelope.SCHEME_ECDSA_SECP256K1, abi.encodePacked(vm.addr(GROUP_PK))
        );
    }

    // --- helpers ---

    function _message(uint64 nonce, bytes memory payload)
        internal
        view
        returns (Message.CrossChainMessage memory)
    {
        return Message.CrossChainMessage({
            version: Message.VERSION,
            srcChainId: SUI_ID,
            dstChainId: HYPER_ID,
            nonce: nonce,
            srcApp: SUI_PEER,
            dstApp: Message.addressToBytes32(address(recipient)),
            payload: payload
        });
    }

    function _sign(uint256 pk, bytes32 digest) internal pure returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, digest);
        return abi.encodePacked(r, s, v);
    }

    function _envelope(uint256 pk, Message.CrossChainMessage memory m)
        internal
        view
        returns (Envelope.SignatureEnvelope memory)
    {
        return Envelope.SignatureEnvelope({
            schemeTag: Envelope.SCHEME_ECDSA_SECP256K1,
            groupPubkeyId: GROUP_KEY_ID,
            signature: _sign(pk, Message.hash(m, domainSep))
        });
    }

    // --- inbox: receive / dispatch ---

    function test_receive_delivers_valid_signature() public {
        Message.CrossChainMessage memory m = _message(0, bytes("payload-bytes"));
        bytes32 h = Message.hash(m, domainSep);

        vm.expectEmit(true, false, false, true, address(inbox));
        emit Inbox.MessageDelivered(h, SUI_ID, 0);
        inbox.receiveMessage(m, _envelope(GROUP_PK, m));

        assertEq(recipient.calls(), 1);
        assertEq(recipient.lastSrcChainId(), SUI_ID);
        assertEq(recipient.lastSrcApp(), SUI_PEER);
        assertEq(recipient.lastPayload(), bytes("payload-bytes"));
        assertTrue(inbox.consumed(h));
        assertEq(inbox.highestNonce(SUI_ID), 0);
    }

    function test_receive_rejects_replay() public {
        Message.CrossChainMessage memory m = _message(0, bytes("p"));
        Envelope.SignatureEnvelope memory env = _envelope(GROUP_PK, m);
        inbox.receiveMessage(m, env);

        vm.expectRevert(
            abi.encodeWithSelector(Inbox.AlreadyConsumed.selector, Message.hash(m, domainSep))
        );
        inbox.receiveMessage(m, env);
    }

    function test_receive_rejects_bad_signature() public {
        Message.CrossChainMessage memory m = _message(0, bytes("p"));
        // Sign with a different key → ecrecover != group address.
        Envelope.SignatureEnvelope memory env = _envelope(0xBADBAD, m);
        vm.expectRevert(Envelope.InvalidSignature.selector);
        inbox.receiveMessage(m, env);
    }

    function test_receive_rejects_wrong_dst_chain() public {
        Message.CrossChainMessage memory m = _message(0, bytes("p"));
        m.dstChainId = SUI_ID; // not this inbox's chain
        vm.expectRevert(
            abi.encodeWithSelector(Inbox.WrongDstChain.selector, HYPER_ID, SUI_ID)
        );
        inbox.receiveMessage(m, _envelope(GROUP_PK, m));
    }

    function test_receive_rejects_when_paused() public {
        vm.prank(guardian);
        inbox.setPaused(true);

        Message.CrossChainMessage memory m = _message(0, bytes("p"));
        vm.expectRevert(Inbox.InboxPaused.selector);
        inbox.receiveMessage(m, _envelope(GROUP_PK, m));
    }

    function test_receive_rejects_scheme_key_mismatch() public {
        // Register an Ed25519 key under a new id, but present an ECDSA envelope.
        registry.registerGroupKey(2, Envelope.SCHEME_ED25519, abi.encodePacked(vm.addr(GROUP_PK)));
        Message.CrossChainMessage memory m = _message(0, bytes("p"));
        Envelope.SignatureEnvelope memory env = _envelope(GROUP_PK, m);
        env.groupPubkeyId = 2; // registered scheme 0, envelope says 1
        vm.expectRevert(
            abi.encodeWithSelector(
                Inbox.SchemeKeyMismatch.selector,
                Envelope.SCHEME_ED25519,
                Envelope.SCHEME_ECDSA_SECP256K1
            )
        );
        inbox.receiveMessage(m, env);
    }

    // --- outbox: send ---

    function test_outbox_send_assigns_nonce_and_matching_hash() public {
        bytes32 dstApp = SUI_PEER;
        (uint64 n0, bytes32 h0) = outbox.send(SUI_ID, dstApp, bytes("first"));
        (uint64 n1,) = outbox.send(SUI_ID, dstApp, bytes("second"));
        assertEq(n0, 0);
        assertEq(n1, 1);
        assertEq(outbox.nextNonce(SUI_ID), 2);

        Message.CrossChainMessage memory expected = Message.CrossChainMessage({
            version: Message.VERSION,
            srcChainId: HYPER_ID,
            dstChainId: SUI_ID,
            nonce: 0,
            srcApp: Message.addressToBytes32(address(this)),
            dstApp: dstApp,
            payload: bytes("first")
        });
        assertEq(h0, Message.hash(expected, domainSep));
    }

    function test_outbox_send_blocked_when_paused() public {
        vm.prank(guardian);
        outbox.setPaused(true);
        vm.expectRevert(Outbox.OutboxPaused.selector);
        outbox.send(SUI_ID, SUI_PEER, bytes("x"));
    }

    function test_outbox_setPaused_only_guardian() public {
        vm.expectRevert(Outbox.NotGuardian.selector);
        outbox.setPaused(true);
    }

    // --- registry ---

    function test_registry_duplicate_chain_reverts() public {
        vm.expectRevert(
            abi.encodeWithSelector(Registry.ChainAlreadyRegistered.selector, SUI_ID)
        );
        registry.registerChain(SUI_ID, bytes("dup"), bytes32(0), bytes32(0), 1, 0);
    }

    function test_registry_register_only_governance() public {
        vm.prank(makeAddr("stranger"));
        vm.expectRevert(Registry.NotGovernance.selector);
        registry.registerChain(99, bytes(""), bytes32(0), bytes32(0), 0, 0);
    }

    function test_registry_rejects_family_zero_id() public {
        // internalId < 2^27 → family bits = 0 → UnknownFamily.
        vm.expectRevert(abi.encodeWithSelector(ChainId.UnknownFamily.selector, uint8(0)));
        registry.registerChain(123, bytes(""), bytes32(0), bytes32(0), 0, 0);
    }
}
