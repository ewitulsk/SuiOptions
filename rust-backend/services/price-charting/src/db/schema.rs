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

diesel::allow_tables_to_appear_in_same_query!(pool_trades, pool_mids, watch_cursor);
