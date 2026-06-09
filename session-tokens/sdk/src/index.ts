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
  serializeSessionMessage,
  serializeRevokeMessage,
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
  LocalSponsorClient,
  HttpSponsorClient,
  type SponsorClient,
  type LocalSponsorOptions,
  type HttpSponsorOptions,
  type SponsoredReservation,
} from "./sponsor.js";

export {
  fetchGeneration,
  readAccountBalance,
  readGeneration,
  readSpent,
  resolveAccountId,
} from "./reads.js";

export type {
  Network,
  SolanaSignMessage,
  EthereumSignMessage,
  RootScheme,
  SessionConfig,
  CreateSessionOptions,
  SessionStatus,
} from "./types.js";
