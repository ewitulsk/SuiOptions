// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IAccessControl} from "@openzeppelin/contracts/access/IAccessControl.sol";
import {IAccessControlDefaultAdminRules} from
    "@openzeppelin/contracts/access/extensions/IAccessControlDefaultAdminRules.sol";

import {SpokeVault} from "../src/SpokeVault.sol";
import {Wire} from "../src/lib/Wire.sol";
import {MockIntegration} from "./mocks/MockIntegration.sol";
import {SpokeTestBase} from "./utils/SpokeTestBase.sol";

contract SpokeVaultDepositTest is SpokeTestBase {
    function test_deposit_escrows_and_sends_notice() public {
        vm.recordLogs();
        uint256 before = tusdg.balanceOf(alice);
        uint64 seq = doDeposit(alice, 100e6, 2);
        assertEq(seq, 1);
        assertEq(tusdg.balanceOf(alice), before - 100e6);
        assertEq(tusdg.balanceOf(address(vault)), 100e6);
        (uint128 pending, uint128 active, uint128 reserved) = vault.funds(USDG);
        assertEq(pending, 100e6);
        assertEq(active, 0);
        assertEq(reserved, 0);

        (address depositor, uint8 asset, uint8 tranche, SpokeVault.DepositStatus status, uint128 amount,) =
            vault.deposits(seq);
        assertEq(depositor, alice);
        assertEq(asset, USDG);
        assertEq(tranche, 2);
        assertEq(uint8(status), uint8(SpokeVault.DepositStatus.Pending));
        assertEq(amount, 100e6);

        // The DepositNotice left through the endpoint with correct wiring.
        (bytes memory env, bytes memory payload) = split(lastOutbound());
        (uint8 msgType, Wire.Envelope memory e) = Wire.decodeEnvelope(env);
        assertEq(msgType, Wire.MSG_DEPOSIT_NOTICE);
        assertEq(e.srcChainId, LOCAL_CHAIN);
        assertEq(e.dstChainId, HUB_CHAIN);
        assertEq(e.srcApp, bytes32(uint256(uint160(address(vault)))));
        assertEq(e.dstApp, HUB_APP);
        assertEq(e.seq, 1);
        Wire.DepositNotice memory n = Wire.decodeDepositNotice(payload);
        assertEq(n.spokeId, SPOKE_ID);
        assertEq(n.depositSeq, seq);
        assertEq(n.depositor, bytes32(uint256(uint160(alice))));
        assertEq(n.asset, USDG);
        assertEq(n.amount, 100e6);
        assertEq(n.tranche, 2);
        assertEq(n.tsMs, uint64(block.timestamp) * 1000);
    }

    function test_deposit_input_validation() public {
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.UnknownAsset.selector, 9));
        vault.deposit(9, 100e6, 0);

        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.BadTranche.selector, 3));
        vault.deposit(USDG, 100e6, 3);

        vm.prank(alice);
        vm.expectRevert(SpokeVault.ZeroAmount.selector);
        vault.deposit(USDG, 0, 0);

        vm.prank(alice);
        vm.expectRevert(
            abi.encodeWithSelector(SpokeVault.AmountTooLarge.selector, uint256(type(uint128).max) + 1)
        );
        vault.deposit(USDG, uint256(type(uint128).max) + 1, 0);
    }

    function test_deposit_blocked_by_local_pause() public {
        vm.prank(pauser);
        vault.setLocalPause(true);
        vm.prank(alice);
        vm.expectRevert(SpokeVault.VaultPaused.selector);
        vault.deposit(USDG, 100e6, 0);

        vm.prank(pauser);
        vault.setLocalPause(false);
        doDeposit(alice, 100e6, 0);
    }

    function test_deposit_blocked_by_hub_pause() public {
        sendConfigSync(true, false, curator, RELAYER_EP_ID, bytes32(0));
        vm.prank(alice);
        vm.expectRevert(SpokeVault.VaultPaused.selector);
        vault.deposit(USDG, 100e6, 0);
    }

    function test_whitelist_gates_deposit_and_withdraw() public {
        vm.prank(whitelister);
        vault.setWhitelistEnabled(true);

        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.NotWhitelisted.selector, alice));
        vault.deposit(USDG, 100e6, 0);

        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.NotWhitelisted.selector, alice));
        vault.requestWithdraw(0, 1, false);

        vm.prank(whitelister);
        vault.setWhitelisted(alice, true);
        doDeposit(alice, 100e6, 0);

        // Disabled again = open for everyone.
        vm.prank(whitelister);
        vault.setWhitelistEnabled(false);
        doDeposit(bob, 50e6, 0);
    }

    // ────────────────────────── ACK lifecycle ──────────────────────────

    function test_depositAck_accepted_moves_pending_to_active() public {
        uint64 seq = doDeposit(alice, 100e6, 2);
        vm.expectEmit(true, false, false, true);
        emit SpokeVault.DepositAcked(seq, 555);
        vm.expectEmit(true, true, false, true);
        emit SpokeVault.SharesRecorded(alice, 2, 555);
        sendDepositAck(seq, true, 555);

        (uint128 pending, uint128 active,) = vault.funds(USDG);
        assertEq(pending, 0);
        assertEq(active, 100e6);
        assertEq(vault.shareMirror(alice, 2), 555);
        (,,, SpokeVault.DepositStatus status,,) = vault.deposits(seq);
        assertEq(uint8(status), uint8(SpokeVault.DepositStatus.Acked));
    }

    function test_depositAck_rejected_refunds_escrow() public {
        uint256 before = tusdg.balanceOf(alice);
        uint64 seq = doDeposit(alice, 100e6, 0);
        vm.expectEmit(true, false, false, true);
        emit SpokeVault.DepositRejected(seq);
        sendDepositAck(seq, false, 0);

        assertEq(tusdg.balanceOf(alice), before);
        (uint128 pending, uint128 active,) = vault.funds(USDG);
        assertEq(pending, 0);
        assertEq(active, 0);
        assertEq(vault.shareMirror(alice, 0), 0);
        (,,, SpokeVault.DepositStatus status,,) = vault.deposits(seq);
        assertEq(uint8(status), uint8(SpokeVault.DepositStatus.Refunded));
    }

    function test_reclaim_lifecycle() public {
        uint256 before = tusdg.balanceOf(alice);
        uint64 seq = doDeposit(alice, 100e6, 0);

        // Too early.
        vm.prank(alice);
        vm.expectRevert(
            abi.encodeWithSelector(
                SpokeVault.TimeoutNotElapsed.selector, uint64(block.timestamp) + DEPOSIT_TIMEOUT
            )
        );
        vault.reclaim(seq);

        vm.warp(block.timestamp + DEPOSIT_TIMEOUT);

        // Wrong caller.
        vm.prank(bob);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.NotDepositor.selector, bob));
        vault.reclaim(seq);

        vm.prank(alice);
        vault.reclaim(seq);
        assertEq(tusdg.balanceOf(alice), before);
        (uint128 pending,,) = vault.funds(USDG);
        assertEq(pending, 0);

        // Double reclaim.
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.DepositNotPending.selector, seq));
        vault.reclaim(seq);
    }

    function test_late_ack_after_reclaim_alarms_but_lane_continues() public {
        uint64 seq = doDeposit(alice, 100e6, 0);
        vm.warp(block.timestamp + DEPOSIT_TIMEOUT);
        vm.prank(alice);
        vault.reclaim(seq);

        // The late ACK must NOT revert, must NOT change funds, and must alarm.
        vm.expectEmit(true, false, false, true);
        emit SpokeVault.AlarmAckForReclaimed(seq, true, 555);
        sendDepositAck(seq, true, 555);
        (uint128 pending, uint128 active,) = vault.funds(USDG);
        assertEq(pending, 0);
        assertEq(active, 0);
        assertEq(vault.shareMirror(alice, 0), 0);

        // Lane continues: a fresh deposit + ACK works afterwards.
        uint64 seq2 = doDeposit(bob, 40e6, 0);
        sendDepositAck(seq2, true, 100);
        (, uint128 active2,) = vault.funds(USDG);
        assertEq(active2, 40e6);
    }

    function test_ack_for_unknown_seq_alarms() public {
        vm.expectEmit(true, false, false, true);
        emit SpokeVault.AlarmAckForReclaimed(77, true, 1);
        sendDepositAck(77, true, 1);
        assertEq(vault.lastInboundSeq(), hubSeq); // seq advanced anyway
    }
}

