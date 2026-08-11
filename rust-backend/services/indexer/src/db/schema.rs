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
        option_kind     -> Text,
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
        option_kind      -> Text,
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
    rfqs (rfq_id) {
        // The generic auction object id (four-package layout); the adapter's
        // Rfq metadata object id rides in `meta_id`.
        rfq_id          -> Text,
        bucket_id       -> Nullable<Text>,
        origin          -> Text,
        amount          -> Numeric,
        reserve_premium -> Numeric,
        deadline_ms     -> Int8,
        best_premium    -> Nullable<Numeric>,
        best_bidder     -> Nullable<Text>,
        status          -> Text,
        winner          -> Nullable<Text>,
        net_premium     -> Nullable<Numeric>,
        position_id     -> Nullable<Text>,
        gross_premium   -> Nullable<Numeric>,
        fee             -> Nullable<Numeric>,
        updated_at_seq  -> Int8,
        auction_kind    -> Text,
        meta_id         -> Nullable<Text>,
    }
}

diesel::table! {
    rfq_bids (rfq_id, sequence) {
        rfq_id         -> Text,
        sequence       -> Int8,
        bidder         -> Text,
        call_recipient -> Text,
        premium        -> Numeric,
        auction_kind   -> Text,
    }
}

diesel::table! {
    vaults (vault_id) {
        vault_id         -> Text,
        underlying_type  -> Text,
        settlement_type  -> Text,
        share_type       -> Text,
        round            -> Int8,
        current_bucket   -> Nullable<Text>,
        latest_pps       -> Nullable<Numeric>,
        total_shares     -> Numeric,
        pending_deposits -> Numeric,
        deposits_paused  -> Bool,
        updated_at_seq   -> Int8,
        mgmt_fee_bps_annual      -> Nullable<Int8>,
        perf_fee_bps             -> Nullable<Int8>,
        round_ms                 -> Nullable<Int8>,
        selling_window_ms        -> Nullable<Int8>,
        min_strike_bps_over_spot -> Nullable<Int8>,
        max_strike_bps_over_spot -> Nullable<Int8>,
    }
}

diesel::table! {
    vault_rounds (vault_id, round) {
        vault_id          -> Text,
        round             -> Int8,
        bucket_id         -> Nullable<Text>,
        strike            -> Nullable<Numeric>,
        strike_scale      -> Nullable<Int2>,
        expiry_ms         -> Nullable<Int8>,
        pps               -> Nullable<Numeric>,
        aum               -> Nullable<Numeric>,
        shares            -> Nullable<Numeric>,
        premium_collected -> Nullable<Numeric>,
        mgmt_fee          -> Nullable<Numeric>,
        perf_fee          -> Nullable<Numeric>,
        finalized_at_ms   -> Nullable<Int8>,
        updated_at_seq    -> Int8,
    }
}

diesel::table! {
    vault_user_receipts (vault_id, owner, round, kind) {
        vault_id       -> Text,
        owner          -> Text,
        round          -> Int8,
        kind           -> Text,
        amount         -> Numeric,
        settled        -> Numeric,
        updated_at_seq -> Int8,
    }
}

diesel::table! {
    trading_vaults (vault_id) {
        vault_id            -> Text,
        accounting_asset    -> Text,
        creator             -> Text,
        curator             -> Text,
        curator_cap_id      -> Text,
        state               -> Text,
        lockup_ms           -> Int8,
        curator_fee_bps     -> Int8,
        unwind_grace_ms     -> Int8,
        deposits_paused     -> Bool,
        mm_release_enabled  -> Bool,
        total_shares        -> Numeric,
        position_count      -> Int8,
        pending_withdrawals -> Int8,
        latest_pps_e12      -> Nullable<Numeric>,
        updated_at_seq      -> Int8,
        updated_at_ms       -> Int8,
        external_account    -> Nullable<Text>,
        external_exposure   -> Int8,
        latest_external_equity        -> Nullable<Int8>,
        external_equity_updated_at_ms -> Nullable<Int8>,
        latest_nav          -> Nullable<Numeric>,
        nav_updated_at_ms   -> Nullable<Int8>,
    }
}

diesel::table! {
    trading_vault_positions (vault_id, position_id) {
        vault_id       -> Text,
        position_id    -> Text,
        adapter        -> Text,
        active         -> Bool,
        stored_at_ms   -> Int8,
        removed_at_ms  -> Nullable<Int8>,
        updated_at_seq -> Int8,
        last_value           -> Nullable<Int8>,
        last_appraised_at_ms -> Nullable<Int8>,
    }
}

diesel::table! {
    event_participants (sequence, address, role) {
        sequence -> Int8,
        address  -> Text,
        role     -> Text,
    }
}

diesel::joinable!(event_participants -> indexed_events (sequence));
diesel::joinable!(bucket_deepbook_pools -> buckets (bucket_id));
diesel::joinable!(rfq_bids -> indexed_events (sequence));
diesel::allow_tables_to_appear_in_same_query!(
    event_participants,
    indexer_progress,
    indexed_events,
    accounts,
    buckets,
    positions,
    bucket_deepbook_pools,
    rfqs,
    rfq_bids,
    vaults,
    vault_rounds,
    vault_user_receipts,
    trading_vaults,
    trading_vault_positions,
);
