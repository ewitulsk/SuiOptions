// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Envelope} from "./libraries/Envelope.sol";
import {Message} from "./libraries/Message.sol";
import {Registry} from "./Registry.sol";
import {IMessageRecipient} from "./interfaces/IMessageRecipient.sol";

/// @title Inbox
/// @notice One per chain. Verifies an aggregated threshold ECDSA signature
///         against the registered group key, enforces exactly-once delivery,
///         and dispatches the payload to the destination app (bridge-spec.md
///         §2.5). Unlike Move, the EVM has dynamic dispatch, so delivery is a
///         direct `onReceive` call rather than a hot potato.
///
///         Ordering is windowed/out-of-order (§2.6): the `consumed` hash-set is
///         the exactly-once guard, marked before the external call
///         (checks-effects-interactions) so a reentrant replay reverts.
contract Inbox {
    Registry public immutable registry;
    /// Internal id of THIS chain; every accepted message must target it.
    uint32 public immutable dstChainId;

    mapping(bytes32 => bool) public consumed;
    mapping(uint32 => uint64) public highestNonce; // observability only
    bool public paused;

    event MessageDelivered(bytes32 indexed messageHash, uint32 srcChainId, uint64 nonce);
    event PausedSet(bool paused);

    error InboxPaused();
    error WrongDstChain(uint32 expected, uint32 got);
    error AlreadyConsumed(bytes32 messageHash);
    error SchemeKeyMismatch(uint8 registered, uint8 envelope);
    error UnsupportedScheme(uint8 schemeTag);
    error BadGroupKeyLength(uint256 length);
    error NotGuardian();

    modifier onlyGuardian() {
        if (msg.sender != registry.guardian()) revert NotGuardian();
        _;
    }

    constructor(Registry registry_, uint32 dstChainId_) {
        registry = registry_;
        dstChainId = dstChainId_;
    }

    /// @notice Verify and deliver a message. Named `receiveMessage` because
    ///         `receive` is reserved by Solidity (the ether-receive function).
    ///         The relayer is untrusted: every check is self-contained here.
    function receiveMessage(
        Message.CrossChainMessage calldata message,
        Envelope.SignatureEnvelope calldata envelope
    ) external {
        if (paused) revert InboxPaused();
        if (message.dstChainId != dstChainId) revert WrongDstChain(dstChainId, message.dstChainId);

        bytes32 messageHash = Message.hash(message);
        if (consumed[messageHash]) revert AlreadyConsumed(messageHash);

        (uint8 registeredScheme, bytes memory key) = registry.groupKey(envelope.groupPubkeyId);
        if (registeredScheme != envelope.schemeTag) {
            revert SchemeKeyMismatch(registeredScheme, envelope.schemeTag);
        }
        if (envelope.schemeTag != Envelope.SCHEME_ECDSA_SECP256K1) {
            revert UnsupportedScheme(envelope.schemeTag);
        }
        Envelope.verifyEcdsa(messageHash, envelope.signature, _keyToAddress(key));

        // Effects before interaction (replay-safe under reentrancy).
        consumed[messageHash] = true;
        if (message.nonce > highestNonce[message.srcChainId]) {
            highestNonce[message.srcChainId] = message.nonce;
        }
        emit MessageDelivered(messageHash, message.srcChainId, message.nonce);

        IMessageRecipient(Message.bytes32ToAddress(message.dstApp)).onReceive(
            message.srcChainId, message.srcApp, message.payload
        );
    }

    /// @notice Global inbound circuit breaker (§2.7). Guardian-gated.
    function setPaused(bool paused_) external onlyGuardian {
        paused = paused_;
        emit PausedSet(paused_);
    }

    /// @dev Decode a 20-byte registered ECDSA group key into an address.
    function _keyToAddress(bytes memory key) private pure returns (address addr) {
        if (key.length != 20) revert BadGroupKeyLength(key.length);
        assembly {
            // First 32 bytes of `key` data hold the 20-byte address left-aligned;
            // shift right 96 bits to right-align it.
            addr := shr(96, mload(add(key, 0x20)))
        }
    }
}

