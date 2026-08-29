// Injected-EVM-wallet account hook (MetaMask-style), viem only. The header's
// wallet is Sui; the spoke screen carries its own connect button, mirroring
// how Bridge.tsx handles Phantom for Solana.

import { useCallback, useEffect, useState } from "react";
import type { Address } from "viem";

import { SPOKE_CONFIG } from "../config";
import {
  ensureSpokeChain,
  getSpokeWalletClient,
  hasInjectedProvider,
} from "./client";

export type EvmAccount = {
  /** An injected provider exists (else render an install hint). */
  hasProvider: boolean;
  /** Connected account, null before connect / after disconnect. */
  address: Address | null;
  /** The wallet's current chain id (not necessarily the spoke chain). */
  chainId: number | null;
  /** Wallet is connected AND sitting on the configured spoke chain. */
  onSpokeChain: boolean;
  /** Prompt to connect and switch/add the spoke chain. */
  connect: () => Promise<void>;
};

export function useEvmAccount(): EvmAccount {
  const [address, setAddress] = useState<Address | null>(null);
  const [chainId, setChainId] = useState<number | null>(null);
  const hasProvider = hasInjectedProvider();

  // Silently resync already-authorized accounts on mount, then track the
  // provider's own account/chain switches.
  useEffect(() => {
    if (!hasProvider) return;
    const provider = window.ethereum!;

    provider
      .request({ method: "eth_accounts" })
      .then((accounts) => setAddress((accounts as Address[])[0] ?? null))
      .catch(() => {});
    provider
      .request({ method: "eth_chainId" })
      .then((id) => setChainId(Number(id)))
      .catch(() => {});

    const onAccounts = (accounts: unknown) =>
      setAddress(((accounts as Address[])[0] as Address | undefined) ?? null);
    const onChain = (id: unknown) => setChainId(Number(id));
    provider.on("accountsChanged", onAccounts);
    provider.on("chainChanged", onChain);
    return () => {
      provider.removeListener("accountsChanged", onAccounts);
      provider.removeListener("chainChanged", onChain);
    };
  }, [hasProvider]);

  const connect = useCallback(async () => {
    const wallet = getSpokeWalletClient();
    const [account] = await wallet.requestAddresses();
    setAddress(account ?? null);
    await ensureSpokeChain(wallet);
    setChainId(await wallet.getChainId());
  }, []);

  return {
    hasProvider,
    address,
    chainId,
    onSpokeChain:
      address !== null && SPOKE_CONFIG !== undefined && chainId === SPOKE_CONFIG.chainId,
    connect,
  };
}
