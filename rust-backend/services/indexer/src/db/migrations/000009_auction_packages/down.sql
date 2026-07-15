ALTER TABLE rfq_bids ALTER COLUMN auction_kind SET DEFAULT 'call';
ALTER TABLE rfq_bids RENAME COLUMN auction_kind TO option_kind;

ALTER TABLE rfqs DROP COLUMN meta_id;
ALTER TABLE rfqs ALTER COLUMN bucket_id SET NOT NULL;
ALTER TABLE rfqs ALTER COLUMN auction_kind SET DEFAULT 'call';
ALTER TABLE rfqs RENAME COLUMN auction_kind TO option_kind;
