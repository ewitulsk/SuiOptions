-- Tweets mentioning our account, with their latest engagement counters.
CREATE TABLE tracked_tweets (
    tweet_id           TEXT PRIMARY KEY,
    author_id          TEXT NOT NULL,
    author_handle      TEXT NOT NULL,
    text               TEXT NOT NULL,
    tweet_created_at   TIMESTAMPTZ NOT NULL,
    first_seen_at      TIMESTAMPTZ NOT NULL,
    metrics_updated_at TIMESTAMPTZ NOT NULL,
    likes              INT8 NOT NULL DEFAULT 0,
    retweets           INT8 NOT NULL DEFAULT 0,
    replies            INT8 NOT NULL DEFAULT 0,
    quotes             INT8 NOT NULL DEFAULT 0
);

-- Leaderboard groups by author; refresh scans by age/staleness.
CREATE INDEX tracked_tweets_author_handle_idx ON tracked_tweets (author_handle);
CREATE INDEX tracked_tweets_refresh_idx
    ON tracked_tweets (tweet_created_at, metrics_updated_at);

-- since_id cursor for the mention search (singleton row, id = 1).
CREATE TABLE poll_cursor (
    id         INT2 PRIMARY KEY,
    since_id   TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
