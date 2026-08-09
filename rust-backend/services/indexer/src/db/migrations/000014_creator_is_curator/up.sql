-- SO-365: creator is always the initial curator and max_positions is gone
-- from the vault config, so neither column has a source any more.
ALTER TABLE trading_vaults DROP COLUMN rotation_authority;
ALTER TABLE trading_vaults DROP COLUMN max_positions;
