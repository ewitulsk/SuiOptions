# airdrop-bot

Discord slash-command bot for the engagement airdrop. Deliberately a
**separate Discord application + deployment from social-bot** (the tweeting
bot): different audience (community server vs team), different blast radius,
no shared secrets.

Commands are read-only, served from engagement-service — no allow list:

```
/leaderboard [count]   — top authors by airdrop points (default 10, max 25)
/points <handle>       — one twitter handle's points + rank
```

Discord delivers commands as signed HTTP webhooks (no gateway/socket
connection), so one axum server behind nginx serves everything:

- `POST /discord/interactions` — verified with the application public key
  (Ed25519).
- `GET /health`

Public staging URL (nginx strips the prefix):

```
https://<alb-host>/staging/airdrop-bot/discord/interactions
```

## One-time platform setup

1. Create a NEW application at discord.com/developers/applications (do not
   reuse social-bot's).
2. General Information → copy the **Public Key** into the
   `options/staging/airdrop-bot` AWS secret (`discord_public_key`).
3. Register the slash commands (once per application; needs the bot token):

   ```sh
   curl -X PUT "https://discord.com/api/v10/applications/<APP_ID>/commands" \
     -H "Authorization: Bot <BOT_TOKEN>" -H "Content-Type: application/json" \
     -d '[
       {
         "name": "leaderboard",
         "description": "Top authors by airdrop points",
         "options": [
           {"type": 4, "name": "count", "description": "How many entries (max 25)", "required": false}
         ]
       },
       {
         "name": "points",
         "description": "Airdrop points for a twitter handle",
         "options": [
           {"type": 3, "name": "handle", "description": "Twitter handle (with or without @)", "required": true}
         ]
       }
     ]'
   ```

4. Set General Information → **Interactions Endpoint URL** to
   `https://<alb-host>/staging/airdrop-bot/discord/interactions`. Discord
   verifies the endpoint with a signed PING on save, so deploy airdrop-bot
   (with the real public key in the secret) first.
5. Install the app to the community server (Installation → Guild Install).

The bot token is only needed for the one-time command registration above;
the service itself never uses it (interaction webhooks authenticate with the
interaction token Discord sends in each request).
