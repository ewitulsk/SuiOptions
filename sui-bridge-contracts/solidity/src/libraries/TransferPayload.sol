// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title TransferPayload
/// @notice Layer 2 transfer payload carried in `CrossChainMessage.payload`
///         (bridge-spec.md §3.2/§3.3). Byte-identical to the Move
///         `locker::transfer_payload` and Rust `bridge_types::transfer` codecs.
///
///         Fixed big-endian packed layout (72 bytes):
///           assetId   bytes32           32
///           amount    uint64 big-endian  8   (wire amount, fixed WIRE_DECIMALS)
///           recipient bytes32           32
library TransferPayload {
    /// Shared wire precision; the Locker scales to/from local decimals.
    uint8 internal constant WIRE_DECIMALS = 8;
    uint256 internal constant ENCODED_LEN = 72;

    struct Data {
        bytes32 assetId;
        uint64 amount;
        bytes32 recipient;
    }

    error BadPayloadLength(uint256 length);

    function encode(Data memory d) internal pure returns (bytes memory) {
        return abi.encodePacked(d.assetId, d.amount, d.recipient);
    }

    function decode(bytes memory b) internal pure returns (Data memory d) {
        if (b.length != ENCODED_LEN) revert BadPayloadLength(b.length);
        bytes32 assetId;
        uint64 amount;
        bytes32 recipient;
        assembly {
            assetId := mload(add(b, 0x20)) // bytes [0,32)
            amount := shr(192, mload(add(b, 0x40))) // top 8 bytes of [32,64)
            recipient := mload(add(b, 0x48)) // bytes [40,72)
        }
        d = Data(assetId, amount, recipient);
    }
}
