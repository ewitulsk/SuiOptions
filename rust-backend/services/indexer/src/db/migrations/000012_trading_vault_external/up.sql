-- SO-299: external MM account columns on the trading-vault view, fed from
-- the ExternalAccountSet/Cleared/Released/Returned events plus the
-- equity_oracle's EquityPosted attestations.

ALTER TABLE trading_vaults
    ADD COLUMN external_account TEXT,
    ADD COLUMN external_exposure BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN latest_external_equity BIGINT,
    ADD COLUMN external_equity_updated_at_ms BIGINT;
