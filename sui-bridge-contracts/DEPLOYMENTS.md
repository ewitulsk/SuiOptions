# Bridge deployments

## ✅ M2 round trip — LIVE both directions (2026-07-01)

HyperEVM→Sui→HyperEVM completed on testnet, supply invariant holding:
1. HyperEVM `Locker.lock(1 tBTC)` → escrow 1e18; Outbox committed `0x8228fc…`.
2. Ed25519-signed (group id 1) → Sui `locker::bridge_receive<WBTC>` → **minted 1.00 WBTC**.
3. Sui `locker::bridge_out(1 WBTC)` → burned; Outbox committed `0x2449c1a3…`.
4. ECDSA-signed (group id 2) → HyperEVM `Inbox.receiveMessage` → **released 1 tBTC**, escrow 0.

Both reconstructed digests matched the on-chain `messageHash`; both signatures verified
on the real Inboxes.

**Relayer binary validated (2026-07-01).** Ran the actual `bridge-signer-service` +
`bridge-relayer` binaries against these live contracts:
- Signer live-verified the EVM commitment via `RpcVerifier`/`EvmProbe` and signed (202).
- The real `SuiDestSubmitter` write path delivered end-to-end: sui-sdk connect → on-chain
  `getObject(dst_app)` type read → `parse_dispatch` → `bridge_receive` PTB → execute →
  **minted 2.00 WBTC** (a fresh 2-tBTC lock).
- Two findings fixed/noted: (1) `EvmProbe` used an unbounded `eth_getLogs` range which
  public RPCs reject ("max block range 1000") — fixed to a bounded `lookback_blocks`
  window (default 1000); (2) the EVM *source* watcher hits intermittent HTTP 500
  (`ErrUpstreamsExhausted`) from the shared `rpcs.chain.link` endpoint under sustained
  polling — a public-RPC rate limit, not a code bug; use a dedicated RPC in production.

**Layer-2 + demo group keys (deployer `0xab8d…` on Sui, `0x303c…` on EVM):**

| Object | Id |
|--------|----|
| Sui `sui_bridge` pkg (supersedes `0x6435…`) | `0xe403373522aa0dce645671bbd36ca1e80147c6418e1e4e08c97c8fa224a81253` |
| Sui Inbox / Outbox | `0xd35e063d1a1e5d6e5d57925523869cc86db61e164620f22d9d9e5bad77b9870d` / `0x779a01e6b2352fdb99340691699f21eb03cccfe9dec4419860adf23eb149f6df` |
| Sui GroupKeyRegistry (Ed25519 id 1 = `2152f8…`, seed `[0x42;32]`) | `0x39cce89c3c8b374b1a09922668394d555e11588e8e60d990db43c4835ced21a3` |
| Sui `locker` pkg / Locker`<WBTC>` (Mint) | `0x3ef9871fa5f93ac300317d3b240c85ee8f59f504de255e8e4c3f13b8d404160b` / `0x96e9a86b1c0585a49b5a75e13c2d77633bee4d1c9c31ef8659f1e708c788cae8` |
| Sui WBTC coin pkg | `0x6ef3c4764e35d35a4ca62d7aa4b96d2ee1360dfc23c073eca3ff2ec37b3507d3` |
| EVM Locker (Escrow) / test tBTC | `0x84E2e2C27217E10dF502cABD095421F9b364E098` / `0x8244B193a4a545D316c0eDe86c256A40dB0Ea439` |
| EVM group key id 2 (ECDSA `19e7…`, seed `[0x11;32]`) on Registry `0x676fBa34…` | — |

> The demo uses group keys whose seeds we control (`[0x42;32]`/`[0x11;32]`) so a manual
> relay can sign; the ticket-01 group keys (`0x40cc…`/`0x6B908C…`) have external seeds.

---


## Domain separator (ticket 01)

Digest is `keccak256(DOMAIN_SEP || encode(message))`, `DOMAIN_SEP =
keccak256("XCHAIN_MSG_V1" || deployment_salt)` (spec §2.2). The **deployment
salt for this testnet deployment** is:

