# solana-mm-bot — frontend integration

There is no frontend-facing surface: solana-mm-bot is the MM **counterparty** on the solana-quoting-service WebSocket (it answers `RFQBroadcast`/`BulkViewRFQBroadcast` with signed/indicative quotes) and bids the venue's on-chain auctions directly.

Frontends never talk to it — integrate against solana-quoting-service instead; see `docs/solana/backend/05-solana-quoting-service.md` (RFQ flow, `quote_bytes_b64` for the Ed25519SigVerify precompile ix) and the quoting service's own frontend guide.

Ops surface only: `/health` + `/metrics` on port 9010.
