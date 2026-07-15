# SIWS Session Keys — demo frontend

Vite + React 18 + `@mysten/dapp-kit` (same stack as `../../frontend`). Exercises
the full SDK flow against the published `siws_session` package.

## What it shows

1. **Sponsor** — a demo-only in-browser gas payer (keypair in localStorage).
   Pays gas + seeds the Account; can't move user funds.
2. **Connect Phantom (Solana / SIWS) or MetaMask (Ethereum / SIWE)** — either
   wallet can be the root identity. MetaMask signs the canonical EIP-4361
   message; the contract recovers the address via on-chain `ecrecover`.
3. **Open session** — ONE Solana signature mints a scoped `SessionCap` to a
   non-extractable WebCrypto Ed25519 key.
4. **Auto-signed withdraw** — repeated app calls with NO wallet prompt, gas paid
   by the sponsor, enforced against the per-tx / total caps on-chain.
5. **Status & revoke** — live spent/remaining/generation; revoke bumps the
   account generation and kills every cap (one Solana signature).

The session persists across reloads (the non-extractable key handle is stored
in IndexedDB — never a raw secret).

## Run

```bash
# 1. Publish the contracts and note the package id + Registry object id.
#    (cd ../contracts && sui client publish --gas-budget 200000000)

# 2. Point the demo at them:
cat > .env.local <<EOF
VITE_NETWORK=testnet
VITE_PACKAGE_ID=0x<package id>
VITE_REGISTRY_ID=0x<registry object id>
VITE_COIN_TYPE=0x2::sui::SUI
EOF

# 3. Install + run:
npm install
npm run dev          # http://localhost:5174
```

On first load, copy the **sponsor address** and fund it from the testnet faucet
(`sui client faucet --address <addr>` or https://faucet.sui.io). Then connect
Phantom, open a session, fund the Account, and try auto-signed withdrawals.

## Requirements

- [Phantom](https://phantom.app) (Solana root) and/or
  [MetaMask](https://metamask.io) (Ethereum root) browser extension.
- A browser with WebCrypto Ed25519 (recent Chrome/Safari/Firefox) for the
  non-extractable session key; otherwise the SDK falls back to a software key
  that won't survive reloads.
