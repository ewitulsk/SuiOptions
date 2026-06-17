//! Hand-written Diesel schema matching the migrations in `migrations/`.

diesel::table! {
    scheduler_rolls (id) {
        id                    -> Int8,
        underlying_symbol     -> Text,
        settlement_symbol     -> Text,
        expiry_ms             -> Int8,
        expiry_interval_ms    -> Int8,
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

diesel::table! {
    scheduler_vaults (id) {
        id                  -> Int8,
        underlying_symbol   -> Text,
        settlement_symbol   -> Text,
        round_ms            -> Int8,
        state               -> Text,
        share_coin_package  -> Nullable<Text>,
        share_coin_type     -> Nullable<Text>,
        share_cap_id        -> Nullable<Text>,
        vault_id            -> Nullable<Text>,
        publish_digest      -> Nullable<Text>,
        create_digest       -> Nullable<Text>,
        last_error          -> Nullable<Text>,
        created_at          -> Timestamptz,
        updated_at          -> Timestamptz,
    }
}
