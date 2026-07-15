import type { EthereumSignMessage } from "@yourorg/sui-siws-session";

interface Eip1193 {
  request(args: { method: string; params?: unknown[] }): Promise<unknown>;
  isMetaMask?: boolean;
}

function getProvider(): Eip1193 | null {
  const e = (window as unknown as { ethereum?: Eip1193 }).ethereum;
  return e ?? null;
}

function utf8ToHex(s: string): string {
  let h = "0x";
  for (const b of new TextEncoder().encode(s)) h += b.toString(16).padStart(2, "0");
  return h;
}

/** Adapts a MetaMask (EIP-1193) provider to the SDK's `EthereumSignMessage`. */
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
  address: string;
  chainId: number;
}> {
  const provider = getProvider();
  if (!provider) {
    throw new Error("MetaMask not found — install it from metamask.io");
  }
  const accounts = (await provider.request({ method: "eth_requestAccounts" })) as string[];
  const address = accounts[0];
  const chainHex = (await provider.request({ method: "eth_chainId" })) as string;
  return { adapter: new MetaMaskAdapter(provider, address), address, chainId: parseInt(chainHex, 16) };
}
