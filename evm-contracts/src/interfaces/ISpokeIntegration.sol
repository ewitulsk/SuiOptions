// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title ISpokeIntegration — curator integration interface (plan §6)
/// @notice The only path for funds to leave a `SpokeVault` other than user
///         payouts. Funds flow vault → integration only via
///         `SpokeVault.extendTo` (curator-only, active-funds-only, blocked
///         while any payout is queued and while risk_off / stale-heartbeat;
///         the integration must be in the hub-registered set). The return
///         path (`SpokeVault.returnFunds`) is permissionless: anyone can
///         push funds back to the vault.
interface ISpokeIntegration {
    /// @notice Called by the vault after transferring `amount` of `asset`
    ///         to the integration via `extendTo`.
    function onFundsReceived(address asset, uint256 amount) external;

    /// @notice Raw venue state for `StateSync.integration_raw`.
    /// @dev NEVER a valuation — the hub values integrations through its own
    ///      per-integration valuation adapter.
    function rawState() external view returns (bytes memory);
}
