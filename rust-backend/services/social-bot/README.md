# social-bot

Slack + Discord slash-command bots in one deployment. Allow-listed users post
tweets from any account twitter-service manages:

```
/tweet <account> <text…>
```

Both bots are plain signed HTTP webhooks (no gateway/socket connections), so
the one axum server behind nginx serves both platforms:

- `POST /slack/command` — Slack slash command, verified with the app signing
  secret (HMAC-SHA256).
- `POST /discord/interactions` — Discord interactions endpoint, verified with
  the application public key (Ed25519).
- `GET /health`

Public staging URLs (nginx strips the prefix):

```
https://<alb-host>/staging/social-bot/slack/command
https://<alb-host>/staging/social-bot/discord/interactions
```

## Who can post

The allow lists live in config (`config.staging.toml`), not secrets — user ids
aren't sensitive and changing them is an ordinary config deploy:

```toml
slack_allowed_user_ids   = ["U0123456789"]
discord_allowed_user_ids = ["123456789012345678"]
```

Empty lists mean nobody can post. Allow-listed users may post from **any**
configured Twitter account (`GET twitter-service /accounts`).

## One-time platform setup

### Slack

1. Create an app at api.slack.com/apps (from scratch, your workspace).
2. Slash Commands → Create New Command: command `/tweet`, request URL
   `https://<alb-host>/staging/social-bot/slack/command`, usage hint
   `<account> <text>`.
3. Install the app to the workspace.
4. Basic Information → App Credentials → copy the **Signing Secret** into the
   `options/staging/social-bot` AWS secret (`slack_signing_secret`).

### Discord

1. Create an application at discord.com/developers/applications.
2. General Information → copy the **Public Key** into the
   `options/staging/social-bot` AWS secret (`discord_public_key`).
3. Register the slash command (once per application; needs the bot token):

   ```sh
   curl -X POST "https://discord.com/api/v10/applications/<APP_ID>/commands" \
     -H "Authorization: Bot <BOT_TOKEN>" -H "Content-Type: application/json" \
     -d '{
       "name": "tweet",
       "description": "Post a tweet from one of our accounts",
       "options": [
         {"type": 3, "name": "account", "description": "Account to post from", "required": true},
         {"type": 3, "name": "text", "description": "Tweet text", "required": true}
       ]
     }'
   ```

4. Set General Information → **Interactions Endpoint URL** to
   `https://<alb-host>/staging/social-bot/discord/interactions`. Discord
   verifies the endpoint with a signed PING on save, so deploy social-bot
   (with the real public key in the secret) first.
5. Install the app to your server (Installation → Guild Install).

The bot token is only needed for the one-time command registration above; the
service itself never uses it (interaction webhooks authenticate with the
interaction token Discord sends in each request).
