//! Hand-written equivalent of what `diesel print-schema` would generate.
//! Kept in sync with `migrations/000001_init/up.sql`.

diesel::table! {
    indexer_progress (id) {
        id              -> Int2,
        last_checkpoint -> Int8,
        last_sequence   -> Int8,
        updated_at      -> Timestamptz,
    }
}

diesel::table! {
    indexed_events (sequence) {
        sequence     -> Int8,
        checkpoint   -> Int8,
        tx_digest    -> Text,
        event_index  -> Int4,
        timestamp_ms -> Int8,
        event_type   -> Text,
        payload      -> Jsonb,
    }
}

diesel::table! {
    accounts (account_id) {
        account_id     -> Text,
        owner          -> Nullable<Text>,
        signing_pubkey -> Bytea,
        updated_at_seq -> Int8,
    }
}

diesel::table! {
    account_balances (account_id, asset_type) {
        account_id     -> Text,
        asset_type     -> Text,
        balance        -> Numeric,
        updated_at_seq -> Int8,
    }
}

diesel::table! {
    buckets (bucket_id) {
        bucket_id       -> Text,
        asset_type      -> Text,
        settlement_type -> Text,
        strike          -> Numeric,
        strike_scale    -> Int2,
        expiry_ms       -> Int8,
        total_written   -> Numeric,
        exercise_cursor -> Numeric,
        cleaned         -> Bool,
        invalidated     -> Bool,
        updated_at_seq  -> Int8,
    }
}

diesel::table! {
    positions (bucket_id, range_start) {
        bucket_id      -> Text,
        range_start    -> Numeric,
        range_end      -> Numeric,
        recipient      -> Text,
        updated_at_seq -> Int8,
    }
}

diesel::joinable!(account_balances -> accounts (account_id));
diesel::allow_tables_to_appear_in_same_query!(
    indexer_progress,
    indexed_events,
    accounts,
    account_balances,
    buckets,
    positions,
);