```
deployment_salt = keccak256("sui-options-bridge:testnet:2026-07")
                = 0x857bf91867252dcae83dfed125aa1f1862c15fb15aba78b499e3fb72cfaabc8a
```

This exact 32-byte value MUST be passed to the Sui `inbox::create`/`outbox::create`,
the Solidity `Inbox`/`Outbox` constructors (`DEPLOYMENT_SALT`), and both services
(`deployment_salt_hex` → `BRIDGE_DEPLOYMENT_SALT`). Cross-language parity is
locked by the shared test vectors (test salt `0x01*32`, digest
`0x535392536947463d04988702a5480f431f34efed3cf557dc12aa434c2decd707`).

On-chain `DOMAIN_SEP` for this salt (derived on-chain by both chains):
`0x734dccf071e185b986c6693b02fba9371d89a10fca38cb4e73793ca8607fd1dc`.

## Sui testnet — REDEPLOYED 2026-07-01 (domain-separated, ticket 01)

Fresh publish (the `message::hash` signature changed → upgrade-incompatible).
Deployer / governance / guardian: `0xab8d1b5a5311c9400e3eaf5c3b641f10fb48b43cc30d365fa8a98a6ca6bd4865`
Publish digest: `HWBj2wrY5QcsYkoNfugC69e5hK2mQQKwfHCtwcA2eb84`

| Object | Id |
|--------|----|
| Package | `0x6435311f4d8891f7392cadcc3cc503e71757ba3d24f043f5c04733da7ae6b000` |
| ChainRegistry (shared) | `0x75789ceda6f51224483e5d1f1dfd40f70cd085439aa943883a5ccc56557a0d22` |
| GroupKeyRegistry (shared) | `0x16ffe9d907a9bd1bd274cc8b48bc3092ae3546f9e9d2c57e7841984468856144` |
| GovernanceCap | `0xe241c3a8bdd77a1883434eda223ed1f1ac12042902b3a4bf557cb8692d437d81` |
| GuardianCap | `0xc2f4c83325ecf5ff3bebd3e085202ff2a9775d93e11c928e33ae4879c4ac7c46` |
| UpgradeCap | `0xb0096a1cc730608cd6b703d999d94e75114b02450d27b6d3b00480e62eee98aa` |
| Inbox (dst=Sui, shared) | `0x32c3cfe0571167002fc386d7bae00a6d761ada54f70dcf4e8e18ee615c230250` |
| Outbox (src=Sui, shared) | `0x0e505016a6b46226b203506827a93e52f780f79c87bce8d80f94143b6e8a6431` |

Wiring: Sui (`134217728`, finality 1) + HyperEVM (`268436454`, finality 0/12,
EVM addrs zeroed pending the EVM redeploy) registered; Ed25519 group key id `1` =
`0x40cc5cb8a797c03eece3e93b09243c6bff29346def1020c8fdce6f6b17b0be3e`.
On-chain smoke: Inbox + Outbox `domain_sep` both read back = the expected
`0x734dcc…d1dc`. The 2026-06-29 Sui deployment below is **superseded**.

## HyperEVM testnet — REDEPLOYED 2026-07-01 (domain-separated, ticket 01)

Broadcast via the Chainlink first-party RPC `https://rpcs.chain.link/hyperevm/testnet`
(the canonical `rpc.hyperliquid-testnet.xyz` is blocked by an upstream SNI egress
filter that injects `ff ff ff ff ff` at the TLS handshake — see the investigation
notes; the endpoint/URL are correct, the path is filtered).
Deployer / governance / guardian: `0x303c0af404a4444c3224aaF2628988940C6D5705`

| Contract | Address |
|----------|---------|
| Registry | `0x676fBa345f0e5dB7931AdB214d73B3A1989A0fD2` |
| Inbox (dst=HyperEVM) | `0xD4524ce4b234c24B156631Ca612EC387de39968C` |
| Outbox (src=HyperEVM) | `0x1797FAa1eAF0cc1fC7C092Db0035A3c46A357ff6` |

On-chain verification (via cast): both `Inbox.domainSep()` and `Outbox.domainSep()`
= `0x734dcc…d1dc` — **byte-identical to the live Sui contracts**, so one threshold
signature's digest domain matches on both chains. Group key id `1` = ECDSA
`0x6B908C2c00C2C99865301b112e04550a412b421e`. `Inbox.dstChainId()` = 268436454.

