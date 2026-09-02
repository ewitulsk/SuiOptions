// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IMessageEndpoint} from "../interfaces/IMessageEndpoint.sol";
import {ISpokeVault} from "../interfaces/ISpokeVault.sol";
import {Wire} from "../lib/Wire.sol";
import {
    ILayerZeroEndpointV2,
    ILayerZeroReceiver,
    MessagingParams,
    MessagingFee,
    Origin
} from "../vendor/ILayerZeroEndpointV2.sol";

/// @title LayerZeroEndpoint — primary transport endpoint (plan §2.3)
/// @notice OApp-style adapter between the SpokeVault and the LayerZero V2
///         endpoint on this chain. The hub peer (EID + OApp address) is
///         pinned at construction; switching peers means deploying a new
///         candidate and activating it via hub ConfigSync. Verification
///         rides the lane's configured DVN set — this contract only checks
///         that delivery comes from the real LayerZero endpoint and from
///         the pinned hub peer.
contract LayerZeroEndpoint is IMessageEndpoint, ILayerZeroReceiver {
    ISpokeVault public immutable VAULT;
    ILayerZeroEndpointV2 public immutable LZ_ENDPOINT;
    uint32 public immutable HUB_EID;
    bytes32 public immutable HUB_PEER;

    /// @notice LayerZero executor options attached to every send (e.g.
    ///         lzReceive gas); fixed at construction.
    bytes public sendOptions;

    error NotVault(address caller);
    error NotLzEndpoint(address caller);
    error BadPeer(uint32 srcEid, bytes32 sender);

    constructor(
        address vault,
        address lzEndpoint,
        uint32 hubEid,
        bytes32 hubPeer,
        bytes memory sendOptions_
    ) {
        VAULT = ISpokeVault(vault);
        LZ_ENDPOINT = ILayerZeroEndpointV2(lzEndpoint);
        HUB_EID = hubEid;
        HUB_PEER = hubPeer;
        sendOptions = sendOptions_;
    }

    /// @inheritdoc IMessageEndpoint
    function send(bytes calldata message) external payable {
        if (msg.sender != address(VAULT)) revert NotVault(msg.sender);
        LZ_ENDPOINT.send{value: msg.value}(_params(message), address(VAULT));
    }

    /// @inheritdoc IMessageEndpoint
    function quoteFee(bytes calldata message) external view returns (uint256) {
        MessagingFee memory fee = LZ_ENDPOINT.quote(_params(message), address(this));
        return fee.nativeFee;
    }

    /// @notice LayerZero delivery: only the endpoint may call, and only
    ///         with the pinned hub origin.
    function lzReceive(
        Origin calldata origin,
        bytes32, /* guid */
        bytes calldata message,
        address, /* executor */
        bytes calldata /* extraData */
    ) external payable {
        if (msg.sender != address(LZ_ENDPOINT)) revert NotLzEndpoint(msg.sender);
        if (origin.srcEid != HUB_EID || origin.sender != HUB_PEER) {
            revert BadPeer(origin.srcEid, origin.sender);
        }
        VAULT.handleMessage(message[:Wire.ENVELOPE_LEN], message[Wire.ENVELOPE_LEN:]);
    }

    function _params(bytes calldata message) private view returns (MessagingParams memory) {
        return MessagingParams({
            dstEid: HUB_EID,
            receiver: HUB_PEER,
            message: message,
            options: sendOptions,
            payInLzToken: false
        });
    }
}
