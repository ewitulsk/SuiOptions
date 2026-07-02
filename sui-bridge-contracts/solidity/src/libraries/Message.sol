// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title Message
/// @notice Canonical cross-chain message + keccak256 digest. The encoding MUST
///         be byte-identical to `sui_bridge::message` (Move) so one threshold
///         signature verifies on both chains (bridge-spec.md §2.2).
///
///         Fixed big-endian packed layout — `abi.encodePacked` reproduces the
///         exact bytes the Move `encode` emits:
///
///         version (u8) | srcChainId (u32) | dstChainId (u32) | nonce (u64)
///           | srcApp (bytes32) | dstApp (bytes32) | payloadLen (u32) | payload
///
///         message_hash = keccak256(DOMAIN_SEP || encode(message)), where
///         DOMAIN_SEP = keccak256("XCHAIN_MSG_V1" || deploymentSalt) binds every
///         signed message to one logical deployment (spec §2.2). Signers sign
///         over this 32-byte digest directly (no EIP-191 prefix), matching the
///         Ed25519 path on Sui.
library Message {
    uint8 internal constant VERSION = 1;

    /// @notice Domain-separation tag hashed with the per-deployment salt.
    bytes13 internal constant DOMAIN_TAG = "XCHAIN_MSG_V1";

    error BadVersion(uint8 version);

    struct CrossChainMessage {
        uint8 version;
        uint32 srcChainId;
        uint32 dstChainId;
        uint64 nonce;
        bytes32 srcApp;
        bytes32 dstApp;
        bytes payload;
    }

    function encode(CrossChainMessage memory m) internal pure returns (bytes memory) {
        if (m.version != VERSION) revert BadVersion(m.version);
        return abi.encodePacked(
            m.version,
            m.srcChainId,
            m.dstChainId,
            m.nonce,
            m.srcApp,
            m.dstApp,
            uint32(m.payload.length),
            m.payload
        );
    }

    /// @notice DOMAIN_SEP = keccak256("XCHAIN_MSG_V1" || deploymentSalt).
    ///         Derived once at contract construction so the stored separator is
    ///         auditable on-chain.
    function deriveDomainSep(bytes32 deploymentSalt) internal pure returns (bytes32) {
        return keccak256(abi.encodePacked(DOMAIN_TAG, deploymentSalt));
    }

    /// @notice keccak256(domainSep || encode(message)) — the digest signers sign.
    function hash(CrossChainMessage memory m, bytes32 domainSep) internal pure returns (bytes32) {
        return keccak256(abi.encodePacked(domainSep, encode(m)));
    }

    /// @notice Left-pad an EVM address into a bytes32 app identity (spec §2.2).
    function addressToBytes32(address a) internal pure returns (bytes32) {
        return bytes32(uint256(uint160(a)));
    }

    /// @notice Recover an EVM address from a bytes32 app identity.
    function bytes32ToAddress(bytes32 b) internal pure returns (address) {
        return address(uint160(uint256(b)));
    }
}
