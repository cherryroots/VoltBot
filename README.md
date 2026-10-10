# Vivy

Vivy is a Discord bot written in Rust: a chat companion with her own memory, moods and voice, plus reminders, a movie night betting game and repost detection. It is a rewrite of [voltgpt](https://github.com/cherryroots/voltgpt), the older Go bot, ported one feature at a time.

The code aims to be:

- **Event driven.** Each feature is its own module that listens to the Discord events it cares about, with guards deciding when it runs. Features never import each other.
- **Provider agnostic.** AI chat sits behind one trait, so Claude, OpenAI or (later) Gemini can be swapped in.
- **Simple and readable.** Easy to study for someone learning Rust.

Built with [serenity](https://github.com/serenity-rs/serenity) and [poise](https://github.com/serenity-rs/poise) for Discord, [tokio](https://tokio.rs), and one SQLite file. [`docs/feature-map.md`](docs/feature-map.md) has the full design and the port plan.

## Features

Every feature has a section in `config.toml` and can be turned off or limited to some servers or channels. `config.example.toml` explains every setting.

### Chat

@-mention Vivy to talk to her; replying to one of her answers continues the conversation. `provider` under `[ai]` picks Claude or OpenAI, and `fallback` names a backup that takes over when the main one is out of credit (tried again a day later) or down (an hour later).

- **Tools:** web search and web pages, code in a sandbox that keeps its files per channel (on Claude, with Anthropic's skills for spreadsheets, documents, slides and PDFs), attached files, message search, pins and server events.
- **Controls:** ❌ under an answer stops or deletes it, 🔁 asks again. Only the person who asked can use them.
- **Chime-ins:** now and then (3% of messages, then an hour's cooldown per channel) she reads the last 25 messages and adds a line, reacts, or stays quiet.
- **Follow-ups:** when someone mentions something coming up, she can check in afterwards to ask how it went.
- **Cost:** Claude's spend is tracked per month against `monthly_budget`, with warnings in the log channel at 80% and 100%. An optional Anthropic Admin key reads the real bill; read the caution in `.env.example` first.

### Memory and personality

Vivy keeps notes between conversations in a folder per server (and a private one per person in DMs): a file per person and topic under `/memories/users/<id>/`, server notes like `channels.md` and `culture.md`, and notes about herself under `/memories/vivy/`, so she grows her own personality with each server. A person's own word about themselves overrules what others said.

- `/memory show` shows what she saved about you, `/memory forget` deletes it, and admins can `/memory delete` anything.
- Once a day she tidies each server's notes and updates her notes about herself. Once a week she writes a diary entry and posts it in `diary_channels`.
- Her mood sets her Discord status, and optionally her avatar (`faces_dir`), her nickname emoji (`nickname`) and the tone of her answers. Her banner follows the time of day (`banners_dir`).
- She learns the server's custom emoji and uses them like a regular.

### Voice messages

With an ElevenLabs key, Vivy can answer with a Discord voice message in her own voice. `monthly_characters` under `[features.voice]` caps the use.

### Reminders

`@Vivy remind me in 2h30m to …`, `at 16:30 CET`, `tomorrow at 9am`, `next friday` or `on 2026-12-24 at noon`. `/reminders` lists and deletes them, `/timezone` sets your zone, and delivered reminders have snooze buttons.

### Movie wheel

voltgpt's betting game for movie night. `/wheel_status` shows the round as a picture with buttons to claim, bet and set the winner. Bets and taxes go into a pot that the bets on the winner share by stake, and a player can bet at most half their money per round. Admins use `/wheel_add`, `/insert_bet` and `/reset_wheel` (which starts a new season). The Help button explains the rules.

### Snail detection

Links and pictures are remembered as they're posted. Right-click a message, then Apps, then **Check Snail** to see who posted it first. Admins can read older history with `/snail_backfill start`.

### Control panel

A status picture in a channel of your choice, refreshed every minute: uptime, version, the AI provider and its spend, Vivy's mood, per-feature stats and graphs. Warnings, errors and start and stop notices go to a log channel.

## Setup

### 1. Create the Discord application

1. Create an application at <https://discord.com/developers/applications> and add a bot to it.
2. Under **Bot**, turn on the **Message Content Intent** and copy the token.
3. Invite the bot with the `bot` and `applications.commands` scopes and these permissions: View Channels, Send Messages, Send Messages in Threads, Embed Links, Attach Files, Read Message History, Add Reactions. Add Change Nickname if you use `nickname`.
4. Create two private channels for the control panel, for example `#bot-logs` and `#bot-status`, and copy their IDs (turn on Developer Mode in Discord, then right-click the channel).

### 2. Build

You need [Rust](https://rustup.rs) (stable) and `ffmpeg`. For emoji and unusual letters in the pictures, also install `fonts-noto-color-emoji` and `fonts-noto-core`.

```bash
git clone https://github.com/cherryroots/Vivy.git
cd Vivy
cargo build --release
```

### 3. Install

```bash
sudo useradd --system --home /opt/vivy --shell /usr/sbin/nologin vivy
sudo mkdir -p /opt/vivy
sudo cp target/release/vivy /opt/vivy/
sudo cp config.example.toml /opt/vivy/config.toml
sudo cp .env.example /opt/vivy/.env
sudo chown -R vivy:vivy /opt/vivy
sudo chmod 600 /opt/vivy/.env
```

Then fill in the two files:

- `.env` holds the secrets: `DISCORD_TOKEN`, plus `ANTHROPIC_API_KEY` and/or `OPENAI_TOKEN` for whichever providers `[ai]` names, and optionally `ELEVENLABS_API_KEY`.
- `config.toml` holds everything else: your user ID in `admins`, `main_server`, the log and status channel IDs, and per-feature settings.

### 4. Run with systemd

```bash
sudo cp deploy/vivy.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now vivy
```

`systemctl status vivy` shows whether it's running and `journalctl -u vivy -f` follows the logs. Set `RUST_LOG` in the service file to change the log level, for example `RUST_LOG=info,vivy::features::chat=debug`. The database, `vivy.db`, is created on the first start, and migrations run automatically.

**Any other way works too.** Every file the bot uses (`.env`, `config.toml`, `vivy.db`, `faces_dir`, `banners_dir`) is found relative to the folder it runs in, so you can also skip the install and run `./target/release/vivy` straight from the repo folder, in `tmux`, or however you like. `VIVY_CONFIG` points at a config file somewhere else.

### Updating

```bash
git pull
cargo build --release
sudo install -o vivy -g vivy target/release/vivy /opt/vivy/vivy
sudo systemctl restart vivy
```

When running from the repo folder, just rebuild and restart the bot.

### Coming from voltgpt

Copy voltgpt's `voltgpt.db` into the bot's folder (`/opt/vivy`) as `old.db` before starting. Its reminders and movie wheel game are imported once (the wheel into `main_server`), then the file is renamed to `old.db.imported`.
