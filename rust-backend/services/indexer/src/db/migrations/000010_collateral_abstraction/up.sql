-- Collateral abstraction (plan §7): core holds no MM funds, so the
-- account-balance materialization is gone. The `accounts` table remains as
-- the QuoteSigner registry (signing key + owner), fed by SignerCreated /
-- SigningKeyRotated.
DROP TABLE IF EXISTS account_balances;
