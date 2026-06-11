// Root-wallet adapters for session sign-in (siws_session). Mirrors the
// adapters in `session-tokens/demo-frontend`; the SDK only needs the
// message-signing surface of each wallet.

import type {
  EthereumSignMessage,
  SolanaSignMessage,
} from "@yourorg/sui-siws-session";

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
