//! Hand-written equivalent of what `diesel print-schema` would generate.
//! Kept in sync with `migrations/000001_init/up.sql`.

diesel::table! {
    indexer_progress (id) {
        id             -> Int2,
        last_slot      -> Int8,
        finalized_slot -> Int8,
        updated_at     -> Timestamptz,
    }
}

diesel::table! {
    indexed_events (sequence) {
        sequence       -> Int8,
        slot           -> Int8,
        signature      -> Text,
        tx_index       -> Int8,
        inner_ix_index -> Int4,
        program        -> Text,
        timestamp_ms   -> Int8,
        event_type     -> Text,
        payload        -> Jsonb,
    }
}

diesel::table! {
    event_participants (sequence, address, role) {
        sequence -> Int8,
        address  -> Text,
        role     -> Text,
    }
}

diesel::table! {
    accounts (account_id) {
        account_id      -> Text,
        owner           -> Text,
        signing_scheme  -> Int2,
        signing_pubkey  -> Bytea,
        updated_at_slot -> Int8,
    }
}

diesel::table! {
    account_balances (account_id, mint) {
        account_id      -> Text,
        mint            -> Text,
        balance         -> Numeric,
        updated_at_slot -> Int8,
    }
}

diesel::table! {
    buckets (bucket_id) {
        bucket_id       -> Text,
        underlying_mint -> Text,
        settlement_mint -> Text,
        option_mint     -> Text,
        option_kind     -> Text,
        strike          -> Numeric,
        strike_scale    -> Int2,
        expiry_ms       -> Int8,
        total_written   -> Numeric,
        exercise_cursor -> Numeric,
        cleaned         -> Bool,
        invalidated     -> Bool,
        updated_at_slot -> Int8,
    }
}

diesel::table! {
    positions (position_id) {
        position_id      -> Text,
        bucket_id        -> Text,
        range_start      -> Numeric,
        range_end        -> Numeric,
        recipient        -> Text,
        option_kind      -> Text,
        premium_received -> Numeric,
        mm_account_id    -> Nullable<Text>,
        signature        -> Text,
        minted_at_ms     -> Int8,
        updated_at_slot  -> Int8,
    }
}

diesel::table! {
    auctions (auction_id) {
        auction_id        -> Text,
        mode              -> Text,
        bucket_id         -> Nullable<Text>,
        creator           -> Text,
        escrow_mint       -> Text,
        bid_mint          -> Text,
        amount            -> Numeric,
        notional          -> Numeric,
        reserve_bid       -> Numeric,
        deadline_ms       -> Int8,
        max_deadline_ms   -> Int8,
        min_increment_bps -> Int8,
        settle_authority  -> Nullable<Text>,
        best_bid          -> Nullable<Numeric>,
        best_bidder       -> Nullable<Text>,
        status            -> Text,
        winner            -> Nullable<Text>,
        token_recipient   -> Nullable<Text>,
        position_id       -> Nullable<Text>,
        gross_bid         -> Nullable<Numeric>,
        fee               -> Nullable<Numeric>,
        net_proceeds      -> Nullable<Numeric>,
        bid_refunded      -> Nullable<Bool>,
        updated_at_slot   -> Int8,
    }
}

diesel::table! {
    auction_bids (auction_id, sequence) {
        auction_id      -> Text,
        sequence        -> Int8,
        bidder          -> Text,
        token_recipient -> Text,
        bid             -> Numeric,
        previous_bid    -> Numeric,
        deadline_ms     -> Int8,
    }
}

diesel::table! {
    vaults (vault_id) {
        vault_id                 -> Text,
        underlying_mint          -> Text,
        settlement_mint          -> Text,
        share_mint               -> Text,
        round                    -> Int8,
        current_bucket           -> Nullable<Text>,
        latest_pps               -> Nullable<Numeric>,
        total_shares             -> Numeric,
        pending_deposits         -> Numeric,
        deposits_paused          -> Bool,
        mgmt_fee_bps_annual      -> Nullable<Int8>,
        perf_fee_bps             -> Nullable<Int8>,
        round_ms                 -> Nullable<Int8>,
        selling_window_ms        -> Nullable<Int8>,
        min_strike_bps_over_spot -> Nullable<Int8>,
        max_strike_bps_over_spot -> Nullable<Int8>,
        updated_at_slot          -> Int8,
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
        selling_ends_ms   -> Nullable<Int8>,
        spot              -> Nullable<Numeric>,
        spot_scale        -> Nullable<Int2>,
        pps               -> Nullable<Numeric>,
        aum               -> Nullable<Numeric>,
        shares            -> Nullable<Numeric>,
        premium_collected -> Nullable<Numeric>,
        mgmt_fee          -> Nullable<Numeric>,
        perf_fee          -> Nullable<Numeric>,
        finalized_at_ms   -> Nullable<Int8>,
        updated_at_slot   -> Int8,
    }
}

diesel::table! {
    vault_receipts (vault_id, owner, round, kind) {
        vault_id        -> Text,
        owner           -> Text,
        round           -> Int8,
        kind            -> Text,
        amount          -> Numeric,
        settled         -> Numeric,
        updated_at_slot -> Int8,
    }
}

diesel::joinable!(account_balances -> accounts (account_id));
diesel::joinable!(event_participants -> indexed_events (sequence));
diesel::allow_tables_to_appear_in_same_query!(
    indexer_progress,
    indexed_events,
    event_participants,
    accounts,
    account_balances,
    buckets,
    positions,
    auctions,
    auction_bids,
    vaults,
    vault_rounds,
    vault_receipts,
);
