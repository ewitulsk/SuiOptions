// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {TransferPayload} from "../src/libraries/TransferPayload.sol";

contract TransferPayloadTest is Test {
    bytes32 constant ASSET = 0x1111111111111111111111111111111111111111111111111111111111111111;
    bytes32 constant RECIP = 0x2222222222222222222222222222222222222222222222222222222222222222;

    /// Parity vector: identical bytes to the Move + Rust `known_encoding_vector`.
    function test_known_encoding_vector() public pure {
        TransferPayload.Data memory d = TransferPayload.Data(ASSET, 123_456_789, RECIP);
        bytes memory enc = TransferPayload.encode(d);
        assertEq(
            enc,
            hex"111111111111111111111111111111111111111111111111111111111111111100000000075bcd152222222222222222222222222222222222222222222222222222222222222222"
        );
        assertEq(enc.length, 72);
    }

    function test_round_trips() public pure {
        TransferPayload.Data memory d = TransferPayload.Data(ASSET, type(uint64).max, RECIP);
        TransferPayload.Data memory back = TransferPayload.decode(TransferPayload.encode(d));
        assertEq(back.assetId, d.assetId);
        assertEq(back.amount, d.amount);
        assertEq(back.recipient, d.recipient);
    }

    function test_rejects_bad_length() public {
        vm.expectRevert(abi.encodeWithSelector(TransferPayload.BadPayloadLength.selector, uint256(71)));
        this.decodeExt(new bytes(71));
    }

    function decodeExt(bytes calldata b) external pure returns (TransferPayload.Data memory) {
        return TransferPayload.decode(b);
    }
}
