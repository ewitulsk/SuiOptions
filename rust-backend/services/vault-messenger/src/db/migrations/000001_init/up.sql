-- Per-lane ordered message queue. A lane is (direction, spoke_id); seq is
-- the wire envelope sequence, strictly increasing per lane, so the unique
-- constraint is the duplicate suppressor for re-observed events.
CREATE TABLE vault_messages (
  id BIGSERIAL PRIMARY KEY,
  direction TEXT NOT NULL,      -- 'spoke_to_hub' | 'hub_to_spoke'
  spoke_id BIGINT NOT NULL,
  seq BIGINT NOT NULL,          -- wire envelope seq
  msg_type SMALLINT NOT NULL,   -- vault_messages::MsgType discriminant
  message_hex TEXT NOT NULL,    -- full wire bytes (envelope || payload)
  status TEXT NOT NULL,         -- pending | submitted | confirmed | failed
  attempts INT NOT NULL DEFAULT 0,
  tx_hash TEXT,                 -- delivery tx (sui digest / evm hash)
  error TEXT,
  observed_tx TEXT,             -- source-chain tx the message was seen in
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (direction, spoke_id, seq)
);
CREATE INDEX vault_messages_status_idx ON vault_messages (direction, status);

-- Watcher scan positions: EVM block number and Sui GraphQL event cursors,
-- stored verbatim.
CREATE TABLE watch_cursors (
  name TEXT PRIMARY KEY,
  cursor TEXT NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Hub-booked payables (SpokeWithdrawProcessed with pay_units > 0), settled
-- by SpokePayoutSettled. Feeds the payout-queue-aged alert.
CREATE TABLE spoke_payables (
  spoke_id BIGINT NOT NULL,
  request_seq BIGINT NOT NULL,
  pay_units NUMERIC NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  settled_at TIMESTAMPTZ,
  PRIMARY KEY (spoke_id, request_seq)
);

-- Latest per-spoke report from SpokeStateSynced events (fee pot, spoke
-- clock). Feeds the fee-pot-low alert and GET /lanes.
CREATE TABLE lane_stats (
  spoke_id BIGINT PRIMARY KEY,
  fee_pot NUMERIC NOT NULL,
  last_state_sync_ms BIGINT NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
