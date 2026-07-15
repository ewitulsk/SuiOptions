//! Diesel row types.

use chrono::{DateTime, Utc};
use diesel::prelude::*;

use super::schema::{poll_cursor, tracked_tweets};

#[derive(Insertable, Queryable, Debug, Clone)]
#[diesel(table_name = tracked_tweets)]
pub struct TweetRow {
    pub tweet_id: String,
    pub author_id: String,
    pub author_handle: String,
    pub text: String,
    pub tweet_created_at: DateTime<Utc>,
    pub first_seen_at: DateTime<Utc>,
    pub metrics_updated_at: DateTime<Utc>,
    pub likes: i64,
    pub retweets: i64,
    pub replies: i64,
    pub quotes: i64,
}

#[derive(Insertable, Queryable, AsChangeset, Debug, Clone)]
#[diesel(table_name = poll_cursor)]
pub struct CursorRow {
    pub id: i16,
    pub since_id: String,
    pub updated_at: DateTime<Utc>,
}
