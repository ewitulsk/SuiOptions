// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Client, IRouterClient, IAny2EVMMessageReceiver} from "../../src/vendor/CCIP.sol";

/// @notice Test double for the CCIP router: records sends, charges a
///         settable fee, and can deliver messages into a receiver with an
///         arbitrary source.
contract MockCcipRouter is IRouterClient {
    uint256 public fee;
    uint256 public messageCounter;

    bytes public lastData;
    uint64 public lastSelector;
    bytes public lastReceiver;
    uint256 public lastValue;
    uint256 public sendCount;

    function setFee(uint256 fee_) external {
        fee = fee_;
    }

    function ccipSend(uint64 destinationChainSelector, Client.EVM2AnyMessage calldata message)
        external
        payable
        returns (bytes32)
    {
        require(msg.value >= fee, "MockCcip: underpaid");
        lastData = message.data;
        lastSelector = destinationChainSelector;
        lastReceiver = message.receiver;
        lastValue = msg.value;
        sendCount += 1;
        return bytes32(++messageCounter);
    }

    function getFee(uint64, Client.EVM2AnyMessage calldata) external view returns (uint256) {
        return fee;
    }

    function deliver(address target, uint64 sourceChainSelector, bytes calldata sender, bytes calldata data)
        external
    {
        IAny2EVMMessageReceiver(target).ccipReceive(
            Client.Any2EVMMessage({
                messageId: bytes32(++messageCounter),
                sourceChainSelector: sourceChainSelector,
                sender: sender,
                data: data,
                destTokenAmounts: new Client.EVMTokenAmount[](0)
            })
        );
    }
}
