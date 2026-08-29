// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title Minimal LayerZero V2 endpoint interface (vendored)
/// @notice Trimmed to the surface `LayerZeroEndpoint.sol` needs. Canonical
///         upstream: `@layerzerolabs/lz-evm-protocol-v2/contracts/interfaces/
///         ILayerZeroEndpointV2.sol` (LayerZero-Labs/LayerZero-v2). Field
///         names and struct layouts match the upstream definitions so a
///         real endpoint is call-compatible.
struct MessagingParams {
    uint32 dstEid;
    bytes32 receiver;
    bytes message;
    bytes options;
    bool payInLzToken;
}

struct MessagingFee {
    uint256 nativeFee;
    uint256 lzTokenFee;
}

struct MessagingReceipt {
    bytes32 guid;
    uint64 nonce;
    MessagingFee fee;
}

/// @notice Origin of a delivered LayerZero message (upstream:
///         `ILayerZeroReceiver.Origin`).
struct Origin {
    uint32 srcEid;
    bytes32 sender;
    uint64 nonce;
}

interface ILayerZeroEndpointV2 {
    function send(MessagingParams calldata params, address refundAddress)
        external
        payable
        returns (MessagingReceipt memory);

    function quote(MessagingParams calldata params, address sender)
        external
        view
        returns (MessagingFee memory);
}

/// @notice Receiver surface the LayerZero endpoint calls on delivery
///         (upstream: `ILayerZeroReceiver`, trimmed).
interface ILayerZeroReceiver {
    function lzReceive(
        Origin calldata origin,
        bytes32 guid,
        bytes calldata message,
        address executor,
        bytes calldata extraData
    ) external payable;
}
