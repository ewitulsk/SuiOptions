// Lazy loader for the frost-wasm participant (the curator half of the
// FROST 2-of-2 ceremony — see frontend/frost-wasm/README.md).
//
// The compiled pkg is a COMMITTED artifact: Vercel builds never run a Rust
// toolchain. Vite bundles the .js glue statically (dynamic import) and
// serves the .wasm as an asset via `new URL(..., import.meta.url)`; the
// module is only fetched when a ceremony actually starts.

import type * as FrostWasm from "./pkg/frost_wasm";

export type Frost = typeof FrostWasm;

let loaded: Promise<Frost> | null = null;

/** Load + initialize the wasm module (idempotent). */
export function loadFrost(): Promise<Frost> {
  if (!loaded) {
    loaded = (async () => {
      const mod = await import("./pkg/frost_wasm");
      await mod.default(
        new URL("./pkg/frost_wasm_bg.wasm", import.meta.url),
      );
      return mod;
    })();
  }
  return loaded;
}

/** Assemble the full Sui serialized ed25519 signature for a group-signed
 * digest: base64( 0x00 scheme flag || 64-byte signature || 32-byte pubkey ).
 * What Sui transaction auth and Bluefin's request verifier both expect. */
export function suiSerializedSignature(
  signatureHex: string,
  groupPublicKeyHex: string,
): string {
  const sig = hexToBytes(signatureHex);
  const pk = hexToBytes(groupPublicKeyHex);
  if (sig.length !== 64 || pk.length !== 32) {
    throw new Error("malformed signature/pubkey for Sui serialization");
  }
  const out = new Uint8Array(1 + 64 + 32);
  out[0] = 0x00; // ed25519 scheme flag
  out.set(sig, 1);
  out.set(pk, 65);
  return bytesToBase64(out);
}

export function hexToBytes(hex: string): Uint8Array {
  const clean = hex.startsWith("0x") ? hex.slice(2) : hex;
  if (clean.length % 2 !== 0) throw new Error("odd-length hex");
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

export function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
