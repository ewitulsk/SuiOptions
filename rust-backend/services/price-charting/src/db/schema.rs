//! Hand-written diesel schema; kept in sync with `migrations/`.

diesel::table! {
    pool_trades (pool_id, tx_digest, event_index) {
        time          -> Timestamptz,
        pool_id       -> Text,
        bucket_id     -> Text,
        price         -> Double,
        price_raw     -> Numeric,
        base_qty      -> Numeric,
        quote_qty     -> Numeric,
        base_decimals -> Int2,
        taker_is_bid  -> Bool,
        tx_digest     -> Text,
        event_index   -> Int8,
    }
}

diesel::table! {
    pool_mids (pool_id, time) {
        time      -> Timestamptz,
        pool_id   -> Text,
        bucket_id -> Text,
        best_bid  -> Double,
        best_ask  -> Double,
        mid       -> Double,
    }
}

diesel::table! {
    watch_cursor (id) {
        id         -> Int2,
        cursor_tx  -> Text,
        cursor_ev  -> Int8,
        updated_at -> Timestamptz,
    }
}

// Predicted-APY time-series (one row per (vault, kind, horizon) per sampler
// tick). The `time` column anchors the hypertable; the natural snapshot key is
// (vault_id, kind, horizon) read at the latest `time`.
diesel::table! {
    vault_predicted_apy (vault_id, kind, horizon, time) {
        time                 -> Timestamptz,
        vault_id             -> Text,
        kind                 -> Text,
        horizon              -> Int4,
        t_ms                 -> Int8,
        apy                  -> Float8,
        apy_low              -> Float8,
        apy_high             -> Float8,
        assignment_prob      -> Float8,
        downside_round_yield -> Float8,
        confidence           -> Float8,
    }
}

// Realized-APY time-series (one row per finalized round, idempotent).
diesel::table! {
    vault_realized_apy (vault_id, round, time) {
        time     -> Timestamptz,
        vault_id -> Text,
        round    -> Int8,
        apy      -> Float8,
    }
}

diesel::allow_tables_to_appear_in_same_query!(pool_trades, pool_mids, watch_cursor);
