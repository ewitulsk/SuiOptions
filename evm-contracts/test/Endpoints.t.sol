// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IAccessControl} from "@openzeppelin/contracts/access/IAccessControl.sol";

import {SpokeVault} from "../src/SpokeVault.sol";
import {LayerZeroEndpoint} from "../src/endpoints/LayerZeroEndpoint.sol";
import {CCIPEndpoint} from "../src/endpoints/CCIPEndpoint.sol";
import {RelayerEndpoint} from "../src/endpoints/RelayerEndpoint.sol";
import {Wire} from "../src/lib/Wire.sol";
import {Origin} from "../src/vendor/ILayerZeroEndpointV2.sol";
import {Client} from "../src/vendor/CCIP.sol";
import {MockLzEndpoint} from "./mocks/MockLzEndpoint.sol";
import {MockCcipRouter} from "./mocks/MockCcipRouter.sol";
import {SpokeTestBase} from "./utils/SpokeTestBase.sol";

contract EndpointsTest is SpokeTestBase {
    uint8 constant LZ_EP_ID = 2;
    uint8 constant CCIP_EP_ID = 3;
    uint32 constant HUB_EID = 30_168;
    bytes32 constant HUB_PEER = bytes32(uint256(0xa11ce));
    uint64 constant HUB_SELECTOR = 123_456_789;

    MockLzEndpoint lzMock;
    LayerZeroEndpoint lzEp;
    MockCcipRouter ccipMock;
    CCIPEndpoint ccipEp;
    bytes hubSender;

    function setUp() public override {
        super.setUp();
        lzMock = new MockLzEndpoint();
        lzEp = new LayerZeroEndpoint(address(vault), address(lzMock), HUB_EID, HUB_PEER, "");
        hubSender = abi.encodePacked(HUB_APP);
        ccipMock = new MockCcipRouter();
        ccipEp = new CCIPEndpoint(address(vault), address(ccipMock), HUB_SELECTOR, hubSender, "");
        vm.startPrank(admin);
        vault.bindEndpoint(LZ_EP_ID, address(lzEp));
        vault.bindEndpoint(CCIP_EP_ID, address(ccipEp));
        vm.stopPrank();
    }

    function _switchTo(uint8 endpointId) internal {
        sendConfigSync(false, false, curator, endpointId, bytes32(0));
    }

    // ─────────────────────────── relayer ───────────────────────────────

    function test_relayer_deliver_requires_role() public {
        bytes memory m = Wire.encodeConfigSync(
            hubEnv(),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), RELAYER_EP_ID, bytes32(0))
        );
        bytes32 relayerRole = relayer.RELAYER_ROLE();
        vm.prank(alice);
        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControl.AccessControlUnauthorizedAccount.selector, alice, relayerRole
            )
        );
        relayer.deliver(m);
    }

    function test_relayer_send_only_vault_and_fee_zero() public {
        assertEq(relayer.quoteFee(hex""), 0);
        vm.expectRevert(abi.encodeWithSelector(RelayerEndpoint.NotVault.selector, address(this)));
        relayer.send(hex"01");
    }

    // ───────────────── ConfigSync endpoint switch drill ────────────────

    function test_switch_drill_relayer_to_lz() public {
        assertEq(vault.activeEndpoint(), address(relayer));

        vm.expectEmit(true, false, false, true);
        emit SpokeVault.EndpointSwitched(LZ_EP_ID, address(lzEp));
        _switchTo(LZ_EP_ID);
        assertEq(vault.activeEndpoint(), address(lzEp));

        // The old endpoint can no longer deliver.
        bytes memory m = Wire.encodeConfigSync(
            hubEnv(),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), LZ_EP_ID, bytes32(0))
        );
        vm.prank(relayerBot);
        vm.expectRevert(abi.encodeWithSelector(SpokeVault.NotEndpoint.selector, address(relayer)));
        relayer.deliver(m);

        // Delivery through the LayerZero mock works (same message, same lane seq).
        lzMock.deliver(address(lzEp), HUB_EID, HUB_PEER, m);
        assertEq(vault.lastInboundSeq(), hubSeq);

        // Outbound now flows through LayerZero: fee quoted and paid from the pot.
        lzMock.setFee(0.01 ether);
        uint256 potBefore = vault.feePot();
        doDeposit(alice, 100e6, 0);
        assertEq(vault.feePot(), potBefore - 0.01 ether);
        assertEq(address(lzMock).balance, 0.01 ether);
        assertEq(lzMock.lastDstEid(), HUB_EID);
        assertEq(lzMock.lastReceiver(), HUB_PEER);
        (bytes memory env, bytes memory payload) = split(lzMock.lastMessage());
        (uint8 msgType,) = Wire.decodeEnvelope(env);
        assertEq(msgType, Wire.MSG_DEPOSIT_NOTICE);
        assertEq(Wire.decodeDepositNotice(payload).amount, 100e6);

        // And back to CCIP via a ConfigSync through the now-active LZ lane.
        bytes memory toCcip = Wire.encodeConfigSync(
            hubEnv(),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), CCIP_EP_ID, bytes32(0))
        );
        lzMock.deliver(address(lzEp), HUB_EID, HUB_PEER, toCcip);
        assertEq(vault.activeEndpoint(), address(ccipEp));
    }

    // ─────────────────────── fee pot exhaustion ────────────────────────

    function test_fee_pot_exhaustion_reverts_user_action() public {
        _switchTo(LZ_EP_ID);
        lzMock.setFee(2 ether); // pot only holds 1 ether
        uint256 pot = vault.feePot();
        vm.prank(alice);
        vm.expectRevert(
            abi.encodeWithSelector(SpokeVault.FeePotInsufficient.selector, 2 ether, pot)
        );
        vault.deposit(USDG, 100e6, 0);

        // Nothing escrowed on the failed attempt.
        (uint128 pending,,) = vault.funds(USDG);
        assertEq(pending, 0);

        // Topping up the pot unblocks the deposit.
        vault.fundFees{value: 2 ether}();
        doDeposit(alice, 100e6, 0);
        assertEq(lzMock.sendCount(), 1);
    }

    // ─────────────────────────── LayerZero ─────────────────────────────

    function test_lzReceive_guards() public {
        _switchTo(LZ_EP_ID);
        bytes memory m = Wire.encodeConfigSync(
            hubEnv(),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), LZ_EP_ID, bytes32(0))
        );

        // Only the LayerZero endpoint contract may call lzReceive.
        vm.expectRevert(
            abi.encodeWithSelector(LayerZeroEndpoint.NotLzEndpoint.selector, address(this))
        );
        lzEp.lzReceive(Origin(HUB_EID, HUB_PEER, 1), bytes32(0), m, address(0), "");

        // Wrong source EID.
        vm.expectRevert(
            abi.encodeWithSelector(LayerZeroEndpoint.BadPeer.selector, HUB_EID + 1, HUB_PEER)
        );
        lzMock.deliver(address(lzEp), HUB_EID + 1, HUB_PEER, m);

        // Wrong peer.
        vm.expectRevert(
            abi.encodeWithSelector(LayerZeroEndpoint.BadPeer.selector, HUB_EID, bytes32(uint256(1)))
        );
        lzMock.deliver(address(lzEp), HUB_EID, bytes32(uint256(1)), m);

        // Correct origin delivers.
        lzMock.deliver(address(lzEp), HUB_EID, HUB_PEER, m);
        assertEq(vault.lastInboundSeq(), hubSeq);
    }

    function test_lz_send_only_vault() public {
        vm.expectRevert(abi.encodeWithSelector(LayerZeroEndpoint.NotVault.selector, address(this)));
        lzEp.send(hex"01");
    }

    // ────────────────────────────── CCIP ───────────────────────────────

    function test_ccip_delivery_and_guards() public {
        _switchTo(CCIP_EP_ID);
        bytes memory m = Wire.encodeConfigSync(
            hubEnv(),
            Wire.ConfigSync(false, false, bytes32(uint256(uint160(curator))), CCIP_EP_ID, bytes32(0))
        );

        // Only the router may call ccipReceive → delivered via mock only.
        vm.expectRevert(abi.encodeWithSelector(CCIPEndpoint.NotRouter.selector, address(this)));
        ccipEp.ccipReceive(
            Client.Any2EVMMessage({
                messageId: bytes32(0),
                sourceChainSelector: HUB_SELECTOR,
                sender: hubSender,
                data: m,
                destTokenAmounts: new Client.EVMTokenAmount[](0)
            })
        );

        // Wrong source chain selector.
        vm.expectRevert(
            abi.encodeWithSelector(CCIPEndpoint.BadSource.selector, HUB_SELECTOR + 1, hubSender)
        );
        ccipMock.deliver(address(ccipEp), HUB_SELECTOR + 1, hubSender, m);

        // Wrong sender.
        bytes memory badSender = hex"deadbeef";
        vm.expectRevert(
            abi.encodeWithSelector(CCIPEndpoint.BadSource.selector, HUB_SELECTOR, badSender)
        );
        ccipMock.deliver(address(ccipEp), HUB_SELECTOR, badSender, m);

        // Correct source delivers.
        ccipMock.deliver(address(ccipEp), HUB_SELECTOR, hubSender, m);
        assertEq(vault.lastInboundSeq(), hubSeq);
    }

    function test_ccip_outbound_send_pays_router_fee() public {
        _switchTo(CCIP_EP_ID);
        ccipMock.setFee(0.02 ether);
        uint256 potBefore = vault.feePot();
        doDeposit(alice, 42e6, 1);
        assertEq(vault.feePot(), potBefore - 0.02 ether);
        assertEq(address(ccipMock).balance, 0.02 ether);
        assertEq(ccipMock.lastSelector(), HUB_SELECTOR);
        assertEq(ccipMock.lastReceiver(), hubSender);
        (bytes memory env, bytes memory payload) = split(ccipMock.lastData());
        (uint8 msgType,) = Wire.decodeEnvelope(env);
        assertEq(msgType, Wire.MSG_DEPOSIT_NOTICE);
        Wire.DepositNotice memory n = Wire.decodeDepositNotice(payload);
        assertEq(n.amount, 42e6);
        assertEq(n.tranche, 1);
    }

    function test_ccip_send_only_vault() public {
        vm.expectRevert(abi.encodeWithSelector(CCIPEndpoint.NotVault.selector, address(this)));
        ccipEp.send(hex"01");
    }
}
