CREATE TABLE cctp_transfers (
  id BIGSERIAL PRIMARY KEY,
  origin_chain TEXT NOT NULL,          -- 'sui' | 'solana'
  origin_tx_hash TEXT NOT NULL,
  origin_wallet TEXT NOT NULL,
  destination_wallet TEXT,             -- supplied by the frontend (needed to create a Solana ATA)
  mint_recipient TEXT,                 -- decoded from the CCTP message (hex bytes32)
  amount NUMERIC,                      -- decoded from the CCTP message (USDC base units)
  status TEXT NOT NULL,                -- pending_attestation | attested | minting | complete | failed
  message_hex TEXT,
  attestation_hex TEXT,
  mint_tx_hash TEXT,
  error TEXT,
  attempts INT NOT NULL DEFAULT 0,
  -- timing instrumentation (bridge duration = minted_at - burned_at)
  burned_at TIMESTAMPTZ,               -- on-chain timestamp of the source burn tx
  attested_at TIMESTAMPTZ,             -- when the poller first saw the attestation complete
  minted_at TIMESTAMPTZ,               -- on-chain timestamp of the destination mint tx
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (origin_chain, origin_tx_hash)
);
CREATE INDEX cctp_transfers_status_idx ON cctp_transfers (status);
CREATE INDEX cctp_transfers_origin_wallet_idx ON cctp_transfers (origin_wallet);
