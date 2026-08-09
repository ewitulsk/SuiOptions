ALTER TABLE trading_vaults ADD COLUMN rotation_authority SMALLINT NOT NULL DEFAULT 0;
ALTER TABLE trading_vaults ADD COLUMN max_positions BIGINT NOT NULL DEFAULT 0;
