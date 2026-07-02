// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {ChainId} from "../src/libraries/ChainId.sol";
import {Envelope} from "../src/libraries/Envelope.sol";
import {Message} from "../src/libraries/Message.sol";
import {TransferPayload} from "../src/libraries/TransferPayload.sol";
import {Registry} from "../src/Registry.sol";
import {Outbox} from "../src/Outbox.sol";
import {Inbox} from "../src/Inbox.sol";
import {Locker} from "../src/Locker.sol";
import {WrappedToken} from "../src/WrappedToken.sol";

contract LockerTest is Test {
    Registry registry;
    Outbox outbox;
    Inbox inbox;

    // Home (18-dec ERC-20, escrow) and foreign (6-dec wrapped, mint) lockers,
    // both on HYPER_ID; the sibling chain is SUI_ID for routing.
    WrappedToken homeToken;
    Locker homeLocker;
    WrappedToken wrapped;
    Locker foreignLocker;

    address admin = address(this);
    address guardian = makeAddr("guardian");
    address user = makeAddr("user");
    address recipient = makeAddr("recipient");

    bytes32 constant ASSET = 0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa;
    bytes32 constant SALT = 0x0101010101010101010101010101010101010101010101010101010101010101;
    uint256 constant GROUP_PK = 0xA11CE;

    uint32 immutable SUI_ID = ChainId.encode(ChainId.FAMILY_SUI, 0);
    uint32 immutable HYPER_ID = ChainId.encode(ChainId.FAMILY_EVM, 998);

    function setUp() public {
        registry = new Registry(admin, guardian);
        outbox = new Outbox(registry, HYPER_ID, SALT);
        inbox = new Inbox(registry, HYPER_ID, SALT);
        registry.registerChain(SUI_ID, bytes("sui"), bytes32(0), bytes32(0), 1, 0);
        registry.registerChain(HYPER_ID, bytes("hyper"), bytes32(0), bytes32(0), 0, 12);
        registry.registerGroupKey(
            1, Envelope.SCHEME_ECDSA_SECP256K1, abi.encodePacked(vm.addr(GROUP_PK))
        );

        // Home: 18-decimal native token, escrow locker.
        homeToken = new WrappedToken("Home", "H", 18, admin);
        homeLocker = new Locker(outbox, address(inbox), ASSET, address(homeToken), Locker.Mode.Escrow, 18, admin);

        // Foreign: 6-decimal wrapped token, mint locker. No unbacked seed — the
        // only way to hold wrapped is via a delivered mint (keeps the supply
        // invariant honest).
        wrapped = new WrappedToken("Wrapped", "W", 6, admin);
        foreignLocker = new Locker(outbox, address(inbox), ASSET, address(wrapped), Locker.Mode.Mint, 6, admin);
        wrapped.transferOwnership(address(foreignLocker));

        // Peers keyed by the *sibling* chain id (SUI_ID here).
        homeLocker.setPeer(SUI_ID, _id(address(foreignLocker)));
        foreignLocker.setPeer(SUI_ID, _id(address(homeLocker)));

        homeToken.mint(user, 10e18);
    }

    // --- helpers ---

    function _id(address a) internal pure returns (bytes32) {
        return Message.addressToBytes32(a);
    }

    function _payload(uint64 wireAmount, address to) internal pure returns (bytes memory) {
        return TransferPayload.encode(TransferPayload.Data(ASSET, wireAmount, bytes32(uint256(uint160(to)))));
    }

    // --- outbound ---

    function test_lock_escrows_and_scales_to_wire() public {
        vm.startPrank(user);
        homeToken.approve(address(homeLocker), 1e18);
        homeLocker.lock(1e18, SUI_ID, _id(recipient));
        vm.stopPrank();

        assertEq(homeToken.balanceOf(address(homeLocker)), 1e18);
        assertEq(homeLocker.escrowed(), 1e18);
        assertEq(outbox.nextNonce(SUI_ID), 1); // one message committed
    }

    function test_lock_rejects_dust() public {
        vm.startPrank(user);
        homeToken.approve(address(homeLocker), type(uint256).max);
        // 1e18 + 1 is not divisible by 10^(18-8)=1e10 → dust.
        vm.expectRevert(Locker.AmountHasDust.selector);
        homeLocker.lock(1e18 + 1, SUI_ID, _id(recipient));
        vm.stopPrank();
    }

    function test_lock_on_mint_locker_reverts_wrong_mode() public {
        vm.expectRevert(Locker.WrongMode.selector);
        foreignLocker.lock(1e6, SUI_ID, _id(recipient));
    }

    function test_bridgeOut_unknown_peer_reverts() public {
        vm.startPrank(user);
        homeToken.approve(address(homeLocker), 1e18);
        vm.expectRevert(abi.encodeWithSelector(Locker.UnknownPeer.selector, uint32(999)));
        homeLocker.lock(1e18, 999, _id(recipient));
        vm.stopPrank();
    }

    function test_burn_on_foreign_scales_from_local() public {
        // Obtain wrapped via a real delivery first (5e6 = 5.0 tokens at 6-dec).
        vm.prank(address(inbox));
        foreignLocker.onReceive(SUI_ID, _id(address(homeLocker)), _payload(5e8, user));
        assertEq(wrapped.balanceOf(user), 5e6);

        vm.prank(user);
        foreignLocker.burn(1e6, SUI_ID, _id(recipient)); // 1.0 token at 6 decimals
        assertEq(wrapped.balanceOf(user), 4e6);
        assertEq(wrapped.totalSupply(), 4e6);
        assertEq(outbox.nextNonce(SUI_ID), 1); // burn committed one outbound message
    }

    // --- inbound ---

    function test_inbound_release_escrow() public {
        // Fund escrow first (user locks), then a return message releases it.
        vm.startPrank(user);
        homeToken.approve(address(homeLocker), 1e18);
        homeLocker.lock(1e18, SUI_ID, _id(recipient));
        vm.stopPrank();

        vm.prank(address(inbox));
        homeLocker.onReceive(SUI_ID, _id(address(foreignLocker)), _payload(1e8, recipient));

        assertEq(homeToken.balanceOf(recipient), 1e18); // 1e8 wire → 1e18 local (18-dec)
        assertEq(homeLocker.escrowed(), 0);
    }

    function test_inbound_mint_foreign() public {
        vm.prank(address(inbox));
        foreignLocker.onReceive(SUI_ID, _id(address(homeLocker)), _payload(1e8, recipient));
        assertEq(wrapped.balanceOf(recipient), 1e6); // 1e8 wire → 1e6 local (6-dec)
    }

    function test_onReceive_only_inbox() public {
        vm.expectRevert(Locker.NotInbox.selector);
        foreignLocker.onReceive(SUI_ID, _id(address(homeLocker)), _payload(1e8, recipient));
    }

    function test_onReceive_peer_mismatch() public {
        vm.prank(address(inbox));
        vm.expectRevert(Locker.PeerMismatch.selector);
        foreignLocker.onReceive(SUI_ID, _id(makeAddr("imposter")), _payload(1e8, recipient));
    }

    function test_onReceive_asset_mismatch() public {
        bytes memory bad = TransferPayload.encode(
            TransferPayload.Data(bytes32(uint256(1)), 1e8, bytes32(uint256(uint160(recipient))))
        );
        vm.prank(address(inbox));
        vm.expectRevert(Locker.AssetMismatch.selector);
        foreignLocker.onReceive(SUI_ID, _id(address(homeLocker)), bad);
    }

    // --- rate limit queue + claim ---

    function test_rate_limit_queues_over_cap_and_claims_after_window() public {
        vm.warp(10_000);
        foreignLocker.setRateLimit(1000, 1e8); // window 1000s, cap 1e8 wire

        // First delivery within cap → mints immediately.
        vm.prank(address(inbox));
        foreignLocker.onReceive(SUI_ID, _id(address(homeLocker)), _payload(1e8, recipient));
        assertEq(wrapped.balanceOf(recipient), 1e6);

        // Second delivery exceeds the window cap → queued, NOT reverted, no mint.
        vm.prank(address(inbox));
        foreignLocker.onReceive(SUI_ID, _id(address(homeLocker)), _payload(1e8, user));
        assertEq(wrapped.balanceOf(user), 0); // queued, nothing minted yet
        (address qr, uint64 qa, uint64 unlockAt, bool claimed) = foreignLocker.queued(0);
        assertEq(qr, user);
        assertEq(qa, 1e8);
        assertEq(unlockAt, 11_000);
        assertFalse(claimed);

        // Claim before unlock → reverts; after the window → mints.
        vm.expectRevert(Locker.StillLocked.selector);
        foreignLocker.claim(0);

        vm.warp(11_000);
        foreignLocker.claim(0);
        assertEq(wrapped.balanceOf(user), 1e6); // now delivered

        // Double claim guarded.
        vm.expectRevert(Locker.BadQueueEntry.selector);
        foreignLocker.claim(0);
    }

    // --- pause ---

    function test_pause_blocks_in_and_out() public {
        foreignLocker.setPaused(true);

        vm.prank(user);
        vm.expectRevert(Locker.Paused.selector);
        foreignLocker.burn(1e6, SUI_ID, _id(recipient));

        vm.prank(address(inbox));
        vm.expectRevert(Locker.Paused.selector);
        foreignLocker.onReceive(SUI_ID, _id(address(homeLocker)), _payload(1e8, recipient));
    }

    function test_setPeer_only_admin() public {
        vm.prank(makeAddr("stranger"));
        vm.expectRevert(Locker.NotAdmin.selector);
        foreignLocker.setPeer(SUI_ID, _id(address(homeLocker)));
    }

    function test_transferAdmin_hands_over_control() public {
        address gov = makeAddr("gov");
        foreignLocker.transferAdmin(gov);
        assertEq(foreignLocker.admin(), gov);
        // old admin can no longer act
        vm.expectRevert(Locker.NotAdmin.selector);
        foreignLocker.setPaused(true);
        // new admin can
        vm.prank(gov);
        foreignLocker.setPaused(true);
        assertTrue(foreignLocker.paused());
    }

    // --- supply invariant across a round trip ---

    function test_supply_invariant_round_trip() public {
        // 1) Lock 1e18 on home → escrow 1e18 (= 1e8 wire).
        vm.startPrank(user);
        homeToken.approve(address(homeLocker), 1e18);
        homeLocker.lock(1e18, SUI_ID, _id(recipient));
        vm.stopPrank();

        // 2) Deliver to foreign → mint 1e6 wrapped (= 1e8 wire).
        vm.prank(address(inbox));
        foreignLocker.onReceive(SUI_ID, _id(address(homeLocker)), _payload(1e8, recipient));

        // Invariant: wrapped supply (wire) <= escrow (wire). Here equal.
        assertEq(wrapped.totalSupply(), 1e6);
        assertEq(homeLocker.escrowed(), 1e18);

        // 3) Burn on foreign → send back.
        vm.prank(recipient);
        foreignLocker.burn(1e6, SUI_ID, _id(recipient));
        assertEq(wrapped.totalSupply(), 0);

        // 4) Deliver back to home → release 1e18, escrow drains to 0.
        vm.prank(address(inbox));
        homeLocker.onReceive(SUI_ID, _id(address(foreignLocker)), _payload(1e8, recipient));
        assertEq(homeToken.balanceOf(recipient), 1e18);
        assertEq(homeLocker.escrowed(), 0);
    }

    // --- end-to-end through the real Inbox (ECDSA verify → dispatch → mint) ---

    function test_e2e_inbox_dispatch_mints() public {
        Message.CrossChainMessage memory m = Message.CrossChainMessage({
            version: Message.VERSION,
            srcChainId: SUI_ID,
            dstChainId: HYPER_ID,
            nonce: 0,
            srcApp: _id(address(homeLocker)),
            dstApp: _id(address(foreignLocker)),
            payload: _payload(1e8, recipient)
        });
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(GROUP_PK, Message.hash(m, Message.deriveDomainSep(SALT)));
        Envelope.SignatureEnvelope memory env = Envelope.SignatureEnvelope({
            schemeTag: Envelope.SCHEME_ECDSA_SECP256K1,
            groupPubkeyId: 1,
            signature: abi.encodePacked(r, s, v)
        });

        inbox.receiveMessage(m, env);
        assertEq(wrapped.balanceOf(recipient), 1e6);
        assertTrue(inbox.consumed(Message.hash(m, Message.deriveDomainSep(SALT))));
    }
}
