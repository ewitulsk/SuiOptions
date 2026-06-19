// @yourorg/sui-siws-session — public surface.

export {
  createSession,
  createSessionEth,
  restoreSession,
  SessionHandle,
  SessionExpiredError,
  type CreateSessionEthOptions,
  type WithdrawAuth,
  type SignWithdrawParams,
} from "./session.js";

export {
  canonicalCoinType,
  encodeLimits,
  serializeSessionMessage,
  serializeRevokeMessage,
  serializeWithdrawMessage,
  type SpendLimit,
  type SessionMessageFields,
  type RevokeMessageFields,
  type WithdrawMessageFields,
} from "./message.js";

export {
  buildSiweSessionMessage,
  buildSiweRevokeMessage,
  buildSiweWithdrawMessage,
  ethAddressToBytes,
  ethSignatureToSui,
  type SiweSessionFields,
  type SiweRevokeFields,
  type SiweWithdrawFields,
} from "./siwe.js";

export {
  makeSessionSigner,
  sessionSignerFromCryptoKey,
  webCryptoEd25519Available,
  type SessionSigner,
} from "./signer.js";

export {
  GasStationSponsorClient,
  LocalSponsorClient,
  SponsorUnavailableError,
  suiOptionsGasStation,
  type GasStationAdapter,
  type GasStationHealth,
  type SponsorClient,
  type LocalSponsorOptions,
  type SponsoredReservation,
  type SuiOptionsGasStationOptions,
} from "./sponsor.js";

export {
  fetchGeneration,
  readAccountBalance,
  readAccountBalances,
  readGeneration,
  readSpent,
  resolveAccountId,
} from "./reads.js";

export { clearSession } from "./store.js";

export type {
  Network,
  SolanaSignMessage,
  EthereumSignMessage,
  RootScheme,
  SessionConfig,
  CreateSessionOptions,
  SessionStatus,
  SpendLimitStatus,
} from "./types.js";
