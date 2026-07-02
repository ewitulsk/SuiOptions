// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ChainId} from "./libraries/ChainId.sol";

/// @title Registry
/// @notice Chain registry + group-key registry + role holders, mirroring the
///         Move `sui_bridge::registry` (bridge-spec.md §7). Two roles:
///         - governance: edit chains, register group keys, set threshold.
///         - guardian:   pause/unpause Outbox + Inbox (those read `guardian()`).
///
///         The family is derived from the internal id's top bits (see ChainId),
///         never stored separately, so the two can't disagree.
contract Registry {
    struct ChainEntry {
        bytes nativeIdentifier; // authoritative native id (full EVM chainId, Sui id, …)
        bytes32 outboxAddr;
        bytes32 inboxAddr;
        uint8 finalityKind; // 0 = EVM confirmation depth, 1 = Sui finalized-checkpoint rule
        uint64 finalityValue;
        bool exists;
    }

    struct GroupKey {
        uint8 schemeTag;
        bytes key; // ECDSA: 20-byte group address; Ed25519: 32-byte pubkey
        bool exists;
    }

    address public governance;
    address public guardian;

    mapping(uint32 => ChainEntry) internal chains;
    mapping(uint32 => GroupKey) internal groupKeys;

    // Signer threshold (k-of-n) — governance metadata. Not enforced on-chain: a
    // single aggregated signature verifies against the group key regardless.
    uint16 public thresholdK = 1;
    uint16 public thresholdN = 1;

    event ChainRegistered(uint32 indexed internalId, uint8 family);
    event GroupKeyRegistered(uint32 indexed groupPubkeyId, uint8 schemeTag);
    event ThresholdSet(uint16 k, uint16 n);
    event GuardianSet(address indexed guardian);
    event GovernanceTransferred(address indexed from, address indexed to);

    error NotGovernance();
    error ChainAlreadyRegistered(uint32 internalId);
    error ChainNotRegistered(uint32 internalId);
    error GroupKeyAlreadyRegistered(uint32 groupPubkeyId);
    error GroupKeyNotRegistered(uint32 groupPubkeyId);
    error InvalidThreshold(uint16 k, uint16 n);
    error ZeroAddress();

    modifier onlyGovernance() {
        if (msg.sender != governance) revert NotGovernance();
        _;
    }

    constructor(address governance_, address guardian_) {
        if (governance_ == address(0) || guardian_ == address(0)) revert ZeroAddress();
        governance = governance_;
        guardian = guardian_;
    }

    // --- chain registry ---

    function registerChain(
        uint32 internalId,
        bytes calldata nativeIdentifier,
        bytes32 outboxAddr,
        bytes32 inboxAddr,
        uint8 finalityKind,
        uint64 finalityValue
    ) external onlyGovernance {
        if (chains[internalId].exists) revert ChainAlreadyRegistered(internalId);
        uint8 fam = ChainId.family(internalId);
        if (!ChainId.isValidFamily(fam)) revert ChainId.UnknownFamily(fam);
        chains[internalId] = ChainEntry({
            nativeIdentifier: nativeIdentifier,
            outboxAddr: outboxAddr,
            inboxAddr: inboxAddr,
            finalityKind: finalityKind,
            finalityValue: finalityValue,
            exists: true
        });
        emit ChainRegistered(internalId, fam);
    }

    function isRegistered(uint32 internalId) external view returns (bool) {
        return chains[internalId].exists;
    }

    function family(uint32 internalId) external view returns (uint8) {
        if (!chains[internalId].exists) revert ChainNotRegistered(internalId);
        return ChainId.family(internalId);
    }

    function finality(uint32 internalId) external view returns (uint8 kind, uint64 value) {
        ChainEntry storage e = chains[internalId];
        if (!e.exists) revert ChainNotRegistered(internalId);
        return (e.finalityKind, e.finalityValue);
    }

    // --- group-key registry ---

    function registerGroupKey(uint32 groupPubkeyId, uint8 schemeTag, bytes calldata key)
        external
        onlyGovernance
    {
        if (groupKeys[groupPubkeyId].exists) revert GroupKeyAlreadyRegistered(groupPubkeyId);
        groupKeys[groupPubkeyId] = GroupKey({schemeTag: schemeTag, key: key, exists: true});
        emit GroupKeyRegistered(groupPubkeyId, schemeTag);
    }

    function hasGroupKey(uint32 groupPubkeyId) external view returns (bool) {
        return groupKeys[groupPubkeyId].exists;
    }

    function groupKey(uint32 groupPubkeyId)
        external
        view
        returns (uint8 schemeTag, bytes memory key)
    {
        GroupKey storage gk = groupKeys[groupPubkeyId];
        if (!gk.exists) revert GroupKeyNotRegistered(groupPubkeyId);
        return (gk.schemeTag, gk.key);
    }

    function setSignerThreshold(uint16 k, uint16 n) external onlyGovernance {
        if (k == 0 || k > n) revert InvalidThreshold(k, n);
        thresholdK = k;
        thresholdN = n;
        emit ThresholdSet(k, n);
    }

    // --- roles ---

    function setGuardian(address guardian_) external onlyGovernance {
        if (guardian_ == address(0)) revert ZeroAddress();
        guardian = guardian_;
        emit GuardianSet(guardian_);
    }

    function transferGovernance(address governance_) external onlyGovernance {
        if (governance_ == address(0)) revert ZeroAddress();
        emit GovernanceTransferred(governance, governance_);
        governance = governance_;
    }
}
