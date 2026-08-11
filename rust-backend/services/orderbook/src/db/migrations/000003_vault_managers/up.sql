-- Trading-vault manager modes (SO-372): BalanceManagers custodied by a
-- trading vault via the exchange_adapter, learned from its CustodyCreated
-- events. `direct` marks identity-only managers whose orders escrow against
-- the VAULT's free balances — fills/matches for them go through the
-- exchange-adapter entries instead of settlement directly, and the mirrored
-- escrow check does not apply. Managers absent from this table are plain
-- wallet (or funded-custody) BMs.
CREATE TABLE exchange_vault_managers (
    manager_id TEXT PRIMARY KEY,
    vault_id   TEXT NOT NULL,
    custody_id TEXT NOT NULL,
    direct     BOOLEAN NOT NULL
);
