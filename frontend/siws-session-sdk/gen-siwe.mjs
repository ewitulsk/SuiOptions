// Generates the shared reference vectors pinned in BOTH test suites
// (`contracts/tests/session_tests.move` + `src/message.test.ts`, and
// `contracts/tests/siwe_tests.move` + `src/siwe.test.ts`), including a REAL
// secp256k1 signature (priv = 0x01 * 32) so the Move test exercises the full
// EIP-191 + ecrecover + keccak address-derivation path.
//
// Run after `npm run build`:  node gen-siwe.mjs

import { keccak_256 } from "@noble/hashes/sha3.js";
import { secp256k1 } from "@noble/curves/secp256k1.js";
import {
  buildSiweSessionMessage,
  buildSiweWithdrawMessage,
  serializeSessionMessage,
  serializeRevokeMessage,
  serializeWithdrawMessage,
} from "./dist/index.js";

const fill = (n, v) => new Uint8Array(n).fill(v);
const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");

// Shared limit fixture (canonical type strings).
const LIMITS = [
  {
    coinType: "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
    perTx: 200n,
    total: 500n,
  },
  {
    coinType: "0x00000000000000000000000000000000000000000000000000000000000000aa::tusdc::TUSDC",
    perTx: 1000000n,
    total: 5000000n,
  },
];

// --- SIWS (Solana) session + revoke messages ---

const session = serializeSessionMessage({
  domain: "0x1",
  network: "testnet",
  solanaPk: fill(32, 0x11),
  sessionKey: "0x2",
  generation: 7,
  nonce: fill(32, 0x22),
  expiresAtMs: 1700000000000,
  limits: LIMITS,
});
console.log("=== SIWS session message ===");
console.log(new TextDecoder().decode(session));

const revoke = serializeRevokeMessage({
  domain: "0x1",
  network: "testnet",
  solanaPk: fill(32, 0x11),
  accountId: "0x3",
  nonce: fill(32, 0x22),
  expiresAtMs: 1700000000000,
});
console.log("\n=== SIWS revoke message ===");
console.log(new TextDecoder().decode(revoke));

// --- SIWE message + real signature ---

const priv = fill(32, 0x01);
const pub = secp256k1.getPublicKey(priv, false); // uncompressed, 0x04 || X || Y
const ethAddress = keccak_256(pub.slice(1)).slice(12);
console.log("\n=== eth address ===");
console.log(hex(ethAddress));

const siwe = buildSiweSessionMessage({
  registryDomain: "0x1",
  ethAddress,
  sessionKey: "0x2",
  generation: 0,
  nonce: fill(32, 0x22),
  expiresAtMs: 1700000000000,
  chainId: 1,
  issuedAt: "2026-06-09T00:00:00.000Z",
  limits: LIMITS,
});
console.log("\n=== SIWE message ===");
console.log(siwe);

// EIP-191 personal_sign digest.
const msgBytes = new TextEncoder().encode(siwe);
const prefixed = new Uint8Array([
  0x19,
  ...new TextEncoder().encode(`Ethereum Signed Message:\n${msgBytes.length}`),
  ...msgBytes,
]);
const digest = keccak_256(prefixed);
// noble v2 "recovered" format is recovery-byte FIRST; Sui wants r||s||v.
// prehash:false — Sui's ecrecover applies keccak to the message itself.
const recovered = secp256k1.sign(digest, priv, { format: "recovered", prehash: false });
const sig65 = new Uint8Array(65);
sig65.set(recovered.slice(1), 0);
sig65[64] = recovered[0];
console.log("\n=== SIWE signature (r||s||v) ===");
console.log(hex(sig65));

// Self-check: emulate the Move recover path (ecrecover -> keccak -> addr).
const sigObj = secp256k1.Signature.fromBytes(sig65.slice(0, 64), "compact")
  .addRecoveryBit(sig65[64]);
const recPub = sigObj.recoverPublicKey(digest).toBytes(false);
const recAddr = keccak_256(recPub.slice(1)).slice(12);
console.log("recover self-check:", hex(recAddr) === hex(ethAddress) ? "OK" : "MISMATCH");

// --- withdrawal-authorization messages (SIWS + SIWE w/ real signature) ---

const SUI_TYPE =
  "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI";

const withdraw = serializeWithdrawMessage({
  domain: "0x1",
  network: "testnet",
  solanaPk: fill(32, 0x11),
  accountId: "0x3",
  coinType: SUI_TYPE,
  amount: 250000n,
  recipient: "0x4",
  nonce: fill(32, 0x22),
  expiresAtMs: 1700000000000,
});
console.log("\n=== SIWS withdraw message ===");
console.log(new TextDecoder().decode(withdraw));

const siweWithdraw = buildSiweWithdrawMessage({
  registryDomain: "0x1",
  ethAddress,
  accountId: "0x3",
  coinType: SUI_TYPE,
  amount: 250000n,
  recipient: "0x4",
  nonce: fill(32, 0x22),
  expiresAtMs: 1700000000000,
  chainId: 1,
  issuedAt: "2026-06-09T00:00:00.000Z",
});
console.log("\n=== SIWE withdraw message ===");
console.log(siweWithdraw);

const wBytes = new TextEncoder().encode(siweWithdraw);
const wPrefixed = new Uint8Array([
  0x19,
  ...new TextEncoder().encode(`Ethereum Signed Message:\n${wBytes.length}`),
  ...wBytes,
]);
const wDigest = keccak_256(wPrefixed);
const wRecovered = secp256k1.sign(wDigest, priv, { format: "recovered", prehash: false });
const wSig65 = new Uint8Array(65);
wSig65.set(wRecovered.slice(1), 0);
wSig65[64] = wRecovered[0];
console.log("\n=== SIWE withdraw signature (r||s||v) ===");
console.log(hex(wSig65));

const wSigObj = secp256k1.Signature.fromBytes(wSig65.slice(0, 64), "compact").addRecoveryBit(
  wSig65[64],
);
const wRecAddr = keccak_256(wSigObj.recoverPublicKey(wDigest).toBytes(false).slice(1)).slice(12);
console.log("withdraw recover self-check:", hex(wRecAddr) === hex(ethAddress) ? "OK" : "MISMATCH");
