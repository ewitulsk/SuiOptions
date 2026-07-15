//! Hand-written diesel schema; kept in sync with `migrations/`.

// One row per tweet mentioning the account, carrying the LATEST engagement
// counters (the leaderboard needs totals, not history — snapshots can be
// added later if trend charts are wanted).
diesel::table! {
    tracked_tweets (tweet_id) {
        tweet_id           -> Text,
        author_id          -> Text,
        // Lowercased at ingest — twitter handles are case-insensitive.
        author_handle      -> Text,
        text               -> Text,
        tweet_created_at   -> Timestamptz,
        first_seen_at      -> Timestamptz,
        metrics_updated_at -> Timestamptz,
        likes              -> Int8,
        retweets           -> Int8,
        replies            -> Int8,
        quotes             -> Int8,
    }
}

// since_id cursor for the mention search (singleton row, id = 1).
diesel::table! {
    poll_cursor (id) {
        id         -> Int2,
        since_id   -> Text,
        updated_at -> Timestamptz,
    }
}

diesel::allow_tables_to_appear_in_same_query!(tracked_tweets, poll_cursor);
