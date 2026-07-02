// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title WrappedToken
/// @notice Minimal ERC-20 whose mint/burn authority is held by a single owner —
///         the foreign-chain Locker (bridge-spec.md §3.1). Deploy with the
///         deployer as owner, then `transferOwnership` to the Locker so only the
///         Locker can mint on delivery and burn on bridge-out.
contract WrappedToken {
    string public name;
    string public symbol;
    uint8 public immutable decimals;

    address public owner;
    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    event OwnershipTransferred(address indexed from, address indexed to);

    error NotOwner();
    error InsufficientBalance();
    error InsufficientAllowance();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(string memory name_, string memory symbol_, uint8 decimals_, address owner_) {
        name = name_;
        symbol = symbol_;
        decimals = decimals_;
        owner = owner_;
        emit OwnershipTransferred(address(0), owner_);
    }

    /// @notice Hand mint/burn authority to the Locker after it is deployed.
    function transferOwnership(address newOwner) external onlyOwner {
        emit OwnershipTransferred(owner, newOwner);
        owner = newOwner;
    }

    function mint(address to, uint256 amount) external onlyOwner {
        totalSupply += amount;
        balanceOf[to] += amount;
        emit Transfer(address(0), to, amount);
    }

    /// @notice Burn from `from`'s balance. The Locker only burns the caller's own
    ///         balance (it passes `msg.sender` of `bridgeOut`).
    function burn(address from, uint256 amount) external onlyOwner {
        uint256 bal = balanceOf[from];
        if (bal < amount) revert InsufficientBalance();
        balanceOf[from] = bal - amount;
        totalSupply -= amount;
        emit Transfer(from, address(0), amount);
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        if (allowed != type(uint256).max) {
            if (allowed < amount) revert InsufficientAllowance();
            allowance[from][msg.sender] = allowed - amount;
        }
        _transfer(from, to, amount);
        return true;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function _transfer(address from, address to, uint256 amount) internal {
        uint256 bal = balanceOf[from];
        if (bal < amount) revert InsufficientBalance();
        balanceOf[from] = bal - amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }
}
