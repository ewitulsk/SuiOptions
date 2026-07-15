//! Hand-written diesel schema; kept in sync with `migrations/`.

diesel::table! {
    cctp_transfers (id) {
        id                 -> Int8,
        origin_chain       -> Text,
        origin_tx_hash     -> Text,
        origin_wallet      -> Text,
        destination_wallet -> Nullable<Text>,
        mint_recipient     -> Nullable<Text>,
        amount             -> Nullable<Numeric>,
        status             -> Text,
        message_hex        -> Nullable<Text>,
        attestation_hex    -> Nullable<Text>,
        mint_tx_hash       -> Nullable<Text>,
        error              -> Nullable<Text>,
        attempts           -> Int4,
        burned_at          -> Nullable<Timestamptz>,
        attested_at        -> Nullable<Timestamptz>,
        minted_at          -> Nullable<Timestamptz>,
        created_at         -> Timestamptz,
        updated_at         -> Timestamptz,
    }
}
