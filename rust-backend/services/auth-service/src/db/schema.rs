//! Diesel table definitions. Hand-written to match `migrations/000001_init`.

diesel::table! {
    users (id) {
        id -> Uuid,
        role -> Text,
        scope_id -> Nullable<Text>,
        created_at -> Timestamptz,
        disabled_at -> Nullable<Timestamptz>,
    }
}

diesel::table! {
    identities (id) {
        id -> Uuid,
        user_id -> Uuid,
        kind -> Text,
        identifier -> Text,
        secret_hash -> Nullable<Text>,
        metadata -> Nullable<Jsonb>,
        verified_at -> Nullable<Timestamptz>,
        created_at -> Timestamptz,
        last_used_at -> Nullable<Timestamptz>,
    }
}

diesel::table! {
    invites (id) {
        id -> Uuid,
        role -> Text,
        scope_id -> Nullable<Text>,
        created_by -> Nullable<Uuid>,
        label -> Nullable<Text>,
        expires_at -> Timestamptz,
        consumed_at -> Nullable<Timestamptz>,
        consumed_by -> Nullable<Uuid>,
        created_at -> Timestamptz,
    }
}

diesel::joinable!(identities -> users (user_id));
diesel::allow_tables_to_appear_in_same_query!(users, identities, invites);
