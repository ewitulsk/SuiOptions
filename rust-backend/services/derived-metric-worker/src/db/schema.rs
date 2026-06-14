// Diesel table for the predicted-APY snapshots. One row per
// (vault, kind, horizon); the worker upserts the latest snapshot each tick.

diesel::table! {
    vault_predictions (vault_id, kind, horizon) {
        vault_id -> Text,
        kind -> Text,
        horizon -> Int4,
        t_ms -> Int8,
        apy -> Float8,
        confidence -> Float8,
        computed_at -> Timestamptz,
    }
}
