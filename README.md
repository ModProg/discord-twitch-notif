# discord-twitch-notif

[![CI Status](https://github.com/ModProg/discord-twitch-notif/actions/workflows/test.yaml/badge.svg)](https://github.com/ModProg/discord-twitch-notif/actions/workflows/test.yaml)

[Add to your DISCORD server.](https://discord.com/api/oauth2/authorize?client_id=1163116705027993670&scope=applications.commands) 

Use with `/subscribe <streamer_name>`, `/subscriptions`, `/unsubscribe <streamer_name>`.

Run your own with docker:

```yaml
volumnes:
  discord-twitch-notif:
    name: discord-twitch-notif
services:
  discord-twitch-notif:
    image: ghcr.io/modprog/discord-twitch-notif:latest
    environment:
      - DISCORD_TOKEN=
      - TWITCH_CLIENT_ID=
      - TWITCH_CLIENT_SECRET=
      - TWITCH_WEBHOOK_PORT=8000
      - TWITCH_WEBHOOK_PATH=/twitch-events
      - TWITCH_WEBHOOK_URL=https://dtn.example.com/twitch-events
    ports:
      - 127.0.0.1:1234:8000
    volumnes:
      - discord-twitch-notif:/user_data.bonsaidb
```
