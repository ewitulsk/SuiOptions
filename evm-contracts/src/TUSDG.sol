// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// @title TUSDG — faucet-mintable test USDG (testnet only)
/// @notice 6-decimal open-mint mock of USDG for the Robinhood spoke,
///         mirroring the Sui `test-tokens` faucet pattern (tusdc.move):
///         anyone may mint any amount. Deployed only in the testnet config
///         set — mainnet uses real USDG.
contract TUSDG is ERC20 {
    constructor() ERC20("Test USDG", "TUSDG") {}

    function decimals() public pure override returns (uint8) {
        return 6;
    }

    /// @notice Open faucet mint to any recipient.
    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    /// @notice Open faucet mint to the caller.
    function mintToSender(uint256 amount) external {
        _mint(msg.sender, amount);
    }
}
