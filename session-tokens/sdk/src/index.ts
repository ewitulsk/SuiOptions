// @yourorg/sui-siws-session — public surface.

export {
  createSession,
  createSessionEth,
  restoreSession,
  SessionHandle,
  SessionExpiredError,
  type CreateSessionEthOptions,
} from "./session.js";

export {
  canonicalCoinType,
  encodeLimits,
  serializeSessionMessage,
  serializeRevokeMessage,
  type SpendLimit,
  type SessionMessageFields,
  type RevokeMessageFields,
} from "./message.js";

export {
  buildSiweSessionMessage,
  buildSiweRevokeMessage,
  ethAddressToBytes,
  ethSignatureToSui,
  type SiweSessionFields,
  type SiweRevokeFields,
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
