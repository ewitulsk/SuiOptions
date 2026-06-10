//! Hand-written equivalent of what `diesel print-schema` would generate.
//! Kept in sync with the migrations.

diesel::table! {
    supported_tokens (coin_type) {
        coin_type    -> Text,
        ticker       -> Text,
        name         -> Text,
        logo_uri     -> Nullable<Text>,
        decimals     -> Int2,
        pyth_feed_id -> Nullable<Text>,
        enabled      -> Bool,
        created_at   -> Timestamptz,
        updated_at   -> Timestamptz,
    }
}

diesel::table! {
    verified_orgs (org_id) {
        org_id     -> Text,
        name       -> Text,
        enabled    -> Bool,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}
