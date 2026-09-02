// viem clients for the EVM spoke chain (docs/multichain-vault-plan.md §4–§5).
//
// Deliberately light: a MetaMask-style injected provider (`window.ethereum`)
// via `createWalletClient(custom(...))` for writes, and an app-owned
// `createPublicClient(http(rpcUrl))` for all reads — no wagmi/rainbowkit.
// The chain itself comes from `SPOKE_CONFIG`, so testnet → mainnet is the
// same config flip as the rest of the app.

import {
  createPublicClient,
  createWalletClient,
  custom,
  defineChain,
  http,
  type Chain,
  type EIP1193Provider,
  type PublicClient,
  type WalletClient,
} from "viem";

import { SPOKE_CONFIG, type SpokeConfig } from "../config";

declare global {
  interface Window {
    ethereum?: EIP1193Provider;
  }
}

export function hasInjectedProvider(): boolean {
  return typeof window !== "undefined" && window.ethereum !== undefined;
}

/** viem `Chain` built from the spoke config (native currency assumed ETH —
 * every candidate spoke chain is an Ethereum L2). */
export function spokeChain(cfg: SpokeConfig): Chain {
  return defineChain({
    id: cfg.chainId,
    name: cfg.chainName,
    nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
    rpcUrls: { default: { http: [cfg.rpcUrl] } },
    blockExplorers: { default: { name: "Explorer", url: cfg.explorerUrl } },
  });
}

// One public client per app run — reads always go through our configured RPC,
// never the wallet's (which may sit on another chain entirely).
let publicClient: PublicClient | undefined;

export function getSpokePublicClient(): PublicClient {
  if (!SPOKE_CONFIG) throw new Error("No spoke deployment on this network");
  if (!publicClient) {
    publicClient = createPublicClient({
      chain: spokeChain(SPOKE_CONFIG),
      transport: http(SPOKE_CONFIG.rpcUrl),
    });
  }
  return publicClient;
}

export function getSpokeWalletClient(): WalletClient {
  if (!SPOKE_CONFIG) throw new Error("No spoke deployment on this network");
  const provider = window.ethereum;
  if (!provider) {
    throw new Error("No EVM wallet found — install MetaMask (or similar)");
  }
  return createWalletClient({
    chain: spokeChain(SPOKE_CONFIG),
    transport: custom(provider),
  });
}

/**
 * Make the wallet sit on the configured spoke chain: try
 * `wallet_switchEthereumChain`, and when the wallet doesn't know the chain,
 * `wallet_addEthereumChain` it from our config and switch again.
 */
export async function ensureSpokeChain(wallet: WalletClient): Promise<void> {
  if (!SPOKE_CONFIG) throw new Error("No spoke deployment on this network");
  const current = await wallet.getChainId();
  if (current === SPOKE_CONFIG.chainId) return;
  try {
    await wallet.switchChain({ id: SPOKE_CONFIG.chainId });
  } catch {
    // Unrecognized chain (EIP-3085 4902) or wallets that garble the code:
    // add from config, then switch.
    await wallet.addChain({ chain: spokeChain(SPOKE_CONFIG) });
    await wallet.switchChain({ id: SPOKE_CONFIG.chainId });
  }
}
