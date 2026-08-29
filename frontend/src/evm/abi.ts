// Minimal hand-derived ABIs for the spoke UI, transcribed from
// evm-contracts/src/SpokeVault.sol and TUSDG.sol — only the functions and
// events this screen actually calls. Kept `as const` so viem infers argument
// and return types.

export const spokeVaultAbi = [
  // ── user actions ──────────────────────────────────────────────────────
  {
    type: "function",
    name: "deposit",
    stateMutability: "nonpayable",
    inputs: [
      { name: "assetCode", type: "uint8" },
      { name: "amount", type: "uint256" },
      { name: "tranche", type: "uint8" },
    ],
    outputs: [],
  },
  {
    type: "function",
    name: "reclaim",
    stateMutability: "nonpayable",
    inputs: [{ name: "depositSeq_", type: "uint64" }],
    outputs: [],
  },
  {
    type: "function",
    name: "requestWithdraw",
    stateMutability: "nonpayable",
    inputs: [
      { name: "tranche", type: "uint8" },
      { name: "shares", type: "uint128" },
      { name: "all", type: "bool" },
    ],
    outputs: [],
  },
  {
    type: "function",
    name: "processPayoutQueue",
    stateMutability: "nonpayable",
    inputs: [{ name: "assetCode", type: "uint8" }],
    outputs: [],
  },
  // ── fund state / flags ───────────────────────────────────────────────
  {
    type: "function",
    name: "funds",
    stateMutability: "view",
    inputs: [{ name: "assetCode", type: "uint8" }],
    outputs: [
      { name: "pending", type: "uint128" },
      { name: "active", type: "uint128" },
      { name: "reserved", type: "uint128" },
    ],
  },
  {
    type: "function",
    name: "paused",
    stateMutability: "view",
    inputs: [],
    outputs: [{ type: "bool" }],
  },
  {
    type: "function",
    name: "riskOff",
    stateMutability: "view",
    inputs: [],
    outputs: [{ type: "bool" }],
  },
  {
    type: "function",
    name: "effectiveRiskOff",
    stateMutability: "view",
    inputs: [],
    outputs: [{ type: "bool" }],
  },
  {
    type: "function",
    name: "lastInboundAt",
    stateMutability: "view",
    inputs: [],
    outputs: [{ type: "uint64" }],
  },
  {
    type: "function",
    name: "HEARTBEAT_TIMEOUT",
    stateMutability: "view",
    inputs: [],
    outputs: [{ type: "uint64" }],
  },
  {
    type: "function",
    name: "DEPOSIT_TIMEOUT",
    stateMutability: "view",
    inputs: [],
    outputs: [{ type: "uint64" }],
  },
  {
    type: "function",
    name: "feePot",
    stateMutability: "view",
    inputs: [],
    outputs: [{ type: "uint256" }],
  },
  // ── deposits ─────────────────────────────────────────────────────────
  {
    type: "function",
    name: "deposits",
    stateMutability: "view",
    inputs: [{ name: "depositSeq", type: "uint64" }],
    outputs: [
      { name: "depositor", type: "address" },
      { name: "asset", type: "uint8" },
      { name: "tranche", type: "uint8" },
      { name: "status", type: "uint8" }, // DepositStatus enum
      { name: "amount", type: "uint128" },
      { name: "ts", type: "uint64" },
    ],
  },
  // ── withdrawals / share mirror ───────────────────────────────────────
  {
    type: "function",
    name: "shareMirror",
    stateMutability: "view",
    inputs: [
      { name: "user", type: "address" },
      { name: "tranche", type: "uint8" },
    ],
    outputs: [{ type: "uint256" }],
  },
  {
    type: "function",
    name: "inFlightRequest",
    stateMutability: "view",
    inputs: [
      { name: "user", type: "address" },
      { name: "tranche", type: "uint8" },
    ],
    outputs: [{ type: "uint64" }],
  },
  {
    type: "function",
    name: "withdrawals",
    stateMutability: "view",
    inputs: [{ name: "requestSeq", type: "uint64" }],
    outputs: [
      { name: "user", type: "address" },
      { name: "tranche", type: "uint8" },
      { name: "all", type: "bool" },
      { name: "open", type: "bool" },
      { name: "shares", type: "uint128" },
    ],
  },
  // ── payout queue ─────────────────────────────────────────────────────
  {
    type: "function",
    name: "queueLength",
    stateMutability: "view",
    inputs: [{ name: "assetCode", type: "uint8" }],
    outputs: [{ type: "uint256" }],
  },
  {
    type: "function",
    name: "payoutHead",
    stateMutability: "view",
    inputs: [{ name: "assetCode", type: "uint8" }],
    outputs: [{ type: "uint256" }],
  },
  {
    type: "function",
    name: "payoutTail",
    stateMutability: "view",
    inputs: [{ name: "assetCode", type: "uint8" }],
    outputs: [{ type: "uint256" }],
  },
  {
    type: "function",
    name: "payoutQueue",
    stateMutability: "view",
    inputs: [
      { name: "assetCode", type: "uint8" },
      { name: "index", type: "uint256" },
    ],
    outputs: [
      { name: "user", type: "address" },
      { name: "requestSeq", type: "uint64" },
      { name: "owed", type: "uint128" },
      { name: "reservedAmt", type: "uint128" },
    ],
  },
  // ── events ───────────────────────────────────────────────────────────
  {
    type: "event",
    name: "Deposited",
    inputs: [
      { name: "depositSeq", type: "uint64", indexed: true },
      { name: "depositor", type: "address", indexed: true },
      { name: "asset", type: "uint8", indexed: false },
      { name: "amount", type: "uint128", indexed: false },
      { name: "tranche", type: "uint8", indexed: false },
    ],
  },
  {
    type: "event",
    name: "DepositAcked",
    inputs: [
      { name: "depositSeq", type: "uint64", indexed: true },
      { name: "shares", type: "uint128", indexed: false },
    ],
  },
  {
    type: "event",
    name: "DepositRejected",
    inputs: [{ name: "depositSeq", type: "uint64", indexed: true }],
  },
  {
    type: "event",
    name: "DepositReclaimed",
    inputs: [{ name: "depositSeq", type: "uint64", indexed: true }],
  },
  {
    type: "event",
    name: "WithdrawRequested",
    inputs: [
      { name: "requestSeq", type: "uint64", indexed: true },
      { name: "user", type: "address", indexed: true },
      { name: "tranche", type: "uint8", indexed: false },
      { name: "shares", type: "uint128", indexed: false },
      { name: "all", type: "bool", indexed: false },
    ],
  },
  {
    type: "event",
    name: "PayoutQueued",
    inputs: [
      { name: "requestSeq", type: "uint64", indexed: true },
      { name: "user", type: "address", indexed: true },
      { name: "asset", type: "uint8", indexed: false },
      { name: "owed", type: "uint128", indexed: false },
    ],
  },
  {
    type: "event",
    name: "PayoutPaid",
    inputs: [
      { name: "requestSeq", type: "uint64", indexed: true },
      { name: "user", type: "address", indexed: true },
      { name: "asset", type: "uint8", indexed: false },
      { name: "amount", type: "uint128", indexed: false },
    ],
  },
] as const;

