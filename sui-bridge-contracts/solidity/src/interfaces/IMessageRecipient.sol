// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title IMessageRecipient
/// @notice Destination-app callback the Inbox dispatches to (bridge-spec.md
///         §3.4 `onReceive`). The app MUST check `msg.sender == inbox` and that
///         `srcApp` is its registered peer before acting on `payload`.
interface IMessageRecipient {
    function onReceive(uint32 srcChainId, bytes32 srcApp, bytes calldata payload) external;
}
