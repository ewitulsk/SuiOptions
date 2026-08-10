-- Market whitelist: serving/intake is gated on `enabled`. Rows are upserted
-- from the deployments record at boot (new rows default enabled); rows whose
-- registry left the record are disabled, never deleted — historical fills
-- and orders reference them. Flipping `enabled` off is the ops delist path
-- and survives restarts (the boot upsert never touches the column).
ALTER TABLE exchange_markets
    ADD COLUMN enabled   BOOLEAN     NOT NULL DEFAULT TRUE,
    ADD COLUMN listed_at TIMESTAMPTZ NOT NULL DEFAULT now();
