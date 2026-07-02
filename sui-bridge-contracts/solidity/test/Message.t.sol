// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {ChainId} from "../src/libraries/ChainId.sol";
import {Message} from "../src/libraries/Message.sol";

contract MessageTest is Test {
    bytes32 constant SRC_APP = 0xabababababababababababababababababababababababababababababababab;
    bytes32 constant DST_APP = 0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd;

    function _vectorMessage() internal pure returns (Message.CrossChainMessage memory) {
        return Message.CrossChainMessage({
            version: Message.VERSION,
            srcChainId: ChainId.encode(ChainId.FAMILY_EVM, 998), // 268436454
            dstChainId: ChainId.encode(ChainId.FAMILY_SUI, 0), // 134217728
            nonce: 7,
            srcApp: SRC_APP,
            dstApp: DST_APP,
            payload: bytes("hello-bridge")
        });
    }

    /// Digest parity with the Move side: this exact message hashes to the same
    /// value in `sui_bridge::message_tests::known_digest_vector`. If either
    /// encoding drifts, a signature made for one chain fails on the other.
    function test_known_digest_matches_sui() public pure {
        bytes32 expected = 0x7b767c416104fbef99880be0416fa07353493afb6547ad67d700029ce09572af;
        assertEq(Message.hash(_vectorMessage()), expected);
    }

    function test_encode_length_is_fixed_header_plus_payload() public pure {
        Message.CrossChainMessage memory m = _vectorMessage();
        bytes memory enc = Message.encode(m);
        // 1 + 4 + 4 + 8 + 32 + 32 + 4 + payload.
        assertEq(enc.length, 1 + 4 + 4 + 8 + 32 + 32 + 4 + m.payload.length);
    }

    function test_hash_is_field_sensitive() public pure {
        Message.CrossChainMessage memory a = _vectorMessage();
        Message.CrossChainMessage memory b = _vectorMessage();
        b.nonce = 8;
        assertTrue(Message.hash(a) != Message.hash(b));
    }

    function test_encode_rejects_bad_version() public {
        Message.CrossChainMessage memory m = _vectorMessage();
        m.version = 2;
        vm.expectRevert(abi.encodeWithSelector(Message.BadVersion.selector, uint8(2)));
        this.encodeExt(m);
    }

    function test_address_bytes32_round_trip() public pure {
        address a = address(0x1234567890AbcdEF1234567890aBcdef12345678);
        assertEq(Message.bytes32ToAddress(Message.addressToBytes32(a)), a);
    }

    function encodeExt(Message.CrossChainMessage calldata m) external pure returns (bytes memory) {
        return Message.encode(m);
    }
}
