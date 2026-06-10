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
        signing_scheme -> Nullable<Int2>,
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
    orgs (org_id) {
        org_id         -> Text,
        name           -> Text,
        fee_bps        -> Int8,
        creator        -> Text,
        updated_at_seq -> Int8,
    }
}

diesel::table! {
    buckets (bucket_id) {
        bucket_id       -> Text,
        asset_type      -> Text,
        settlement_type -> Text,
        call_type       -> Text,
        strike          -> Numeric,
        strike_scale    -> Int2,
        expiry_ms       -> Int8,
        total_written   -> Numeric,
        exercise_cursor -> Numeric,
        cleaned         -> Bool,
        invalidated     -> Bool,
        updated_at_seq  -> Int8,
        org_id          -> Text,
    }
}

diesel::table! {
    positions (bucket_id, range_start) {
        bucket_id      -> Text,
        range_start    -> Numeric,
        range_end      -> Numeric,
        object_id      -> Text,
        recipient      -> Text,
        updated_at_seq -> Int8,
        premium_received -> Numeric,
        mm_account_id    -> Text,
        tx_digest        -> Text,
        minted_at_ms     -> Int8,
    }
}

diesel::table! {
    bucket_deepbook_pools (bucket_id) {
        bucket_id            -> Text,
        pool_id              -> Text,
        base_asset_type      -> Text,
        quote_asset_type     -> Text,
        tick_size            -> Int8,
        lot_size             -> Int8,
        min_size             -> Int8,
        taker_fee            -> Int8,
        maker_fee            -> Int8,
        created_checkpoint   -> Int8,
        created_timestamp_ms -> Int8,
        updated_at_seq       -> Int8,
    }
}

diesel::table! {
    event_participants (sequence, address, role) {
        sequence -> Int8,
        address  -> Text,
        role     -> Text,
    }
}

diesel::joinable!(account_balances -> accounts (account_id));
diesel::joinable!(event_participants -> indexed_events (sequence));
diesel::joinable!(bucket_deepbook_pools -> buckets (bucket_id));
diesel::allow_tables_to_appear_in_same_query!(
    event_participants,
    indexer_progress,
    indexed_events,
    accounts,
    account_balances,
    orgs,
    buckets,
    positions,
    bucket_deepbook_pools,
);