Registry wiring: HyperEVM (`268436454`, finality 0/12) + Sui (`134217728`,
finality 1/0) registered; the Sui entry carries the **new** Sui Outbox/Inbox
object ids (passed via `SUI_OUTBOX`/`SUI_INBOX`).

> **Follow-up (ticket 02):** the *Sui* ChainRegistry's HyperEVM entry was
> registered with zero outbox/inbox addrs (the EVM addresses didn't exist yet).
> `registry.move` has no `update_chain`, so backfilling the real EVM addresses
> above needs a small governance function added there (the source verifier in
> ticket 02 needs the EVM Outbox address to verify EVM→Sui commitments).


## Sui testnet — deployed 2026-06-29

Deployer / governance / guardian: `0xab8d1b5a5311c9400e3eaf5c3b641f10fb48b43cc30d365fa8a98a6ca6bd4865`
Publish digest: `BcWbRxsj1ZsSSTSc1pAfEbnZzzDCususzYqa81B8EjXY`

| Object | Id |
|--------|----|
| Package | `0x60abcb3006916a853bc7e51abe28e6c27beff659112363cea747a73e6b5d7eb8` |
| ChainRegistry (shared) | `0xba290f44421b8d056ee7c3cc4496c24cf257a6d445c5115d4ca2f18d5d160e20` |
| GroupKeyRegistry (shared) | `0x74ad3c2d056e0f9d1f31c4510005c12950faf5955686791168326bf782fc95fb` |
| GovernanceCap | `0x7b0be8902bb9356b41f2a72cbc24752ce1fa4205959bc911275186991ee6176c` |
| GuardianCap | `0xb96f1538548eebc221cee11e96a5409c78ecce195a5c6fd125d8e9543e4ef002` |
| UpgradeCap | `0x2b953d865611c213b1845c7142136834ab35e5b22c359e0cbc79fc2d576e8c16` |
| Inbox (dst=Sui, shared) | `0x7934aa71a9a3ebd1099fe294b8a24b47d3b78c4cc5969b7ba0a383d73c7e0e1d` |
| Outbox (src=Sui, shared) | `0x3989143acf84f6fcf899b2004eba27e71f5b12a019569c56d7f1604d18b1ec66` |

Registry wiring:
- Sui chain registered: internal id `134217728` (= family 1 `<< 27 | 0`), finality kind 1.
- HyperEVM chain registered: internal id `268436454`, finality kind 0 / value 12,
  outbox/inbox = the EVM addresses below (left-padded to 32 bytes).
- Group key id `1`: Ed25519, pubkey `0x40cc5cb8a797c03eece3e93b09243c6bff29346def1020c8fdce6f6b17b0be3e`.

## HyperEVM testnet — deployed 2026-06-29

Chain: HyperEVM testnet (chainId 998). Internal id `268436454` (= family 2 `<< 27 | 998`).
Deployer / governance / guardian: `0x303c0af404a4444c3224aaF2628988940C6D5705`

| Contract | Address |
|----------|---------|
| Registry | `0x375D5CE9772ea59Ee58B62ec2E25c072872a7401` |
| Inbox (dst=HyperEVM) | `0xA2b0dA5F12628f1FDC1E517DF33F5C3fF528bF74` |
| Outbox (src=HyperEVM) | `0xbCE58f862011C83DA87b6061e3B8bCf3d1767051` |

Registry wiring:
- HyperEVM chain registered (`268436454`), Sui chain registered (`134217728`).
- Group key id `1`: ECDSA, address `0x6B908C2c00C2C99865301b112e04550a412b421e`.

## M1 signer keys (TESTNET ONLY)

Seeds live in `solidity/.env` (gitignored) and the signer-service config. The
same group keys are registered on both chains under **id 1**:
- Ed25519 (Sui): pubkey `0x40cc5cb8a797c03eece3e93b09243c6bff29346def1020c8fdce6f6b17b0be3e`
- ECDSA (EVM): address `0x6B908C2c00C2C99865301b112e04550a412b421e`
