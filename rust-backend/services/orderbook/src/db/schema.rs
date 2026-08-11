//! Hand-written equivalent of what `diesel print-schema` would generate.
//! Kept in sync with `migrations/000001_init/up.sql`.

diesel::table! {
    exchange_markets (registry_id) {
        registry_id     -> Text,
        symbol          -> Text,
        base            -> Text,
        quote           -> Text,
        tick_size       -> Int8,
        min_size        -> Int8,
        lot_size        -> Int8,
        current_fee_bps -> Int8,
        enabled         -> Bool,
        listed_at       -> Timestamptz,
    }
}

diesel::table! {
    exchange_orders (digest) {
        digest       -> Text,
        registry_id  -> Text,
        maker        -> Text,
        manager_id   -> Text,
        maker_token  -> Text,
        side         -> Text,
        price_ticks  -> Int8,
        salt         -> Int8,
        expiry_ms    -> Int8,
        taker_amount -> Int8,
        maker_amount -> Int8,
        filled_taker -> Int8,
        status       -> Text,
        order_json   -> Jsonb,
        order_bytes  -> Bytea,
        created_at   -> Timestamptz,
        updated_at   -> Timestamptz,
    }
}

diesel::table! {
    exchange_fills (tx_digest, event_seq) {
        tx_digest       -> Text,
        event_seq       -> Int8,
        digest          -> Text,
        registry_id     -> Text,
        maker           -> Text,
        taker           -> Text,
        base_amount     -> Int8,
        quote_amount    -> Int8,
        maker_fee       -> Int8,
        taker_fee       -> Int8,
        maker_sold_base -> Bool,
        filled_total    -> Int8,
        timestamp_ms    -> Int8,
    }
}

diesel::table! {
    exchange_balances (manager_id, token) {
        manager_id -> Text,
        token      -> Text,
        amount     -> Int8,
    }
}

diesel::table! {
    exchange_approved_signers (manager_id, signer) {
        manager_id -> Text,
        signer     -> Text,
    }
}

diesel::table! {
    exchange_cursors (name) {
        name   -> Text,
        cursor -> Text,
    }
}

diesel::table! {
    exchange_salt_watermarks (registry_id, maker) {
        registry_id    -> Text,
        maker          -> Text,
        min_valid_salt -> Int8,
    }
}

diesel::table! {
    exchange_vault_managers (manager_id) {
        manager_id -> Text,
        vault_id   -> Text,
        custody_id -> Text,
        direct     -> Bool,
    }
}

diesel::allow_tables_to_appear_in_same_query!(
    exchange_markets,
    exchange_orders,
    exchange_fills,
    exchange_balances,
    exchange_approved_signers,
    exchange_cursors,
    exchange_salt_watermarks,
    exchange_vault_managers,
);
