// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {ChainId} from "../src/libraries/ChainId.sol";
import {Message} from "../src/libraries/Message.sol";

contract MessageTest is Test {
    bytes32 constant SRC_APP = 0xabababababababababababababababababababababababababababababababab;
    bytes32 constant DST_APP = 0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd;
    /// Shared cross-language test salt (0x01*32); DOMAIN_SEP mirrors it.
    bytes32 constant TEST_SALT = 0x0101010101010101010101010101010101010101010101010101010101010101;
    bytes32 immutable DOMAIN_SEP = Message.deriveDomainSep(TEST_SALT);

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

    /// Digest parity with the Move side under the shared TEST_SALT: this exact
    /// message hashes to the same value in
    /// `sui_bridge::message_tests::known_digest_vector` and the bridge-types
    /// `known_digest_vector`. If any encoding drifts, a signature made for one
    /// chain fails on the other.
    function test_known_digest_matches_sui() public view {
        bytes32 expected = 0x535392536947463d04988702a5480f431f34efed3cf557dc12aa434c2decd707;
        assertEq(Message.hash(_vectorMessage(), DOMAIN_SEP), expected);
    }

    function test_encode_length_is_fixed_header_plus_payload() public pure {
        Message.CrossChainMessage memory m = _vectorMessage();
        bytes memory enc = Message.encode(m);
        // 1 + 4 + 4 + 8 + 32 + 32 + 4 + payload.
        assertEq(enc.length, 1 + 4 + 4 + 8 + 32 + 32 + 4 + m.payload.length);
    }

    function test_hash_is_field_sensitive() public view {
        Message.CrossChainMessage memory a = _vectorMessage();
        Message.CrossChainMessage memory b = _vectorMessage();
        b.nonce = 8;
        assertTrue(Message.hash(a, DOMAIN_SEP) != Message.hash(b, DOMAIN_SEP));
    }

    function test_hash_is_domain_separated() public pure {
        bytes32 sepA = Message.deriveDomainSep(bytes32(uint256(1)));
        bytes32 sepB = Message.deriveDomainSep(bytes32(uint256(2)));
        assertTrue(Message.hash(_vectorMessage(), sepA) != Message.hash(_vectorMessage(), sepB));
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
