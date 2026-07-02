// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {Outbox} from "../src/Outbox.sol";
import {Locker} from "../src/Locker.sol";
import {WrappedToken} from "../src/WrappedToken.sol";

/// @notice Deploy one per-asset Locker (bridge-spec.md §3) and wire its peer.
///
/// Env inputs:
///   PRIVATE_KEY            deployer
///   LOCKER_OUTBOX          L1 Outbox address (this chain)
///   LOCKER_INBOX           L1 Inbox address (this chain)
///   LOCKER_ASSET_ID        bytes32 asset id (shared across the route)
///   LOCKER_MODE            0 = Escrow (home), 1 = Mint (foreign)
///   LOCKER_PEER_CHAIN_ID   internal id of the sibling chain
///   LOCKER_PEER            bytes32 sibling Locker identity (e.g. Sui object id)
///   LOCKER_ADMIN           (optional) final admin; defaults to deployer
///   -- Escrow mode --
///   LOCKER_TOKEN           existing ERC-20 to escrow
///   LOCKER_LOCAL_DECIMALS  that token's decimals
///   -- Mint mode --
///   WRAPPED_NAME / WRAPPED_SYMBOL / WRAPPED_DECIMALS  for the new WrappedToken
contract DeployLocker is Script {
    function run() external {
        uint256 pk = vm.envUint("PRIVATE_KEY");
        Locker.Mode mode = Locker.Mode(vm.envUint("LOCKER_MODE"));

        vm.startBroadcast(pk);
        (address token, uint8 decimals) = _prepareToken(mode, vm.addr(pk));
        // Deployer is admin during wiring so it can set the peer.
        Locker locker = new Locker(
            Outbox(vm.envAddress("LOCKER_OUTBOX")),
            vm.envAddress("LOCKER_INBOX"),
            vm.envBytes32("LOCKER_ASSET_ID"),
            token,
            mode,
            decimals,
            vm.addr(pk)
        );
        if (mode == Locker.Mode.Mint) {
            WrappedToken(token).transferOwnership(address(locker));
        }
        locker.setPeer(uint32(vm.envUint("LOCKER_PEER_CHAIN_ID")), vm.envBytes32("LOCKER_PEER"));
        _maybeTransferAdmin(locker, vm.addr(pk));
        vm.stopBroadcast();

        console.log("Locker ", address(locker));
        console.log("token  ", token);
    }

    /// New WrappedToken (Mint) or the configured existing ERC-20 (Escrow).
    function _prepareToken(Locker.Mode mode, address deployer)
        internal
        returns (address token, uint8 decimals)
    {
        if (mode == Locker.Mode.Mint) {
            WrappedToken w = new WrappedToken(
                vm.envString("WRAPPED_NAME"),
                vm.envString("WRAPPED_SYMBOL"),
                uint8(vm.envUint("WRAPPED_DECIMALS")),
                deployer
            );
            return (address(w), w.decimals());
        }
        return (vm.envAddress("LOCKER_TOKEN"), uint8(vm.envUint("LOCKER_LOCAL_DECIMALS")));
    }

    function _maybeTransferAdmin(Locker locker, address deployer) internal {
        address admin = vm.envOr("LOCKER_ADMIN", deployer);
        if (admin != deployer) {
            locker.transferAdmin(admin);
        }
    }
}
