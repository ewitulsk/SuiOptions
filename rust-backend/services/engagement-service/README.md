# engagement-service

Tracks engagement on tweets that mention our account and converts it into
airdrop points. One service owns the whole pipeline (MVP):

```
twitter-service ──GET /mentions──▶ poller ──▶ Postgres (tracked_tweets)
                ◀─GET /tweets/metrics──┘           │
                                                   ▼
airdrop-bot ◀──GET /leaderboard, /points/{handle}──┘
```

- **Tracking** — every `poll_interval_secs` (default 300) the poller asks
  twitter-service for new tweets mentioning `twitter_account` (Twitter
  recent search: original tweets only, last 7 days) and upserts them with
  their like/retweet/reply/quote counters. It also refreshes the stalest
  counters among tweets younger than `refresh_max_age_hours` (default 168),
  100 per tick.
- **Airdrop points** — derived at read time from `[points]` config weights
  (never stored), so re-tuning weights or the ambassador roster is an
  ordinary config deploy with no backfill:

  ```
  engagement_points = likes*like_weight + replies*reply_weight
                    + retweets*retweet_weight + quotes*quote_weight
  airdrop_points    = engagement_points * airdrop_points_per_engagement_point
                    * (ambassador ? ambassador_multiplier : 1)
  ```

  Ambassadors are twitter handles in `points.ambassadors` (config, not
  secrets). Everyone else is "general public" — same leaderboard, no
  multiplier.
- **Leaderboard** — authors ranked by airdrop points.

Internal-only (never proxied by nginx); airdrop-bot is the public surface.

## Endpoints

- `GET /health`
- `GET /leaderboard?limit=N` — ranked entries (default 10, max 100):
  `{rank, handle, ambassador, tweets, likes, retweets, replies, quotes,
  engagement_points, airdrop_points}`
- `GET /points/{handle}` — one handle's entry + rank (404 when untracked).
  Handles are lowercased at ingest; lookups are case-insensitive and a
  leading `@` is accepted.

## Persistence

Postgres via diesel with embedded migrations (same shape as
price-charting). One row per tweet with its latest counters — no snapshot
history in the MVP. One-time provisioning per env (same convention as the
other DB-backed services):

```sql
CREATE DATABASE engagement_staging;
CREATE USER engagement_staging WITH PASSWORD '<env db password>';
GRANT ALL PRIVILEGES ON DATABASE engagement_staging TO engagement_staging;
```

## Not in the MVP (deliberately)

- Per-tweet snapshot history / engagement trend charts.
- Splitting airdrop conversion into its own service — it's a pure function
  in `src/points.rs`; split it out when the airdrop program needs its own
  state (claims, epochs, on-chain distribution).
- Follower-count weighting, spam/bot filtering beyond `-is:retweet`, and
  mention search past Twitter's 7-day recent-search window.
