-- Per-round premium breakdown (vault-implementation-guide nice-to-have):
-- RfqSettled carries gross_premium / fee / net_premium, but the rfqs view only
-- materialised net. Add gross + fee so the vault track record can show the
-- full breakdown. Nullable: open / expired-unsold auctions never settle.
ALTER TABLE rfqs
    ADD COLUMN gross_premium NUMERIC(39),
    ADD COLUMN fee           NUMERIC(39);