contract SpokeVaultLaneTest is SpokeTestBase {
    function test_handleMessage_only_active_endpoint() public {
        bytes memory m = Wire.encodeDepositAck(hubEnv(), Wire.DepositAck(1, true, 1));
        (bytes memory env, bytes memory payload) = split(m);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.NotEndpoint.selector, address(this)));
        vault.handleMessage(env, payload);
    }

    function test_seq_replay_and_out_of_order_rejected() public {
        sendConfigSync(false, false, curator, RELAYER_EP_ID, bytes32(0)); // seq 1
        assertEq(vault.lastInboundSeq(), 1);

        // Replay of seq 1.
        bytes memory replay = Wire.encodeConfigSync(
            Wire.Envelope(HUB_CHAIN, LOCAL_CHAIN, HUB_APP, bytes32(uint256(uint160(address(vault)))), 1),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), RELAYER_EP_ID, bytes32(0))
        );
        vm.prank(relayerBot);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.SeqNotIncreasing.selector, 1, 1));
        relayer.deliver(replay);

        // Gap is fine (transport may drop): jump to seq 5.
        hubSeq = 4;
        sendConfigSync(false, false, curator, RELAYER_EP_ID, bytes32(0)); // seq 5
        assertEq(vault.lastInboundSeq(), 5);

        // Out-of-order below the watermark.
        bytes memory old = Wire.encodeConfigSync(
            Wire.Envelope(HUB_CHAIN, LOCAL_CHAIN, HUB_APP, bytes32(uint256(uint160(address(vault)))), 3),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), RELAYER_EP_ID, bytes32(0))
        );
        vm.prank(relayerBot);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.SeqNotIncreasing.selector, 3, 5));
        relayer.deliver(old);
    }

    function test_bad_origin_rejected() public {
        // Wrong src chain id.
        bytes memory m = Wire.encodeConfigSync(
            Wire.Envelope(99, LOCAL_CHAIN, HUB_APP, bytes32(uint256(uint160(address(vault)))), 1),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), RELAYER_EP_ID, bytes32(0))
        );
        vm.prank(relayerBot);
        vm.expectRevert(SpokeVault.BadOrigin.selector);
        relayer.deliver(m);

        // Wrong src app.
        m = Wire.encodeConfigSync(
            Wire.Envelope(
                HUB_CHAIN, LOCAL_CHAIN, bytes32(uint256(1)), bytes32(uint256(uint160(address(vault)))), 1
            ),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), RELAYER_EP_ID, bytes32(0))
        );
        vm.prank(relayerBot);
        vm.expectRevert(SpokeVault.BadOrigin.selector);
        relayer.deliver(m);

        // Wrong dst app.
        m = Wire.encodeConfigSync(
            Wire.Envelope(HUB_CHAIN, LOCAL_CHAIN, HUB_APP, bytes32(uint256(0xdead)), 1),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), RELAYER_EP_ID, bytes32(0))
        );
        vm.prank(relayerBot);
        vm.expectRevert(SpokeVault.BadOrigin.selector);
        relayer.deliver(m);
    }

    function test_spoke_to_hub_types_rejected_inbound() public {
        bytes memory m = Wire.encodeDepositNotice(
            hubEnv(),
            Wire.DepositNotice(SPOKE_ID, 1, bytes32(uint256(uint160(alice))), USDG, 1, 0, 0)
        );
        vm.prank(relayerBot);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.UnexpectedMsgType.selector, 1));
        relayer.deliver(m);
    }
}

