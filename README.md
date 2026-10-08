# VoltBot

VoltBot is a Discord bot written in Rust. It is a rewrite of [voltgpt](https://github.com/cherryroots/voltgpt), the Go bot behind "Vivy", ported one feature at a time.

## Goals

- **Event driven.** Each feature is its own module that listens to the Discord events it cares about, with guards deciding when it runs.
- **Provider agnostic.** AI chat sits behind one trait, so OpenAI, Claude or Gemini can be swapped in.
- **Simple and readable.** The code should be easy to study for someone learning Rust.

## Stack

- [serenity](https://github.com/serenity-rs/serenity) and [poise](https://github.com/serenity-rs/poise) for Discord
- [tokio](https://tokio.rs) as the async runtime
- SQLite for storage

## Status

Planning. See `docs/feature-map.md` for the features being ported and the order. The setup steps below describe how the bot will run once stage 1 lands.

## Setup

### 1. Create the Discord application

1. Create an application at <https://discord.com/developers/applications> and add a bot to it.
2. Under **Bot**, turn on the **Message Content Intent** and copy the token.
3. Invite the bot with the `bot` and `applications.commands` scopes and these permissions: View Channels, Send Messages, Send Messages in Threads, Embed Links, Attach Files, Read Message History, Add Reactions.
4. Create two private channels for the control panel, for example `#bot-logs` and `#bot-status`, and copy their IDs (turn on Developer Mode in Discord, then right-click the channel).

### 2. Build

You need [Rust](https://rustup.rs) (stable) and `ffmpeg` (used to read video frames).

```bash
git clone https://github.com/cherryroots/VoltBot.git
cd VoltBot
cargo build --release
```

The binary is `target/release/voltbot`.

### 3. Install

```bash
sudo useradd --system --home /opt/voltbot --shell /usr/sbin/nologin voltbot
sudo mkdir -p /opt/voltbot
sudo cp target/release/voltbot /opt/voltbot/
sudo cp config.example.toml /opt/voltbot/config.toml
sudo cp .env.example /opt/voltbot/.env
sudo chown -R voltbot:voltbot /opt/voltbot
sudo chmod 600 /opt/voltbot/.env
```

Fill in `/opt/voltbot/.env` (secrets: `DISCORD_TOKEN`, `OPENAI_TOKEN`, optional `SENTRY_DSN`) and `/opt/voltbot/config.toml` (admin user IDs, the control panel channel IDs, and per-feature settings).

**Coming from voltgpt:** copy its `voltgpt.db` to `/opt/voltbot/old.db` before the first start. The bot imports reminders and the movie wheel game, then renames the file to `old.db.imported`.

### 4. Run with systemd

```bash
sudo cp deploy/voltbot.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now voltbot
```

### 5. Check on it

- **Discord:** the status channel has a message that refreshes every minute with uptime, errors and per-feature stats, and the log channel gets warnings, errors, and start and stop notices.
- **Service state:** `systemctl status voltbot`
- **Full logs:** `journalctl -u voltbot -f`. Set `RUST_LOG` in the service file to change the level, for example `RUST_LOG=info,voltbot::features::chat=debug`.

### Updating

```bash
git pull
cargo build --release
sudo install -o voltbot -g voltbot target/release/voltbot /opt/voltbot/voltbot
sudo systemctl restart voltbot
```

Database migrations run automatically on start.
