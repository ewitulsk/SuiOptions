// design-sync stub for @walletconnect/ethereum-provider.
//
// The real package transitively pulls in @reown/appkit, viem, and the rest of
// the WalletConnect stack (~9 MB) — far over claude.ai/design's 5 MB bundle
// cap. It is reached only through `session/wallets.ts`'s dynamic
// `import("@walletconnect/ethereum-provider")`, which runs solely when a user
// clicks "connect WalletConnect" — never during a static design preview.
//
// Aliased in via .design-sync/tsconfig.dssync.json (cfg.tsconfig) so the
// converter's tsconfigPathsPlugin resolves the import here instead of bundling
// the real stack. Components render identically; only the live WC connection
// path is inert in previews.
export class EthereumProvider {
  static async init(): Promise<never> {
    throw new Error("WalletConnect is unavailable in the design-sync preview bundle");
  }
}
export default { EthereumProvider };
