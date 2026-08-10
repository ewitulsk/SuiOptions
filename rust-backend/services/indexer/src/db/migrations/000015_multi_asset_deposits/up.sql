-- SO-370: multi-asset deposits/withdrawals. The vault's coin type is now
-- the unit of account (deposits may arrive in any allowlisted asset), so
-- the column follows the event field rename.
ALTER TABLE trading_vaults RENAME COLUMN deposit_asset TO accounting_asset;
