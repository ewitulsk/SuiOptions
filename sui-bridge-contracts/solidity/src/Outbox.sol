// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Message} from "./libraries/Message.sol";
import {Registry} from "./Registry.sol";

/// @title Outbox
/// @notice One per chain. Apps call `send` to emit a cross-chain message; it
///         assigns a per-destination nonce, computes the canonical keccak256
///         digest, and emits `MessageCommitted` for the signer group to observe
///         (bridge-spec.md §2.4). `srcApp` is the caller's left-padded address.
contract Outbox {
    Registry public immutable registry;
    /// Internal id of THIS chain (the source for everything it emits).
    uint32 public immutable srcChainId;
    /// Digest domain separator (spec §2.2), derived from the deployment salt.
    bytes32 public immutable domainSep;

    /// nextNonce[dstChainId] — monotonic per (src, dst) lane (§2.6).
    mapping(uint32 => uint64) public nextNonce;
    bool public paused;

    event MessageCommitted(
        bytes32 indexed messageHash,
        uint32 srcChainId,
        uint32 dstChainId,
        uint64 nonce,
        bytes32 srcApp,
        bytes32 dstApp,
        bytes payload
    );
    event PausedSet(bool paused);

    error OutboxPaused();
    error NotGuardian();

    modifier onlyGuardian() {
        if (msg.sender != registry.guardian()) revert NotGuardian();
        _;
    }

    constructor(Registry registry_, uint32 srcChainId_, bytes32 deploymentSalt) {
        registry = registry_;
        srcChainId = srcChainId_;
        domainSep = Message.deriveDomainSep(deploymentSalt);
    }

    /// @notice Emit a message to `dstApp` on `dstChainId`. Returns the assigned
    ///         nonce and canonical message hash.
    function send(uint32 dstChainId, bytes32 dstApp, bytes calldata payload)
        external
        returns (uint64 nonce, bytes32 messageHash)
    {
        if (paused) revert OutboxPaused();

        nonce = nextNonce[dstChainId];
        bytes32 srcApp = Message.addressToBytes32(msg.sender);
        Message.CrossChainMessage memory m = Message.CrossChainMessage({
            version: Message.VERSION,
            srcChainId: srcChainId,
            dstChainId: dstChainId,
            nonce: nonce,
            srcApp: srcApp,
            dstApp: dstApp,
            payload: payload
        });
        messageHash = Message.hash(m, domainSep);

        nextNonce[dstChainId] = nonce + 1;

        emit MessageCommitted(messageHash, srcChainId, dstChainId, nonce, srcApp, dstApp, payload);
    }

    /// @notice Global outbound circuit breaker (§2.7). Guardian-gated.
    function setPaused(bool paused_) external onlyGuardian {
        paused = paused_;
        emit PausedSet(paused_);
    }
}
