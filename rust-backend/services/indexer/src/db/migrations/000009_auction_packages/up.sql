-- Four-package auction restructure (audit): the RFQ/swap venues moved onto a
-- generic auction package, so `rfqs` rows are now keyed by the AUCTION object
-- id (`rfq_id` holds it; the options_rfq adapter's Rfq metadata object id
-- rides in the new `meta_id`). The shared discriminator becomes
-- `auction_kind` (call | put | swap | unknown) — swaps get rows too, and a
-- vault-coupled auction is 'unknown' only until an adapter/vault event (or
-- the escrow/bid type pair) classifies it. `bucket_id` goes nullable: swaps
-- have no bucket, and coupled option auctions may learn theirs late.
-- Fresh deployment (new package ids): existing rows are legacy-only.
ALTER TABLE rfqs RENAME COLUMN option_kind TO auction_kind;
ALTER TABLE rfqs ALTER COLUMN auction_kind SET DEFAULT 'unknown';
ALTER TABLE rfqs ALTER COLUMN bucket_id DROP NOT NULL;
ALTER TABLE rfqs ADD COLUMN meta_id TEXT;

ALTER TABLE rfq_bids RENAME COLUMN option_kind TO auction_kind;
ALTER TABLE rfq_bids ALTER COLUMN auction_kind SET DEFAULT 'unknown';
