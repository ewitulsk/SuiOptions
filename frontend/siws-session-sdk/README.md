# @yourorg/sui-siws-session

Browser SDK for the SIWS session-key system. Authenticate with a Solana wallet,
mint a scoped on-chain `SessionCap` to an ephemeral Sui key, then act on the
user's behalf — within enforced caps — without signing every transaction.

See `../sui-siws-session-key-spec.md` for the full design and
`../contracts/` for the on-chain half.

## Surface

```ts
createSession(opts)        // → SessionHandle (one Solana / SIWS signature)
createSessionEth(opts)     // → SessionHandle (one Ethereum / SIWE EIP-4361 signature)
handle.execute(build)      // auto-signed, sponsored app call (no prompt)
handle.status()            // { expiresAt, spent, remaining, generation, active }
handle.revoke()            // root-signed generation bump → kills all caps (scheme-aware)
restoreSession(config)     // → SessionHandle | null (non-extractable keys only)
```

Both root schemes share the same `SessionHandle`, sponsor, and cap machinery —
they differ only in how the root signature is produced and verified (ed25519
vs. secp256k1 `ecrecover`). For Ethereum, pass an `EthereumSignMessage` (a thin
MetaMask `personal_sign` adapter) and the EVM `chainId`.

## Quick start

```ts
import {
  createSession,
  LocalSponsorClient,
} from "@yourorg/sui-siws-session";
import { SuiClient } from "@mysten/sui/jsonRpc"; // SuiJsonRpcClient

const client = new SuiClient({ url: "https://fullnode.testnet.sui.io" });

// Dev sponsor (in-process gas payer). In prod, use HttpSponsorClient against
// a relayer you run.
const sponsor = new LocalSponsorClient(client, gasKeypair, {
  allowedTargets: [`${PACKAGE_ID}::app_example::withdraw`],
});

const session = await createSession({
  client,
  network: "testnet",
  packageId: PACKAGE_ID,
  registryId: REGISTRY_ID,
  coinType: "0x2::sui::SUI",
  sponsor,
  solanaWallet: phantomAdapter,   // SolanaSignMessage
  spendCap: 1_000_000_000n,
  perTxCap: 100_000_000n,
  ttlMs: 15 * 60_000,
  allowed: ["siws_session::app_example::withdraw"], // matches the Move const
  persist: true,
});

// Later — no wallet prompt, gas paid by the sponsor:
await session.execute((tx, { capId, accountId }) => {
  tx.moveCall({
    target: `${PACKAGE_ID}::app_example::withdraw`,
    typeArguments: ["0x2::sui::SUI"],
    arguments: [
      tx.object(capId),
      tx.object(accountId),
      tx.object("0x6"),         // Clock
      tx.pure.u64(50_000_000n),
      tx.pure.address(recipient),
    ],
  });
});
```

## Key design points

- **Canonical message** (`message.ts`) is byte-for-byte identical to
  `contracts/sources/message.move`; both are pinned against the same reference
  vectors. This is the single highest-risk integration point.
- **Ephemeral key** (`signer.ts`) prefers a **non-extractable** WebCrypto
  Ed25519 key — the private key never enters JS memory. Falls back to an
  in-memory software keypair where WebCrypto Ed25519 is unavailable.
- **Reload survival** (`store.ts`) persists only the non-extractable
  `CryptoKeyPair` handle in IndexedDB — never a raw secret. Software-key
  sessions intentionally do not survive reloads.
- **The `allowed` selectors** must match the Move allowlist literals
  (`siws_session::app_example::withdraw`), not the numeric package id — the
  on-chain `const` uses the symbolic package name.

## Allowlist note

The `allowed` array gates which app functions a cap may call (enforced
on-chain in `session::authorize`). The sponsor's `allowedTargets` is a
*separate*, defense-in-depth gate on which calls the relayer will pay gas for.

## Scripts

```bash
npm run build       # emit dist/
npm run typecheck
npm test            # serializer byte-exactness
```
