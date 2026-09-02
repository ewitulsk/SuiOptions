// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title IMessageEndpoint — transport-agnostic message endpoint (plan §2.3)
/// @notice A `SpokeVault` talks to exactly one active endpoint at a time.
///         Outbound: the vault quotes the transport fee, pays it from its
///         fee pot, and calls `send`. Inbound: the endpoint verifies
///         delivery via its own transport, then calls
///         `spokeVault.handleMessage(envelope, payload)`; the vault checks
///         `msg.sender == active endpoint` plus the wire seq.
interface IMessageEndpoint {
    /// @notice Send a fully wire-encoded message (envelope ‖ payload) to the hub.
    /// @dev `msg.value` must cover the transport fee quoted by `quoteFee`.
    function send(bytes calldata message) external payable;

    /// @notice Quote the native fee required to send `message`.
    function quoteFee(bytes calldata message) external view returns (uint256);
}
