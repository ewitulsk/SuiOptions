//! Repository over the engagement DB.

use anyhow::{Context, Result};
use chrono::Utc;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, PooledConnection};
use diesel::sql_types::{BigInt, Text};

use crate::points::Engagement;

use super::models::{CursorRow, TweetRow};
use super::schema::{poll_cursor, tracked_tweets};
use super::DbPool;

#[derive(Clone)]
pub struct Repo {
    pool: std::sync::Arc<DbPool>,
}

/// Per-author engagement totals across every tracked tweet.
#[derive(QueryableByName, Debug, Clone)]
pub struct AuthorTotals {
    #[diesel(sql_type = Text)]
    pub author_handle: String,
    #[diesel(sql_type = BigInt)]
    pub tweets: i64,
    #[diesel(sql_type = BigInt)]
    pub likes: i64,
    #[diesel(sql_type = BigInt)]
    pub retweets: i64,
    #[diesel(sql_type = BigInt)]
    pub replies: i64,
    #[diesel(sql_type = BigInt)]
    pub quotes: i64,
}

impl AuthorTotals {
    pub fn engagement(&self) -> Engagement {
        Engagement {
            likes: self.likes,
            retweets: self.retweets,
            replies: self.replies,
            quotes: self.quotes,
        }
    }
}

impl Repo {
    pub fn new(pool: std::sync::Arc<DbPool>) -> Self {
        Self { pool }
    }

    fn conn(&self) -> Result<PooledConnection<ConnectionManager<PgConnection>>> {
        self.pool.get().context("checking out DB connection")
    }

    pub fn load_since_id(&self) -> Result<Option<String>> {
        let mut conn = self.conn()?;
        let row = poll_cursor::table
            .find(1i16)
            .first::<CursorRow>(&mut conn)
            .optional()
            .context("loading poll_cursor")?;
        Ok(row.map(|r| r.since_id))
    }

    /// Upsert a mention batch (replays refresh the counters) and advance the
    /// since_id cursor in one transaction. Returns rows written.
    pub fn upsert_mentions(&self, rows: &[TweetRow], newest_id: Option<&str>) -> Result<usize> {
        let mut conn = self.conn()?;
        conn.transaction::<_, anyhow::Error, _>(|conn| {
            let mut written = 0;
            for row in rows {
                written += diesel::insert_into(tracked_tweets::table)
                    .values(row)
                    .on_conflict(tracked_tweets::tweet_id)
                    .do_update()
                    .set((
                        tracked_tweets::likes.eq(row.likes),
                        tracked_tweets::retweets.eq(row.retweets),
                        tracked_tweets::replies.eq(row.replies),
                        tracked_tweets::quotes.eq(row.quotes),
                        tracked_tweets::metrics_updated_at.eq(row.metrics_updated_at),
                    ))
                    .execute(conn)
                    .context("upserting tracked_tweets")?;
            }
            if let Some(newest) = newest_id {
                diesel::insert_into(poll_cursor::table)
                    .values(CursorRow {
                        id: 1,
                        since_id: newest.to_string(),
                        updated_at: Utc::now(),
                    })
                    .on_conflict(poll_cursor::id)
                    .do_update()
                    .set((
                        poll_cursor::since_id.eq(newest),
                        poll_cursor::updated_at.eq(Utc::now()),
                    ))
                    .execute(conn)
                    .context("advancing poll_cursor")?;
            }
            Ok(written)
        })
    }

    /// Tweet ids still young enough to refresh, stalest counters first.
    pub fn refresh_candidates(&self, max_age_hours: i64, limit: i64) -> Result<Vec<String>> {
        #[derive(QueryableByName)]
        struct IdRow {
            #[diesel(sql_type = Text)]
            tweet_id: String,
        }

        let mut conn = self.conn()?;
        // make_interval only accepts int4 args, hence the explicit cast.
        let rows = diesel::sql_query(
            "SELECT tweet_id FROM tracked_tweets \
             WHERE tweet_created_at > now() - make_interval(hours => $1::int4) \
             ORDER BY metrics_updated_at ASC LIMIT $2",
        )
        .bind::<BigInt, _>(max_age_hours)
        .bind::<BigInt, _>(limit)
        .load::<IdRow>(&mut conn)
        .context("querying refresh candidates")?;
        Ok(rows.into_iter().map(|r| r.tweet_id).collect())
    }

    /// Overwrite counters for refreshed tweets.
    pub fn update_metrics(&self, updates: &[(String, Engagement)]) -> Result<()> {
        let mut conn = self.conn()?;
        let now = Utc::now();
        for (tweet_id, e) in updates {
            diesel::update(tracked_tweets::table.find(tweet_id))
                .set((
                    tracked_tweets::likes.eq(e.likes),
                    tracked_tweets::retweets.eq(e.retweets),
                    tracked_tweets::replies.eq(e.replies),
                    tracked_tweets::quotes.eq(e.quotes),
                    tracked_tweets::metrics_updated_at.eq(now),
                ))
                .execute(&mut conn)
                .context("updating tweet metrics")?;
        }
        Ok(())
    }

    /// Engagement totals per author, across every tracked tweet.
    pub fn author_totals(&self) -> Result<Vec<AuthorTotals>> {
        let mut conn = self.conn()?;
        diesel::sql_query(
            "SELECT author_handle, \
                    count(*)                    AS tweets, \
                    coalesce(sum(likes), 0)::int8    AS likes, \
                    coalesce(sum(retweets), 0)::int8 AS retweets, \
                    coalesce(sum(replies), 0)::int8  AS replies, \
                    coalesce(sum(quotes), 0)::int8   AS quotes \
             FROM tracked_tweets GROUP BY author_handle",
        )
        .load::<AuthorTotals>(&mut conn)
        .context("querying author totals")
    }
}