contract SpokeVaultWithdrawTest is SpokeTestBase {
    function test_requestWithdraw_records_and_sends() public {
        activeDeposit(alice, 100e6, 1000);
        vm.recordLogs();
        uint64 seq = doRequestWithdraw(alice, 0, 400, false);
        assertEq(seq, 1);
        assertEq(vault.inFlightRequest(alice, 0), seq);
        (address user, uint8 tranche, bool all, bool open, uint128 shares) = vault.withdrawals(seq);
        assertEq(user, alice);
        assertEq(tranche, 0);
        assertEq(all, false);
        assertEq(open, true);
        assertEq(shares, 400);

        (bytes memory env, bytes memory payload) = split(lastOutbound());
        (uint8 msgType,) = Wire.decodeEnvelope(env);
        assertEq(msgType, Wire.MSG_WITHDRAW_REQUEST);
        Wire.WithdrawRequest memory r = Wire.decodeWithdrawRequest(payload);
        assertEq(r.spokeId, SPOKE_ID);
        assertEq(r.requestSeq, seq);
        assertEq(r.user, bytes32(uint256(uint160(alice))));
        assertEq(r.tranche, 0);
        assertEq(r.shares, 400);
        assertEq(r.all, false);
    }

    function test_requestWithdraw_one_in_flight_per_user_tranche() public {
        uint64 seq = doRequestWithdraw(alice, 0, 400, false);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.WithdrawInFlight.selector, seq));
        vault.requestWithdraw(0, 100, false);

        // Different tranche is independent; other users are independent.
        doRequestWithdraw(alice, 1, 100, false);
        doRequestWithdraw(bob, 0, 100, false);
    }

    function test_requestWithdraw_input_validation() public {
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.BadTranche.selector, 3));
        vault.requestWithdraw(3, 1, false);

        vm.prank(alice);
        vm.expectRevert(SpokeVault.ZeroShares.selector);
        vault.requestWithdraw(0, 0, false);

        // all = true with zero shares is valid.
        doRequestWithdraw(alice, 0, 0, true);
    }

    function test_withdrawAck_rejected_unlocks() public {
        uint64 seq = doRequestWithdraw(alice, 0, 400, false);
        vm.expectEmit(true, false, false, true);
        emit SpokeVault.WithdrawRejected(seq);
        sendWithdrawAck(seq, alice, 0);
        assertEq(vault.inFlightRequest(alice, 0), 0);
        // Can request again.
        doRequestWithdraw(alice, 0, 100, false);
    }

    function test_withdrawAck_pays_immediately_and_sends_receipt() public {
        activeDeposit(alice, 100e6, 1000);
        uint64 seq = doRequestWithdraw(alice, 0, 400, false);
        uint256 before = tusdg.balanceOf(alice);

        vm.recordLogs();
        sendWithdrawAck(seq, alice, 60e6);
        assertEq(tusdg.balanceOf(alice), before + 60e6);
        (, uint128 active, uint128 reserved) = vault.funds(USDG);
        assertEq(active, 40e6);
        assertEq(reserved, 0);
        assertEq(vault.inFlightRequest(alice, 0), 0);
        assertEq(vault.shareMirror(alice, 0), 600); // 1000 - 400 mirror burn
        assertEq(vault.totalQueuedPayouts(), 0);

        (bytes memory env, bytes memory payload) = split(lastOutbound());
        (uint8 msgType,) = Wire.decodeEnvelope(env);
        assertEq(msgType, Wire.MSG_PAYOUT_RECEIPT);
        Wire.PayoutReceipt memory r = Wire.decodePayoutReceipt(payload);
        assertEq(r.spokeId, SPOKE_ID);
        assertEq(r.requestSeq, seq);
        assertEq(r.amount, 60e6);
    }

    function test_withdrawAck_all_zeroes_mirror() public {
        activeDeposit(alice, 100e6, 1000);
        uint64 seq = doRequestWithdraw(alice, 0, 0, true);
        sendWithdrawAck(seq, alice, 50e6);
        assertEq(vault.shareMirror(alice, 0), 0);
    }

    function test_withdrawAck_partial_reserves_and_queues() public {
        activeDeposit(alice, 40e6, 1000);
        uint64 seq = doRequestWithdraw(alice, 0, 0, true);
        uint256 before = tusdg.balanceOf(alice);

        vm.expectEmit(true, true, false, true);
        emit SpokeVault.PayoutQueued(seq, alice, USDG, 100e6);
        sendWithdrawAck(seq, alice, 100e6);

        // Nothing paid yet; what exists moved to reserved.
        assertEq(tusdg.balanceOf(alice), before);
        (, uint128 active, uint128 reserved) = vault.funds(USDG);
        assertEq(active, 0);
        assertEq(reserved, 40e6);
        assertEq(vault.totalQueuedPayouts(), 1);
        assertEq(vault.queueLength(USDG), 1);
    }

    function test_queue_fifo_drain_via_fundPayouts_and_processPayoutQueue() public {
        // Two queued payouts: alice owed 100, bob owed 50. No active funds.
        uint64 seqA = doRequestWithdraw(alice, 0, 0, true);
        uint64 seqB = doRequestWithdraw(bob, 0, 0, true);
        sendWithdrawAck(seqA, alice, 100e6);
        sendWithdrawAck(seqB, bob, 50e6);
        assertEq(vault.totalQueuedPayouts(), 2);

        // Donation of 120: alice paid in full FIRST, bob partially reserved.
        uint256 aliceBefore = tusdg.balanceOf(alice);
        uint256 bobBefore = tusdg.balanceOf(bob);
        tusdg.mint(address(this), 120e6);
        tusdg.approve(address(vault), type(uint256).max);
        vm.expectEmit(true, true, false, true);
        emit SpokeVault.PayoutPaid(seqA, alice, USDG, 100e6);
        vault.fundPayouts(USDG, 120e6);

        assertEq(tusdg.balanceOf(alice), aliceBefore + 100e6);
        assertEq(tusdg.balanceOf(bob), bobBefore);
        (, uint128 active, uint128 reserved) = vault.funds(USDG);
        assertEq(active, 0);
        assertEq(reserved, 20e6); // partial toward bob
        assertEq(vault.totalQueuedPayouts(), 1);

        // A freshly ACKed deposit does NOT auto-service the queue…
        activeDeposit(alice, 50e6, 1);
        (, active, reserved) = vault.funds(USDG);
        assertEq(active, 50e6);
        assertEq(reserved, 20e6);
        assertEq(vault.totalQueuedPayouts(), 1);

        // …but permissionless processPayoutQueue drains it FIFO.
        vm.recordLogs();
        vm.prank(makeAddr("anyone"));
        vault.processPayoutQueue(USDG);
        assertEq(tusdg.balanceOf(bob), bobBefore + 50e6);
        (, active, reserved) = vault.funds(USDG);
        assertEq(active, 20e6);
        assertEq(reserved, 0);
        assertEq(vault.totalQueuedPayouts(), 0);

        // Receipt for bob's payout went out.
        (, bytes memory payload) = split(lastOutbound());
        Wire.PayoutReceipt memory r = Wire.decodePayoutReceipt(payload);
        assertEq(r.requestSeq, seqB);
        assertEq(r.amount, 50e6);
    }

    function test_new_ack_behind_nonempty_queue_goes_fifo() public {
        // Queue alice (owed 100, nothing available).
        uint64 seqA = doRequestWithdraw(alice, 0, 0, true);
        sendWithdrawAck(seqA, alice, 100e6);
        assertEq(vault.totalQueuedPayouts(), 1);

        // Now 30 active arrives, then bob's ack for 20: even though active
        // could cover bob, he must queue behind alice.
        activeDeposit(bob, 30e6, 1);
        uint64 seqB = doRequestWithdraw(bob, 0, 0, true);
        uint256 bobBefore = tusdg.balanceOf(bob);
        sendWithdrawAck(seqB, bob, 20e6);
        assertEq(tusdg.balanceOf(bob), bobBefore);
        assertEq(vault.totalQueuedPayouts(), 2);
        // The 30 active got reserved toward alice's older payout.
        (, uint128 active, uint128 reserved) = vault.funds(USDG);
        assertEq(active, 0);
        assertEq(reserved, 30e6);
    }

    function test_withdrawAck_unknown_request_alarms_not_reverts() public {
        vm.expectEmit(true, false, false, true);
        emit SpokeVault.AlarmUnknownWithdrawAck(99, bytes32(uint256(uint160(alice))), 5e6);
        sendWithdrawAck(99, alice, 5e6);
        assertEq(vault.lastInboundSeq(), hubSeq);

        // User mismatch on a real open request also alarms and leaves it open.
        uint64 seq = doRequestWithdraw(alice, 0, 10, false);
        vm.expectEmit(true, false, false, true);
        emit SpokeVault.AlarmUnknownWithdrawAck(seq, bytes32(uint256(uint160(bob))), 5e6);
        sendWithdrawAck(seq, bob, 5e6);
        (,,, bool open,) = vault.withdrawals(seq);
        assertTrue(open);
    }

    function test_pause_halts_payouts() public {
        activeDeposit(alice, 100e6, 1000);
        vm.prank(pauser);
        vault.setLocalPause(true);

        // WithdrawAck while paused: queued, not paid.
        uint64 seq = doRequestWithdraw(alice, 0, 0, true);
        uint256 before = tusdg.balanceOf(alice);
        sendWithdrawAck(seq, alice, 60e6);
        assertEq(tusdg.balanceOf(alice), before);
        assertEq(vault.totalQueuedPayouts(), 1);

        vm.expectRevert(SpokeVault.VaultPaused.selector);
        vault.processPayoutQueue(USDG);

        // Unpause: queue drains from active.
        vm.prank(pauser);
        vault.setLocalPause(false);
        vault.processPayoutQueue(USDG);
        assertEq(tusdg.balanceOf(alice), before + 60e6);
        assertEq(vault.totalQueuedPayouts(), 0);
    }
}

