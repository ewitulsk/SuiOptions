//! Diesel table definitions. Hand-written to match `migrations/000001_init`.

diesel::table! {
    assets (id) {
        id -> Int4,
        symbol -> Text,
        network_id -> Text,
        onramp_enabled -> Bool,
        offramp_enabled -> Bool,
        swap_enabled -> Bool,
        sort_order -> Int4,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    fee_schedule (id) {
        id -> Int4,
        source -> Text,
        transfer_fee_bps -> Nullable<Int4>,
        ach_fee_cents -> Nullable<Int4>,
        wire_fee_cents -> Nullable<Int4>,
        sepa_fee_cents -> Nullable<Int4>,
        swift_fee_cents -> Nullable<Int4>,
        kyc_fee_cents -> Nullable<Int4>,
        kyb_fee_cents -> Nullable<Int4>,
        effective_from -> Timestamptz,
        fetched_at -> Timestamptz,
        note -> Nullable<Text>,
    }
}

diesel::table! {
    customers (dakota_customer_id) {
        dakota_customer_id -> Text,
        customer_type -> Text,
        is_sub_client -> Bool,
        sub_client_id -> Nullable<Text>,
        external_ref -> Nullable<Text>,
        application_id -> Nullable<Text>,
        kyb_status -> Nullable<Text>,
        kyc_status -> Nullable<Text>,
        application_status -> Nullable<Text>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    accounts (dakota_account_id) {
        dakota_account_id -> Text,
        dakota_customer_id -> Text,
        account_type -> Text,
        source_asset -> Nullable<Text>,
        source_network_id -> Nullable<Text>,
        destination_asset -> Nullable<Text>,
        destination_network_id -> Nullable<Text>,
        rail -> Nullable<Text>,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    ledger_events (event_id) {
        event_id -> Text,
        event_type -> Text,
        resource_type -> Nullable<Text>,
        resource_id -> Nullable<Text>,
        dakota_customer_id -> Nullable<Text>,
        direction -> Nullable<Text>,
        amount_minor -> Nullable<Int8>,
        asset -> Nullable<Text>,
        exchange_rate -> Nullable<Text>,
        fee_minor -> Nullable<Int8>,
        status -> Nullable<Text>,
        occurred_at -> Nullable<Timestamptz>,
        received_at -> Timestamptz,
    }
}

diesel::table! {
    wallets (dakota_wallet_id) {
        dakota_wallet_id -> Text,
        address -> Nullable<Text>,
        family -> Text,
        signer_group_id -> Nullable<Text>,
        policy_id -> Nullable<Text>,
        label -> Nullable<Text>,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    webhook_errors (id) {
        id -> Int4,
        event_id -> Nullable<Text>,
        reason -> Text,
        body_sha256 -> Text,
        received_at -> Timestamptz,
    }
}

diesel::joinable!(accounts -> customers (dakota_customer_id));
diesel::allow_tables_to_appear_in_same_query!(
    assets,
    fee_schedule,
    customers,
    accounts,
    ledger_events,
    wallets,
    webhook_errors
);
