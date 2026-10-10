# Vivy

Vivy is a Discord bot written in Rust. It is a rewrite of [voltgpt](https://github.com/cherryroots/voltgpt), the older Go bot, ported one feature at a time.

## Goals

- **Event driven.** Each feature is its own module that listens to the Discord events it cares about, with guards deciding when it runs.
- **Provider agnostic.** AI chat sits behind one trait, so OpenAI, Claude or Gemini can be swapped in.
- **Simple and readable.** The code should be easy to study for someone learning Rust.

## Stack

- [serenity](https://github.com/serenity-rs/serenity) and [poise](https://github.com/serenity-rs/poise) for Discord
- [tokio](https://tokio.rs) as the async runtime
- SQLite for storage

## Status

Stage 4 of 4: the core (config, database, event dispatcher, logging), the control panel, reminders, the shared helpers (message splitting, multi-message replies, media and text extraction, GIF and video frames), AI chat with Claude or OpenAI, its tools, and the ❌/🔁 reaction controls, the movie wheel, memory, and repost ("snail") detection. Next up is the Gemini provider. See `docs/feature-map.md` for the plan and the order.

Chat runs on Claude or OpenAI, whichever `provider` under `[ai]` in `config.toml` names (OpenAI when it's left out). On Claude, Vivy can search the web, read web pages, and run code in a sandbox that keeps its files per channel, with Anthropic's skills for making spreadsheets, documents, slides and PDFs. Files people attach (spreadsheets, PDFs, data) are uploaded once through Anthropic's Files API and copied into that sandbox, and files her code saves are attached to her reply. Each answer is saved with Claude's own output, thinking included, and sent back unchanged with the rest of the conversation, so later replies keep her earlier reasoning and hit the prompt cache. When Claude declines a request, the API retries it on another Claude model (`fallbacks` under `[ai.claude]`). `fallback` under `[ai]` names a backup provider, either way round: when the main one is out of credit, chat moves to the backup and tries the main one again a day later; when it's down, an hour later. Both switches show in the log channel. The bot adds up what Claude costs each month from the token counts the API reports, at list prices, and warns in the log channel at 80% and 100% of `monthly_budget` under `[ai.claude]`. With an Anthropic Admin API key (`ANTHROPIC_ADMIN_KEY` in `.env`, read the caution in `.env.example` first), the total comes from Anthropic's real bill, read once an hour, with the estimate as the fallback. The status message says whether the admin key is off, working or failing, with the last bill it read and when. It can also show the prompt cache hit rate and what each job (chat, chime-ins, reflection, diary, emoji) cost; `billed_spend`, `show_cache_hits` and `show_job_costs` under `[ai.claude]` turn these on and off.

The movie wheel is voltgpt's betting game for movie night. `/wheel_status` shows the round as a picture with Claim, Place Bet, Remove Bet and Set Winner buttons; admins use `/wheel_add`, `/insert_bet` and `/reset_wheel`, and can undo a winner set by mistake. `/reset_wheel` starts a new season and keeps the old one viewable with `/wheel_status season:`. New seasons use pool betting: all bets and taxes of a round go into a pot that the bets on the winner share by stake, so long shots pay more than favourites, and a pot nobody won carries over. A player's bets in one round add up to at most half of their money, so nobody goes broke in one round. Place Bet opens a private bet slip with the option's current payout and buttons for 10%, 25% and 50% of your money, or any amount. voltgpt's imported game keeps voltgpt's fixed payouts until the next `/reset_wheel`. Change Name sets the name the wheel shows for you (empty goes back to your Discord name), and the Help button under the picture explains the rules privately.

Memory is a folder of notes Vivy keeps between conversations: a folder per person (`/memories/users/<user id>/`) with `about.md` and a file per topic like `games.md`, plus a server folder with a file per topic, like `channels.md` (what each channel is for) and `culture.md` (in-jokes and norms), which Vivy fills in from conversations, message search and its `list_channels` tool. Vivy also keeps notes about herself in each server (`/memories/vivy/personality.md` and `interests.md`), so she develops a personality of her own with each server's members; they're shown to her at the start of every conversation. Her skills folder (`/memories/vivy/skills/`) holds her own how-to notes for jobs that come up again, like the diary or wheel recaps; she opens one when its job comes up. There is one memory folder per server and a private one per person in DMs. Vivy reads and writes it with a `memory` tool whose commands match Anthropic's memory tool. Each question carries only the list of file names (the asker's and the server's files, and one line per other person), so the system prompt stays the same and cached. A person's own word about themselves overrules what others said, and notes from others name who said them. Every change is logged with who asked for it. `/memory show` shows what Vivy saved about you (or any file or folder), `/memory forget` deletes one of your files (picked from a list) or all of them, and admins can `/memory delete` any file or folder. Once a day, each server whose memory changed gets a reflection: Vivy tidies the folder (merging, fixing and dropping notes) and updates her notes about herself, including her mood (`/memories/vivy/mood.md`), whose `status:` line becomes her Discord status. Once a week she writes a short diary entry about her week and posts it in each channel or thread listed in `diary_channels` under `[features.memory]`; targets in the same server get the same entry.

Vivy also chimes in on her own. After any message there's a small chance (3% by default, then an hour of cooldown per channel) that she reads the last 25 messages and adds one short line, reacts with an emoji, or stays quiet. When she doesn't know what people are talking about, she can ask, and she saves the answer. She can save what she notices to memory while she reads along. Replying to her line continues the conversation like any answer. `chime_chance` and `chime_cooldown_minutes` under `[features.chat]` tune it, and `chime_chance = 0` turns it off.

When someone mentions something coming up (an interview, a trip), Vivy can plan to check in afterwards with the `schedule_follow_up` tool; when it's due she reads the channel and, unless it was already talked about, asks how it went. She also knows the server's custom emoji: on start she looks at each emoji's picture once and saves a short description, then only describes new ones, and the `list_server_emoji` tool lists them so she can use them like a regular.

Reminders understand `@Vivy remind me in 2h30m to …`, `at 16:30 CET`, `tomorrow at 9am`, `next friday`, `on 2026-12-24 at noon`, and the time at the end (`… in 2h`). `/reminders` lists and deletes them, `/timezone` sets your zone, and delivered reminders have snooze buttons.

## Setup

### 1. Create the Discord application

1. Create an application at <https://discord.com/developers/applications> and add a bot to it.
2. Under **Bot**, turn on the **Message Content Intent** and copy the token.
3. Invite the bot with the `bot` and `applications.commands` scopes and these permissions: View Channels, Send Messages, Send Messages in Threads, Embed Links, Attach Files, Read Message History, Add Reactions.
4. Create two private channels for the control panel, for example `#bot-logs` and `#bot-status`, and copy their IDs (turn on Developer Mode in Discord, then right-click the channel).

### 2. Build

You need [Rust](https://rustup.rs) (stable) and `ffmpeg` (used to read video frames). Optionally install `fonts-noto-color-emoji` and `fonts-noto-core` (and `fonts-noto-extra` for rarer scripts), so emoji and fancy letters in names show up in the movie wheel picture; restart the bot after installing fonts. The log warns once about each character no installed font has.

```bash
git clone https://github.com/cherryroots/Vivy.git
cd Vivy
cargo build --release
```

The binary is `target/release/vivy`.

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

Fill in `/opt/vivy/.env` (the secret `DISCORD_TOKEN`, and `ANTHROPIC_API_KEY` or `OPENAI_TOKEN` for chat, matching `provider` under `[ai]`, and the other one too if `fallback` names it) and `/opt/vivy/config.toml` (admin user IDs, the log and status channel IDs, and per-feature settings). The example files explain every key.

**Coming from voltgpt:** copy its `voltgpt.db` to `/opt/vivy/old.db` before the first start. The bot imports the reminders and the running movie wheel game, each only once, then renames the file to `old.db.imported`. voltgpt's wheel had no server, so set `main_server` in `config.toml` first: the game is imported into that server, and the import waits until it is set. If Vivy reuses voltgpt's bot account, the per-server slash commands voltgpt registered are removed on start, so nothing shows up twice.

### 4. Run with systemd

```bash
sudo cp deploy/vivy.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now vivy
```

### 5. Check on it

- **Discord:** the status channel has a message that refreshes every minute with uptime, the AI provider and model (with any fallback, and Claude's spend this month), errors and per-feature stats, and the log channel gets warnings, errors, and start and stop notices.
- **Service state:** `systemctl status vivy`
- **Full logs:** `journalctl -u vivy -f`. Set `RUST_LOG` in the service file to change the level, for example `RUST_LOG=info,vivy::features::chat=debug`.

### Updating

```bash
git pull
cargo build --release
sudo install -o vivy -g vivy target/release/vivy /opt/vivy/vivy
sudo systemctl restart vivy
```

Database migrations run automatically on start.
