//! Hand-written diesel schema; kept in sync with `migrations/`.

diesel::table! {
    vault_messages (id) {
        id          -> Int8,
        direction   -> Text,
        spoke_id    -> Int8,
        seq         -> Int8,
        msg_type    -> Int2,
        message_hex -> Text,
        status      -> Text,
        attempts    -> Int4,
        tx_hash     -> Nullable<Text>,
        error       -> Nullable<Text>,
        observed_tx -> Nullable<Text>,
        created_at  -> Timestamptz,
        updated_at  -> Timestamptz,
    }
}

diesel::table! {
    watch_cursors (name) {
        name       -> Text,
        cursor     -> Text,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    spoke_payables (spoke_id, request_seq) {
        spoke_id    -> Int8,
        request_seq -> Int8,
        pay_units   -> Numeric,
        created_at  -> Timestamptz,
        settled_at  -> Nullable<Timestamptz>,
    }
}

diesel::table! {
    lane_stats (spoke_id) {
        spoke_id           -> Int8,
        fee_pot            -> Numeric,
        last_state_sync_ms -> Int8,
        updated_at         -> Timestamptz,
    }
}
