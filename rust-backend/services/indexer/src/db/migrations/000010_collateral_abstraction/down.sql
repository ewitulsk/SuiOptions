CREATE TABLE account_balances (
    account_id      TEXT         NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    asset_type      TEXT         NOT NULL,
    balance         NUMERIC(39)  NOT NULL,
    updated_at_seq  BIGINT       NOT NULL,
    PRIMARY KEY (account_id, asset_type)
);
