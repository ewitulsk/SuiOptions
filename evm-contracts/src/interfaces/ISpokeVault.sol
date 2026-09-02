// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title ISpokeVault — inbound surface an endpoint delivers into
interface ISpokeVault {
    /// @notice Deliver a verified hub → spoke message.
    /// @dev Callable only by the vault's active endpoint. `envelope` is the
    ///      90-byte wire header, `payload` the message body.
    function handleMessage(bytes calldata envelope, bytes calldata payload) external;
}
