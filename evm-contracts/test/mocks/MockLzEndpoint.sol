// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {
    ILayerZeroEndpointV2,
    ILayerZeroReceiver,
    MessagingParams,
    MessagingFee,
    MessagingReceipt,
    Origin
} from "../../src/vendor/ILayerZeroEndpointV2.sol";

/// @notice Test double for the LayerZero V2 endpoint: records sends,
///         charges a settable fee, and can deliver messages into a
///         receiver with an arbitrary origin.
contract MockLzEndpoint is ILayerZeroEndpointV2 {
    uint256 public fee;
    uint64 public nonce;

    bytes public lastMessage;
    uint32 public lastDstEid;
    bytes32 public lastReceiver;
    uint256 public lastValue;
    uint256 public sendCount;

    function setFee(uint256 fee_) external {
        fee = fee_;
    }

    function send(MessagingParams calldata params, address)
        external
        payable
        returns (MessagingReceipt memory r)
    {
        require(msg.value >= fee, "MockLz: underpaid");
        lastMessage = params.message;
        lastDstEid = params.dstEid;
        lastReceiver = params.receiver;
        lastValue = msg.value;
        sendCount += 1;
        r.guid = keccak256(params.message);
        r.nonce = ++nonce;
        r.fee = MessagingFee(msg.value, 0);
    }

    function quote(MessagingParams calldata, address) external view returns (MessagingFee memory) {
        return MessagingFee(fee, 0);
    }

    function deliver(address target, uint32 srcEid, bytes32 sender, bytes calldata message)
        external
    {
        ILayerZeroReceiver(target).lzReceive(
            Origin(srcEid, sender, ++nonce), bytes32(0), message, address(0), ""
        );
    }
}