contract SpokeVaultConfigAndIntegrationTest is SpokeTestBase {
    MockIntegration mi;

    function setUp() public override {
        super.setUp();
        mi = new MockIntegration();
    }

    function test_configSync_applies_gates_and_curator() public {
        address newCurator = makeAddr("newCurator");
        bytes32 root = keccak256("root");
        sendConfigSync(true, true, newCurator, RELAYER_EP_ID, root);
        assertTrue(vault.hubPaused());
        assertTrue(vault.paused());
        assertTrue(vault.riskOff());
        assertTrue(vault.effectiveRiskOff());
        assertEq(vault.curator(), newCurator);
        assertEq(vault.integrationsRoot(), root);
        assertEq(vault.activeEndpoint(), address(relayer)); // unchanged

        sendConfigSync(false, false, newCurator, RELAYER_EP_ID, root);
        assertFalse(vault.paused());
        assertFalse(vault.effectiveRiskOff());
    }

    function test_configSync_unknown_endpoint_id_alarms_keeps_endpoint() public {
        vm.expectEmit(false, false, false, true);
        emit SpokeVault.AlarmUnknownEndpointId(9);
        sendConfigSync(false, false, curator, 9, bytes32(0));
        assertEq(vault.activeEndpoint(), address(relayer));
        // Lane still works.
        sendConfigSync(false, false, curator, RELAYER_EP_ID, bytes32(0));
    }

    function test_setIntegrations_requires_matching_root() public {
        address[] memory list = new address[](1);
        list[0] = address(mi);

        // Bootstrap root is zero: nothing can be installed.
        vm.expectRevert(
            abi.encodeWithSelector(
                SpokeVault.IntegrationsRootMismatch.selector, keccak256(abi.encode(list)), bytes32(0)
            )
        );
        vault.setIntegrations(list);

        // Hub commits a different root: mismatched list still rejected.
        bytes32 root = keccak256("something else");
        sendConfigSync(false, false, curator, RELAYER_EP_ID, root);
        vm.expectRevert(
            abi.encodeWithSelector(
                SpokeVault.IntegrationsRootMismatch.selector, keccak256(abi.encode(list)), root
            )
        );
        vault.setIntegrations(list);

        // Matching root installs (permissionless, called by a rando).
        sendConfigSync(false, false, curator, RELAYER_EP_ID, keccak256(abi.encode(list)));
        vm.prank(makeAddr("anyone"));
        vault.setIntegrations(list);
        assertTrue(vault.isIntegration(address(mi)));
        assertEq(vault.integrationCount(), 1);
    }

    function test_setIntegrations_rejects_unsorted_and_replaces_set() public {
        MockIntegration mi2 = new MockIntegration();
        (address lo, address hi) = address(mi) < address(mi2)
            ? (address(mi), address(mi2))
            : (address(mi2), address(mi));

        address[] memory unsorted = new address[](2);
        unsorted[0] = hi;
        unsorted[1] = lo;
        sendConfigSync(false, false, curator, RELAYER_EP_ID, keccak256(abi.encode(unsorted)));
        vm.expectRevert(SpokeVault.IntegrationsNotSorted.selector);
        vault.setIntegrations(unsorted);

        address[] memory sorted = new address[](2);
        sorted[0] = lo;
        sorted[1] = hi;
        sendConfigSync(false, false, curator, RELAYER_EP_ID, keccak256(abi.encode(sorted)));
        vault.setIntegrations(sorted);
        assertTrue(vault.isIntegration(lo) && vault.isIntegration(hi));

        // A new root with a smaller set replaces (kill switch for the other).
        address[] memory only = new address[](1);
        only[0] = lo;
        sendConfigSync(false, false, curator, RELAYER_EP_ID, keccak256(abi.encode(only)));
        vault.setIntegrations(only);
        assertTrue(vault.isIntegration(lo));
        assertFalse(vault.isIntegration(hi));
        assertEq(vault.integrationCount(), 1);
    }

    function test_extendTo_gating_and_success() public {
        activeDeposit(alice, 100e6, 1000);

        // Not curator.
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.NotCurator.selector, address(this)));
        vault.extendTo(address(mi), USDG, 10e6);

        // Unregistered integration.
        vm.prank(curator);
        vm.expectRevert(
            abi.encodeWithSelector(SpokeVault.IntegrationNotRegistered.selector, address(mi))
        );
        vault.extendTo(address(mi), USDG, 10e6);

        registerIntegration(mi);

        // More than active.
        vm.prank(curator);
        vm.expectRevert(
            abi.encodeWithSelector(SpokeVault.InsufficientActive.selector, uint128(200e6), uint128(100e6))
        );
        vault.extendTo(address(mi), USDG, 200e6);

        // Success.
        vm.prank(curator);
        vault.extendTo(address(mi), USDG, 40e6);
        assertEq(tusdg.balanceOf(address(mi)), 40e6);
        assertEq(mi.lastAsset(), address(tusdg));
        assertEq(mi.lastAmount(), 40e6);
        (, uint128 active,) = vault.funds(USDG);
        assertEq(active, 60e6);
        assertEq(vault.extendedOutstanding(address(mi)), 40e6);
    }

    function test_extendTo_blocked_by_riskOff_and_queued_payouts() public {
        activeDeposit(alice, 100e6, 1000);
        registerIntegration(mi);

        // Hub risk_off blocks.
        sendConfigSync(false, true, curator, RELAYER_EP_ID, vault.integrationsRoot());
        vm.prank(curator);
        vm.expectRevert(SpokeVault.RiskOffActive.selector);
        vault.extendTo(address(mi), USDG, 10e6);
        sendConfigSync(false, false, curator, RELAYER_EP_ID, vault.integrationsRoot());

        // Queued payout blocks.
        uint64 seq = doRequestWithdraw(bob, 0, 0, true);
        sendWithdrawAck(seq, bob, 500e6); // way more than active: queues
        assertEq(vault.totalQueuedPayouts(), 1);
        vm.prank(curator);
        vm.expectRevert(SpokeVault.PayoutsQueued.selector);
        vault.extendTo(address(mi), USDG, 10e6);
    }

    function test_stale_heartbeat_freezes_integrations_not_deposits_or_payouts() public {
        activeDeposit(alice, 100e6, 1000);
        registerIntegration(mi);
        assertFalse(vault.effectiveRiskOff());

        // Hub goes dark past HEARTBEAT_TIMEOUT.
        vm.warp(block.timestamp + HEARTBEAT_TIMEOUT + 1);
        assertTrue(vault.effectiveRiskOff());
        vm.prank(curator);
        vm.expectRevert(SpokeVault.RiskOffActive.selector);
        vault.extendTo(address(mi), USDG, 10e6);

        // Deposits and payouts continue.
        doDeposit(bob, 10e6, 0);
        tusdg.mint(address(this), 5e6);
        tusdg.approve(address(vault), type(uint256).max);
        vault.fundPayouts(USDG, 5e6);

        // Any inbound hub message refreshes the heartbeat.
        sendConfigSync(false, false, curator, RELAYER_EP_ID, vault.integrationsRoot());
        assertFalse(vault.effectiveRiskOff());
        vm.prank(curator);
        vault.extendTo(address(mi), USDG, 10e6);
    }

    function test_returnFunds_services_queue_before_active() public {
        activeDeposit(alice, 40e6, 1000);
        registerIntegration(mi);
        vm.prank(curator);
        vault.extendTo(address(mi), USDG, 40e6);

        // Bob's withdrawal for 100 queues with nothing to reserve.
        uint64 seq = doRequestWithdraw(bob, 0, 0, true);
        sendWithdrawAck(seq, bob, 100e6);
        assertEq(vault.totalQueuedPayouts(), 1);

        // Integration returns 120: queue is served FIRST, rest becomes active.
        uint256 bobBefore = tusdg.balanceOf(bob);
        tusdg.mint(address(this), 120e6);
        tusdg.approve(address(vault), type(uint256).max);
        vault.returnFunds(address(mi), USDG, 120e6);

        assertEq(tusdg.balanceOf(bob), bobBefore + 100e6);
        assertEq(vault.totalQueuedPayouts(), 0);
        (, uint128 active, uint128 reserved) = vault.funds(USDG);
        assertEq(active, 20e6);
        assertEq(reserved, 0);
        assertEq(vault.extendedOutstanding(address(mi)), 0);
    }

    function test_syncState_reports_funds_feePot_and_integration_raw() public {
        activeDeposit(alice, 100e6, 1000);
        registerIntegration(mi);
        mi.setRaw(hex"cafebabe1234");

        // Create some reserved via a queued payout.
        uint64 seq = doRequestWithdraw(bob, 0, 0, true);
        sendWithdrawAck(seq, bob, 300e6);
        (, uint128 active, uint128 reserved) = vault.funds(USDG);

        vm.recordLogs();
        vm.prank(makeAddr("anyone"));
        vault.syncState();

        (bytes memory env, bytes memory payload) = split(lastOutbound());
        (uint8 msgType,) = Wire.decodeEnvelope(env);
        assertEq(msgType, Wire.MSG_STATE_SYNC);
        Wire.StateSync memory s = Wire.decodeStateSync(payload);
        assertEq(s.spokeId, SPOKE_ID);
        assertEq(s.assets.length, 1);
        assertEq(s.assets[0].asset, USDG);
        assertEq(s.assets[0].free, active);
        assertEq(s.assets[0].reserved, reserved);
        assertEq(s.feePotBalance, uint128(vault.feePot()));
        assertEq(s.integrationRaw, hex"cafebabe1234");
        assertEq(s.tsMs, uint64(block.timestamp) * 1000);
    }
}