/** `DepositStatus` enum from SpokeVault.sol, by uint8 value. */
export const DEPOSIT_STATUS = {
  None: 0,
  Pending: 1,
  Acked: 2,
  Refunded: 3,
  Reclaimed: 4,
} as const;

// TUSDG (testnet faucet mint) + the ERC-20 surface the deposit flow needs.
// `mintToSender` only exists on TUSDG; the mainnet-set never calls it.
export const usdgAbi = [
  {
    type: "function",
    name: "balanceOf",
    stateMutability: "view",
    inputs: [{ name: "account", type: "address" }],
    outputs: [{ type: "uint256" }],
  },
  {
    type: "function",
    name: "allowance",
    stateMutability: "view",
    inputs: [
      { name: "owner", type: "address" },
      { name: "spender", type: "address" },
    ],
    outputs: [{ type: "uint256" }],
  },
  {
    type: "function",
    name: "approve",
    stateMutability: "nonpayable",
    inputs: [
      { name: "spender", type: "address" },
      { name: "amount", type: "uint256" },
    ],
    outputs: [{ type: "bool" }],
  },
  {
    type: "function",
    name: "mint",
    stateMutability: "nonpayable",
    inputs: [
      { name: "to", type: "address" },
      { name: "amount", type: "uint256" },
    ],
    outputs: [],
  },
  {
    type: "function",
    name: "mintToSender",
    stateMutability: "nonpayable",
    inputs: [{ name: "amount", type: "uint256" }],
    outputs: [],
  },
] as const;
