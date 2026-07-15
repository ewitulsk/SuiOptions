# Engagement → Airdrop Leaderboard

Plan + MVP scope for the engagement/airdrop program: track engagement on
tweets that mention us, convert it into airdrop points, rank ambassadors and
the general public on a leaderboard, and expose it all through a Discord bot.

## Architecture

```
                       Twitter API v2 (OAuth 1.0a, per-account)
                                      ▲
                                      │ POST /2/tweets            (existing)
                                      │ GET  /2/tweets/search/recent   (new)
                                      │ GET  /2/tweets?ids=…           (new)
                              ┌───────┴───────┐
                              │ twitter-service│  internal :9014
                              └───┬───────▲───┘
                 GET /mentions    │       │   (also: social-bot /tweet,
                 GET /tweets/metrics      │    unchanged)
                              ┌───▼───────┴───┐
                              │ engagement-    │  internal :9017
                              │ service        │──▶ Postgres engagement_<env>
                              │  poll loop     │     (tracked_tweets,
                              │  points module │      poll_cursor)
                              └───┬───────────┘
              GET /leaderboard    │  GET /points/{handle}
                              ┌───▼───────────┐
                              │ airdrop-bot    │  public :9018
                              │ (Discord app,  │  nginx /<env>/airdrop-bot/
                              │  separate from │
                              │  social-bot)   │
                              └───────────────┘
```

Four pieces from the original sketch, mapped to what shipped:

1. **Engagement tracking** — twitter-service grew two signed read endpoints
   (`GET /mentions`, `GET /tweets/metrics`); it stays the single owner of
   Twitter credentials. engagement-service polls it every 5 minutes: new
   mentions of `@suioptions` (original tweets only, no retweets, last 7
   days — Twitter's recent-search window) are upserted into Postgres, and
   the stalest counters among tweets younger than 7 days are refreshed 100
   at a time.
2. **Airdrop conversion** — a pure module inside engagement-service, not a
   separate service (see "Decisions"). Config weights per like/reply/
   retweet/quote → engagement points; a rate + ambassador multiplier →
   airdrop points. Derived at read time, so tuning weights or the
   ambassador roster is a config deploy, no backfill.
3. **Leaderboard** — `GET /leaderboard` on engagement-service: authors
   (keyed by twitter handle) ranked by airdrop points; ambassadors are
   flagged, the general public competes on the same board.
4. **Discord bot** — airdrop-bot, a separate Discord application and
   container from social-bot (the team tweeting bot): community-facing,
   read-only `/leaderboard [count]` and `/points <handle>` commands, no
   allow list, no shared secrets.

## Decisions (and why)

- **One service for tracking + conversion + leaderboard.** The partner
  sketch has engagement-tracking and airdrop-tracking as two services, but
  the conversion is a pure function over the tracked totals — a second
  service would add a network hop, a deployment and a DB with no state of
  its own. `points.rs` keeps the boundary; split it out when the airdrop
  needs real state (claims, epochs, on-chain distribution).
- **Points are derived, never stored.** Only raw counters live in the DB.
- **Handles, not user ids, key the leaderboard.** Simpler to read, matches
  the ambassador roster in config; `author_id` is stored per tweet so we
  can re-key later if handle changes ever matter.
- **airdrop-bot is webhook-based like social-bot** (no gateway connection):
  one axum server, Ed25519-verified interactions, defer + follow-up within
  Discord's 3s ack window.
- **Staging-only for now**, like twitter-service/social-bot — deliberately
  not declared in docker-compose.prod.yml.

## MVP scoping (not built, by design)

- Engagement history/snapshots (trend charts), follower-weighting,
  spam/bot filtering beyond excluding retweets.
- Mentions older than Twitter's 7-day recent-search window: the poll cursor
  makes this moot once the service is running continuously.
- Claim flow / on-chain distribution — the leaderboard is the deliverable;
  distribution is a later phase with its own design.
- Discord↔Twitter account linking (`/register`): points accrue to twitter
  handles; anyone can query any handle.

## Rollout (staging)

1. Terraform apply (new `options/staging/airdrop-bot` secret placeholder).
2. Provision the DB (one-time, infra/README.md convention):
   `CREATE DATABASE engagement_staging; CREATE USER engagement_staging …`.
3. Fill the airdrop-bot secret with the new Discord application's public
   key; register the slash commands (services/airdrop-bot/README.md).
4. Deploy `engagement-service` + `airdrop-bot` (+ rebuilt
   `twitter-service`); point the Discord app's Interactions Endpoint URL at
   `https://<alb-host>/staging/airdrop-bot/discord/interactions`.
5. Set `points.ambassadors` in engagement-service's config as the
   ambassador roster firms up.
