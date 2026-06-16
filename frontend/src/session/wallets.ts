// Root-wallet adapters for session sign-in (siws_session). Mirrors the
// adapters in `session-tokens/demo-frontend`; the SDK only needs the
// message-signing surface of each wallet.

import type {
  EthereumSignMessage,
  SolanaSignMessage,
} from "@yourorg/sui-siws-session";

import { WALLETCONNECT_PROJECT_ID } from "../config";

// --- Phantom (Solana / SIWS) ---

interface PhantomProvider {
  isPhantom?: boolean;
  publicKey: { toBytes(): Uint8Array } | null;
  connect(): Promise<{ publicKey: { toBytes(): Uint8Array } }>;
  signMessage(
    message: Uint8Array,
    display?: "utf8" | "hex",
  ): Promise<{ signature: Uint8Array }>;
}

function phantomProvider(): PhantomProvider | null {
  const p = (window as unknown as { solana?: PhantomProvider }).solana;
  return p?.isPhantom ? p : null;
}

export function hasPhantom(): boolean {
  return phantomProvider() !== null;
}

export class PhantomAdapter implements SolanaSignMessage {
  constructor(private readonly provider: PhantomProvider) {}

  getPublicKey(): Uint8Array {
    if (!this.provider.publicKey) throw new Error("Phantom not connected");
    return this.provider.publicKey.toBytes();
  }

  async signMessage(message: Uint8Array): Promise<{ signature: Uint8Array }> {
    const { signature } = await this.provider.signMessage(message, "utf8");
    return {
      signature:
        signature instanceof Uint8Array ? signature : Uint8Array.from(signature),
    };
  }
}

export async function connectPhantom(): Promise<PhantomAdapter> {
  const provider = phantomProvider();
  if (!provider) {
    throw new Error("Phantom wallet not found — install it from phantom.app");
  }
  await provider.connect();
  return new PhantomAdapter(provider);
}

// --- MetaMask (Ethereum / SIWE) ---

interface Eip1193 {
  request(args: { method: string; params?: unknown[] }): Promise<unknown>;
  isMetaMask?: boolean;
}

function ethereumProvider(): Eip1193 | null {
  const e = (window as unknown as { ethereum?: Eip1193 }).ethereum;
  return e ?? null;
}

export function hasMetaMask(): boolean {
  return ethereumProvider() !== null;
}

function utf8ToHex(s: string): string {
  let h = "0x";
  for (const b of new TextEncoder().encode(s)) h += b.toString(16).padStart(2, "0");
  return h;
}

export class MetaMaskAdapter implements EthereumSignMessage {
  constructor(
    private readonly provider: Eip1193,
    private readonly address: string,
  ) {}

  getAddress(): string {
    return this.address;
  }

  async personalSign(message: string): Promise<string> {
    // personal_sign params: [hex-encoded message, address]. MetaMask renders
    // the decoded UTF-8 (the EIP-4361 screen).
    return (await this.provider.request({
      method: "personal_sign",
      params: [utf8ToHex(message), this.address],
    })) as string;
  }
}

export async function connectMetaMask(): Promise<{
  adapter: MetaMaskAdapter;
  chainId: number;
}> {
  const provider = ethereumProvider();
  if (!provider) {
    throw new Error("MetaMask not found — install it from metamask.io");
  }
  const accounts = (await provider.request({
    method: "eth_requestAccounts",
  })) as string[];
  const address = accounts[0];
  const chainHex = (await provider.request({ method: "eth_chainId" })) as string;
  return {
    adapter: new MetaMaskAdapter(provider, address),
    chainId: parseInt(chainHex, 16),
  };
}

// --- WalletConnect (Ethereum / SIWE) ---
//
// WalletConnect's "Sign In With Ethereum" is the same EIP-4361 path MetaMask
// uses — an EIP-1193 `personal_sign` over the canonical message — so it reuses
// the SDK's `EthereumSignMessage` surface and `createSessionEth`. The only
// difference is the transport: a relay + QR/deeplink modal instead of an
// injected provider, so any mobile/desktop wallet can be the root.

// Minimal slice of `@walletconnect/ethereum-provider`'s EIP-1193 provider.
interface WalletConnectProvider {
  request(args: { method: string; params?: unknown[] }): Promise<unknown>;
  connect(): Promise<void>;
  accounts: string[];
  chainId: number;
}

// Initialized lazily and once: the heavy provider bundle is dynamically
// imported only when a user actually picks WalletConnect, and the relay
// session it holds is reused across sign-in / revoke.
let wcProvider: WalletConnectProvider | null = null;

export function hasWalletConnect(): boolean {
  return Boolean(WALLETCONNECT_PROJECT_ID);
}

async function walletConnectProvider(): Promise<WalletConnectProvider> {
  if (wcProvider) return wcProvider;
  if (!WALLETCONNECT_PROJECT_ID) {
    throw new Error(
      "WalletConnect is not configured — set VITE_WALLETCONNECT_PROJECT_ID",
    );
  }
  const { EthereumProvider } = await import("@walletconnect/ethereum-provider");
  wcProvider = (await EthereumProvider.init({
    projectId: WALLETCONNECT_PROJECT_ID,
    // Ethereum mainnet is enough to carry the SIWE chain id; `optionalChains`
    // keeps smart-contract wallets that don't support eth_chainId connectable.
    optionalChains: [1],
    showQrModal: true,
    metadata: {
      name: "Tideline",
      description: "Tideline options protocol",
      url: window.location.origin,
      icons: [`${window.location.origin}/favicon.ico`],
    },
  })) as unknown as WalletConnectProvider;
  return wcProvider;
}

export class WalletConnectAdapter implements EthereumSignMessage {
  constructor(
    private readonly provider: WalletConnectProvider,
    private readonly address: string,
  ) {}

  getAddress(): string {
    return this.address;
  }

  async personalSign(message: string): Promise<string> {
    // Same params as MetaMask: [hex-encoded message, address]. The connected
    // wallet renders the decoded UTF-8 (the EIP-4361 screen).
    return (await this.provider.request({
      method: "personal_sign",
      params: [utf8ToHex(message), this.address],
    })) as string;
  }
}

export async function connectWalletConnect(): Promise<{
  adapter: WalletConnectAdapter;
  chainId: number;
}> {
  const provider = await walletConnectProvider();
  // A persisted relay session reconnects silently; otherwise open the modal.
  if (provider.accounts.length === 0) {
    await provider.connect();
  }
  const address = provider.accounts[0];
  if (!address) {
    throw new Error("WalletConnect returned no account");
  }
  return { adapter: new WalletConnectAdapter(provider, address), chainId: provider.chainId };
}
