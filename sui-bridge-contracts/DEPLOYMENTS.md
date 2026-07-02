# Bridge deployments

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
