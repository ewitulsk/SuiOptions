// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title Envelope
/// @notice Signature envelope + the EVM verify adapter. Mirrors the
///         `(scheme_tag, group_pubkey_id, signature)` shape of
///         `sui_bridge::envelope` (bridge-spec.md §2.3). Messages destined for
///         EVM are signed with GG20/CGGMP threshold ECDSA (secp256k1) and
///         verified here via `ecrecover` against the registered group address.
library Envelope {
    uint8 internal constant SCHEME_ED25519 = 0;
    uint8 internal constant SCHEME_ECDSA_SECP256K1 = 1;

    // secp256k1 group order / 2 — reject the high-s malleable variant.
    uint256 internal constant SECP256K1_HALF_N =
        0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0;

    struct SignatureEnvelope {
        uint8 schemeTag;
        uint32 groupPubkeyId;
        bytes signature; // ECDSA: 65 bytes, r || s || v
    }

    error BadSignatureLength(uint256 length);
    error MalleableSignature();
    error InvalidSignature();

    /// @notice Verify a 65-byte ECDSA signature over `digest` recovers to
    ///         `groupAddr`. Reverts otherwise.
    function verifyEcdsa(bytes32 digest, bytes memory signature, address groupAddr) internal pure {
        if (signature.length != 65) revert BadSignatureLength(signature.length);

        bytes32 r;
        bytes32 s;
        uint8 v;
        assembly {
            r := mload(add(signature, 0x20))
            s := mload(add(signature, 0x40))
            v := byte(0, mload(add(signature, 0x60)))
        }
        if (uint256(s) > SECP256K1_HALF_N) revert MalleableSignature();
        if (v != 27 && v != 28) revert InvalidSignature();

        address recovered = ecrecover(digest, v, r, s);
        if (recovered == address(0) || recovered != groupAddr) revert InvalidSignature();
    }
}