contract SpokeVaultRolesAndFeesTest is SpokeTestBase {
    function test_role_gating() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControl.AccessControlUnauthorizedAccount.selector,
                alice,
                vault.WHITELIST_ROLE()
            )
        );
        vm.prank(alice);
        vault.setWhitelistEnabled(true);

        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControl.AccessControlUnauthorizedAccount.selector, alice, vault.PAUSER_ROLE()
            )
        );
        vm.prank(alice);
        vault.setLocalPause(true);

        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControl.AccessControlUnauthorizedAccount.selector, alice, bytes32(0)
            )
        );
        vm.prank(alice);
        vault.bindEndpoint(2, makeAddr("ep"));

        // Curator / endpoint / integrations are NOT settable by the admin:
        // no such functions exist; only ConfigSync moves them. (Compile-time
        // property; here we just confirm the curator role check.)
        vm.prank(admin);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.NotCurator.selector, admin));
        vault.extendTo(makeAddr("i"), USDG, 1);
    }

    function test_two_step_default_admin_transfer() public {
        address newAdmin = makeAddr("newAdmin");
        assertEq(vault.defaultAdmin(), admin);

        vm.prank(admin);
        vault.beginDefaultAdminTransfer(newAdmin);

        // Accepting before the delay elapses is enforced.
        vm.prank(newAdmin);
        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControlDefaultAdminRules.AccessControlEnforcedDefaultAdminDelay.selector,
                uint48(block.timestamp) + ADMIN_DELAY
            )
        );
        vault.acceptDefaultAdminTransfer();

        // Only the pending admin can accept.
        vm.warp(block.timestamp + ADMIN_DELAY + 1);
        vm.prank(bob);
        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControlDefaultAdminRules.AccessControlInvalidDefaultAdmin.selector, bob
            )
        );
        vault.acceptDefaultAdminTransfer();

        vm.prank(newAdmin);
        vault.acceptDefaultAdminTransfer();
        assertEq(vault.defaultAdmin(), newAdmin);
        assertFalse(vault.hasRole(vault.DEFAULT_ADMIN_ROLE(), admin));
        assertTrue(vault.hasRole(vault.DEFAULT_ADMIN_ROLE(), newAdmin));

        // New admin can administer roles.
        bytes32 pauserRole = vault.PAUSER_ROLE();
        vm.prank(newAdmin);
        vault.grantRole(pauserRole, newAdmin);
    }

    function test_fee_pot_accounting() public {
        uint256 potBefore = vault.feePot();
        vault.fundFees{value: 0.5 ether}();
        assertEq(vault.feePot(), potBefore + 0.5 ether);

        // Plain native transfer also lands in the pot.
        (bool ok,) = address(vault).call{value: 0.25 ether}("");
        assertTrue(ok);
        assertEq(vault.feePot(), potBefore + 0.75 ether);
        assertEq(address(vault).balance, vault.feePot());
    }
}
