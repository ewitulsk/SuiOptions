import type { SolanaSignMessage } from "@yourorg/sui-siws-session";

// Minimal Phantom provider surface we rely on.
interface PhantomProvider {
  isPhantom?: boolean;
  publicKey: { toBytes(): Uint8Array } | null;
  connect(): Promise<{ publicKey: { toBytes(): Uint8Array } }>;
  signMessage(
    message: Uint8Array,
    display?: "utf8" | "hex",
  ): Promise<{ signature: Uint8Array }>;
}

function getProvider(): PhantomProvider | null {
  const p = (window as unknown as { solana?: PhantomProvider }).solana;
  return p?.isPhantom ? p : null;
}

/** Adapts Phantom's Solana provider to the SDK's `SolanaSignMessage`. */
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

/** Connect Phantom and return an adapter + the 32-byte ed25519 pubkey. */
export async function connectPhantom(): Promise<{
  adapter: PhantomAdapter;
  publicKey: Uint8Array;
}> {
  const provider = getProvider();
  if (!provider) {
    throw new Error("Phantom wallet not found — install it from phantom.app");
  }
  const { publicKey } = await provider.connect();
  return { adapter: new PhantomAdapter(provider), publicKey: publicKey.toBytes() };
}
