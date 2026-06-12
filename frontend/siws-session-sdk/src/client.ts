// Single source of truth for the Sui RPC client type. In @mysten/sui v2 the
// JSON-RPC client is `SuiJsonRpcClient` (formerly `SuiClient`); dapp-kit's
// `useSuiClient()` returns an instance of it.
export type { SuiJsonRpcClient as SuiRpcClient } from "@mysten/sui/jsonRpc";
export type { SuiTransactionBlockResponse } from "@mysten/sui/jsonRpc";
