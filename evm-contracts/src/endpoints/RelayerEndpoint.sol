// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {AccessControlDefaultAdminRules} from
    "@openzeppelin/contracts/access/extensions/AccessControlDefaultAdminRules.sol";

import {IMessageEndpoint} from "../interfaces/IMessageEndpoint.sol";
import {ISpokeVault} from "../interfaces/ISpokeVault.sol";
import {Wire} from "../lib/Wire.sol";

/// @title RelayerEndpoint — dev/CI-only message endpoint (plan §2.3)
/// @notice A registered-sender gate for environments where neither
///         LayerZero nor CCIP exists. NEVER bound in production: inbound
///         trust is a bare RELAYER_ROLE, not a verification network.
///         Outbound messages are only emitted as events for an off-chain
///         relayer to pick up; the transport fee is zero.
contract RelayerEndpoint is AccessControlDefaultAdminRules, IMessageEndpoint {
    /// @notice Accounts allowed to deliver inbound messages (plan §6.1).
    bytes32 public constant RELAYER_ROLE = keccak256("RELAYER_ROLE");

    ISpokeVault public immutable VAULT;

    error NotVault(address caller);

    /// @notice Outbound message for the off-chain relayer to carry to the hub.
    event OutboundMessage(bytes message);

    constructor(address vault, uint48 adminTransferDelay, address admin)
        AccessControlDefaultAdminRules(adminTransferDelay, admin)
    {
        VAULT = ISpokeVault(vault);
    }

    /// @inheritdoc IMessageEndpoint
    function send(bytes calldata message) external payable {
        if (msg.sender != address(VAULT)) revert NotVault(msg.sender);
        emit OutboundMessage(message);
    }

    /// @inheritdoc IMessageEndpoint
    function quoteFee(bytes calldata) external pure returns (uint256) {
        return 0;
    }

    /// @notice Deliver a hub → spoke message: splits the wire envelope from
    ///         the payload and forwards to the vault.
    function deliver(bytes calldata message) external onlyRole(RELAYER_ROLE) {
        VAULT.handleMessage(message[:Wire.ENVELOPE_LEN], message[Wire.ENVELOPE_LEN:]);
    }
}
