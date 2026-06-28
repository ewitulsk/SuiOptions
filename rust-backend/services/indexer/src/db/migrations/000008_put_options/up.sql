-- Cash-secured put support: single tables shared with calls, discriminated by
-- an `option_kind` column. Existing rows (all calls) default to 'call'.
ALTER TABLE buckets   ADD COLUMN option_kind TEXT NOT NULL DEFAULT 'call';
ALTER TABLE positions ADD COLUMN option_kind TEXT NOT NULL DEFAULT 'call';
ALTER TABLE rfqs      ADD COLUMN option_kind TEXT NOT NULL DEFAULT 'call';
ALTER TABLE rfq_bids  ADD COLUMN option_kind TEXT NOT NULL DEFAULT 'call';
