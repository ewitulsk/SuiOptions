// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test, Vm} from "forge-std/Test.sol";
import {SpokeVault} from "../../src/SpokeVault.sol";
import {RelayerEndpoint} from "../../src/endpoints/RelayerEndpoint.sol";
import {TUSDG} from "../../src/TUSDG.sol";
import {Wire} from "../../src/lib/Wire.sol";
import {MockIntegration} from "../mocks/MockIntegration.sol";

/// @notice Shared fixture: TUSDG + RelayerEndpoint + SpokeVault wired
///         together, with helpers to act as the hub (build wire messages,
///         deliver them via the relayer) and to inspect outbound messages.
abstract contract SpokeTestBase is Test {
    uint64 internal constant HUB_CHAIN = 1;
    uint64 internal constant LOCAL_CHAIN = 0x101;
    uint64 internal constant SPOKE_ID = 3;
    bytes32 internal constant HUB_APP = bytes32(uint256(0xbeef));
    uint8 internal constant USDG = 1;
    uint8 internal constant RELAYER_EP_ID = 1;
    uint64 internal constant DEPOSIT_TIMEOUT = 24 hours;
    uint64 internal constant HEARTBEAT_TIMEOUT = 6 hours;
    uint48 internal constant ADMIN_DELAY = 3 days;

    TUSDG internal tusdg;
    SpokeVault internal vault;
    RelayerEndpoint internal relayer;

    address internal admin = makeAddr("admin");
    address internal curator = makeAddr("curator");
    address internal relayerBot = makeAddr("relayerBot");
    address internal whitelister = makeAddr("whitelister");
    address internal pauser = makeAddr("pauser");
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");

    uint64 internal hubSeq;

    function setUp() public virtual {
        vm.warp(1_000_000);
        tusdg = new TUSDG();
        address predicted = vm.computeCreateAddress(address(this), vm.getNonce(address(this)) + 1);
        relayer = new RelayerEndpoint(predicted, 0, admin);
        vault = new SpokeVault(_config(address(relayer)));
        assertEq(address(vault), predicted, "vault address prediction");

        vm.startPrank(admin);
        vault.grantRole(vault.WHITELIST_ROLE(), whitelister);
        vault.grantRole(vault.PAUSER_ROLE(), pauser);
        relayer.grantRole(relayer.RELAYER_ROLE(), relayerBot);
        vm.stopPrank();

        tusdg.mint(alice, 1_000_000e6);
        tusdg.mint(bob, 1_000_000e6);
        vm.prank(alice);
        tusdg.approve(address(vault), type(uint256).max);
        vm.prank(bob);
        tusdg.approve(address(vault), type(uint256).max);

        vault.fundFees{value: 1 ether}();
    }

    function _config(address endpoint) internal view returns (SpokeVault.Config memory) {
        uint8[] memory codes = new uint8[](1);
        codes[0] = USDG;
        address[] memory tokens = new address[](1);
        tokens[0] = address(tusdg);
        return SpokeVault.Config({
            admin: admin,
            adminTransferDelay: ADMIN_DELAY,
            curator: curator,
            endpointId: RELAYER_EP_ID,
            endpoint: endpoint,
            spokeId: SPOKE_ID,
            localChainId: LOCAL_CHAIN,
            hubChainId: HUB_CHAIN,
            hubApp: HUB_APP,
            assetCodes: codes,
            assetTokens: tokens,
            payoutAssetCode: USDG,
            depositTimeout: DEPOSIT_TIMEOUT,
            heartbeatTimeout: HEARTBEAT_TIMEOUT
        });
    }

    // ───────────────────── acting as the hub ───────────────────────────

    function hubEnv() internal returns (Wire.Envelope memory) {
        return Wire.Envelope({
            srcChainId: HUB_CHAIN,
            dstChainId: LOCAL_CHAIN,
            srcApp: HUB_APP,
            dstApp: bytes32(uint256(uint160(address(vault)))),
            seq: ++hubSeq
        });
    }

    function deliver(bytes memory message) internal {
        vm.prank(relayerBot);
        relayer.deliver(message);
    }

    function sendDepositAck(uint64 depositSeq, bool accepted, uint128 shares) internal {
        deliver(Wire.encodeDepositAck(hubEnv(), Wire.DepositAck(depositSeq, accepted, shares)));
    }

    function sendWithdrawAck(uint64 requestSeq, address user, uint128 payAmount) internal {
        deliver(
            Wire.encodeWithdrawAck(
                hubEnv(),
                Wire.WithdrawAck(requestSeq, bytes32(uint256(uint160(user))), payAmount)
            )
        );
    }

    function sendConfigSync(
        bool paused_,
        bool riskOff_,
        address curator_,
        uint8 endpointId,
        bytes32 root
    ) internal {
        deliver(
            Wire.encodeConfigSync(
                hubEnv(),
                Wire.ConfigSync({
                    paused: paused_,
                    riskOff: riskOff_,
                    curator: bytes32(uint256(uint160(curator_))),
                    endpoint: endpointId,
                    integrationsRoot: root
                })
            )
        );
    }

    // ───────────────────────── user actions ────────────────────────────

    function doDeposit(address user, uint256 amount, uint8 tranche) internal returns (uint64) {
        vm.prank(user);
        vault.deposit(USDG, amount, tranche);
        return vault.depositSeq();
    }

    /// @dev Deposit + accepted ACK: `amount` lands in `active`.
    function activeDeposit(address user, uint256 amount, uint128 shares) internal returns (uint64) {
        uint64 seq = doDeposit(user, amount, 0);
        sendDepositAck(seq, true, shares);
        return seq;
    }

    function doRequestWithdraw(address user, uint8 tranche, uint128 shares, bool all)
        internal
        returns (uint64)
    {
        vm.prank(user);
        vault.requestWithdraw(tranche, shares, all);
        return vault.requestSeq();
    }

    function registerIntegration(MockIntegration mi) internal {
        address[] memory list = new address[](1);
        list[0] = address(mi);
        sendConfigSync(false, false, curator, RELAYER_EP_ID, keccak256(abi.encode(list)));
        vault.setIntegrations(list);
    }

    // ───────────────────── outbound inspection ─────────────────────────

    /// @dev Use with vm.recordLogs(): returns the last OutboundMessage the
    ///      relayer endpoint emitted since recording started.
    function lastOutbound() internal returns (bytes memory) {
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 topic = keccak256("OutboundMessage(bytes)");
        for (uint256 i = logs.length; i > 0; i--) {
            if (logs[i - 1].topics[0] == topic) {
                return abi.decode(logs[i - 1].data, (bytes));
            }
        }
        revert("no OutboundMessage recorded");
    }

    function split(bytes memory m) internal pure returns (bytes memory env, bytes memory payload) {
        env = new bytes(Wire.ENVELOPE_LEN);
        payload = new bytes(m.length - Wire.ENVELOPE_LEN);
        for (uint256 i = 0; i < Wire.ENVELOPE_LEN; i++) {
            env[i] = m[i];
        }
        for (uint256 i = 0; i < payload.length; i++) {
            payload[i] = m[Wire.ENVELOPE_LEN + i];
        }
    }
}
