//! Hand-written Diesel schema matching `migrations/0001_scheduler_rolls/up.sql`.

diesel::table! {
    scheduler_rolls (id) {
        id                    -> Int8,
        underlying_symbol     -> Text,
        settlement_symbol     -> Text,
        expiry_ms             -> Int8,
        state                 -> Text,
        tx_digest             -> Nullable<Text>,
        bucket_ids            -> Nullable<Jsonb>,
        confirmed_bucket_ids  -> Nullable<Jsonb>,
        retry_count           -> Int4,
        last_error            -> Nullable<Text>,
        submit_anchor_seq     -> Nullable<Int8>,
        created_at            -> Timestamptz,
        updated_at            -> Timestamptz,
    }
}
