# VoltBot feature map

This maps every feature of the Go bot (voltgpt) to what VoltBot will do with it, which Discord events each feature listens to, and the order we port them in. It is the reference for every later stage.

## Decisions so far

| Topic | Decision |
|---|---|
| Discord library | `serenity` + `poise` (poise handles slash commands, serenity handles raw events) |
| AI provider | OpenAI first, behind a provider trait so Claude and Gemini can be added later |
| Storage | SQLite, raw SQL through `rusqlite` (same idea as the Go bot: no ORM) |
| Async runtime | `tokio` (serenity already uses it) |

## Scope

| Go feature | Go location | VoltBot |
|---|---|---|
| AI chat (mention the bot, streamed reply) | `handler/messages.go`, `apis/openai/chat.go` | **Port** |
| Reminders (`@Vivy remind me in 2h ...`, `/reminders`) | `reminder/`, parts of `handler/` | **Port** |
| Movie wheel betting game | `gamble/`, `handler/gamble_status.go`, wheel commands, buttons, modal | **Port** |
| Image hashing and duplicate detection (`/hash_server`) | `hasher/` | **TODO** (redesign later) |
| Image and video generation (`/draw`, `/video`, Wavespeed) | `apis/wavespeed/` | Dropped |
| Long-term memory (notes, profiles, vector search, digest, admin commands) | `memory/`, `memory_*` commands | Dropped (see "Future memory" below) |

## How features plug in

The Go bot has one big `HandleMessage` function that does hashing, memory capture, reminders and chat in sequence. VoltBot splits that up: each feature is a separate module that says which events it wants, with a guard that decides whether it should run.

The idea in plain terms:

```rust
// Every feature implements this. Default methods do nothing,
// so a feature only overrides the events it cares about.
#[async_trait]
trait Feature: Send + Sync {
    fn name(&self) -> &'static str;

    async fn on_message(&self, ctx: &BotCtx, msg: &Message) -> Result<()> { Ok(()) }
    async fn on_reaction_add(&self, ctx: &BotCtx, r: &Reaction) -> Result<()> { Ok(()) }
    async fn on_component(&self, ctx: &BotCtx, i: &ComponentInteraction) -> Result<()> { Ok(()) }
    async fn on_modal(&self, ctx: &BotCtx, i: &ModalInteraction) -> Result<()> { Ok(()) }
    async fn on_ready(&self, ctx: &BotCtx) -> Result<()> { Ok(()) }
}
```

Guards are small reusable functions (`is_from_bot`, `mentions_bot`, `is_reply_to_bot`, `reaction_on_bot_message`, `is_admin`) that a feature calls at the top of its handler. A central dispatcher receives each serenity event and hands it to every registered feature, so adding a feature means writing one module and adding one line to the feature list. Slash commands stay in poise, which already does registration and argument parsing for us.

Component and modal custom IDs keep the Go convention: `<feature_key>-<state>-<state>`, and the dispatcher routes on the part before the first `-`.

The exact shape of this gets settled in stage 1, with the smallest feature that proves it works.

## Events used

| Event | Who listens |
|---|---|
| Message created | Chat (mentions the bot), Reminders (mentions the bot and starts with "remind") |
| Reaction added | Nothing yet; available for future features (for example reacting on a bot message) |
| Slash command | Reminders (`/reminders`), Movie wheel (4 commands) |
| Button / select menu | Reminders (delete menu), Movie wheel (5 components) |
| Modal submit | Movie wheel (bet amount) |
| Ready | All: load state from SQLite, re-arm reminder timers, log counts |

## Feature details

### 1. AI chat

What it does: when someone @-mentions the bot, it builds a request from the message (text, attachment names, embed text, images, video frames), sends it to OpenAI's Responses API with web search and code interpreter turned on, and streams the answer into a Discord reply, editing it about once per second. Long answers are split across several messages. Files produced by the code interpreter are attached to the final message.

Conversation history: every Discord message ID of a bot reply is stored with its OpenAI response ID (`response_ids` table). Replying to a bot message continues from that response ID. If no ID is found, the reply chain is fetched and rebuilt as input.

Progress feedback: a ⏳ reaction while the model works, removed when it finishes.

Events and guards: message created, guard = not from a bot, mentions the bot, not a reminder trigger.

Provider trait: the parts that differ per provider are building the input, streaming the output, continuing a conversation, and returning generated files. Those go behind a trait. Discord streaming, message splitting and media extraction stay outside it so every provider reuses them.

Change from Go: the system prompt drops the memory/background-facts section, and no per-request memory block is added, so the prompt prefix stays identical between requests and caches well. The current time and channel name are still appended at the end.

Go helpers this needs (port as Rust functions):
- `utility/messages.go`: `SplitMessageSlices`, `SplitParagraph`, `HasVisibleContent` (2000-char splitting), `GetMessagesBefore`, `GetReferencedMessage`, `ReplyChainUsers`, `MessageMentionsUser`, `IsReplyToUser`
- `utility/discord.go`: `CleanMessage`, `ResolveMentions`, `GetMessageMediaURL`, `AttachmentText`, `EmbedText`, `IsAdmin`
- `utility/url.go`: `DownloadBytes`, `URLToExt`, `IsImageURL`, `IsVideoURL`, `MediaType`
- `utility/image.go`: base64 image download, GIF to frames, PNG grid
- `utility/video.go`: video frames through ffmpeg (call the `ffmpeg` binary directly with `tokio::process::Command`)
- `discord/discord.go`: send/edit helpers, link-embed suppression, error message helper

Storage: `response_ids (message_id, response_id)`.

Likely crates: `async-openai` (check it supports the Responses API and streaming; fall back to `reqwest` + `serde` + SSE if not), `reqwest`, `base64`, `image`.

### 2. Reminders

What it does: `@Vivy remind me in 2h30m do the thing` or `@Vivy remind me at 16:30 CET do the thing` stores a reminder (with any attached images) and pings the user in the same channel when it is due. `/reminders` lists your pending reminders with a select menu to delete one.

Events and guards: message created (guard = mentions the bot and text starts with `remind me`, `reminder` or `remind`), slash command `/reminders`, select menu `reminder`, ready (load pending reminders and re-arm timers).

Parsing: relative offsets (`in 1y2mo3w4d5h6m7s`, long and short unit names) and absolute times (`at HH:MM` plus a date and timezone, IANA names or the abbreviations listed in `reminder/parse.go`). The Go parser has tests; port those tests too, they make a good first Rust exercise.

Storage: `reminders` table (user, channel, guild, message, images as base64 JSON, fire time, created time). Timers: one `tokio::time::sleep_until` task per reminder, cancelled through a map of handles when deleted.

Likely crates: `chrono`, `chrono-tz`, `regex`.

### 3. Movie wheel

What it does: a betting game for movie night. Admins add options to the wheel, players claim 100 per round, bet on which option wins, and admins set the winner. Players who bet under 10% of their money get taxed. A status embed shows the round with buttons for claim, bet and winner.

Commands: `/wheel_status`, `/wheel_add` (admin), `/insert_bet` (admin), `/reset_wheel` (admin).
Components: `button_currentround`, `button_claim`, `button_bet`, `button_winner`, `menu_bet` (place, remove, winner). Modal: `modal_bet` (amount).

Events and guards: slash commands, buttons, select menus, modal submit; admin guard on the admin actions.

Storage: the whole game is one JSON blob in `game_state`, loaded at startup and written after every change. Keep that: it maps directly to a `serde` struct behind a `tokio::sync::Mutex`.

The money logic (`playerMoney`, `playerTax`, `payout`) is pure and easy to unit test, so it's worth porting with tests before touching Discord.

## Port order

| Stage | Work | Why this order |
|---|---|---|
| 1 | Skeleton: config from `.env`, SQLite, poise setup, the feature dispatcher and guards, plus Reminders | Reminders touch every event type (message, slash command, select menu, ready, timers) with simple logic, so they prove the skeleton |
| 2 | Shared helpers: message splitting, sending and editing, media extraction, downloads | Chat needs them and they are easy to test on their own |
| 3 | AI chat behind the provider trait, OpenAI implementation | The main feature; builds on stages 1 and 2 |
| 4 | Movie wheel | Self-contained; mostly embeds, buttons and pure money logic |
| Later | Claude provider, then Gemini | When credits arrive |
| Later | Image hashing redesign | TODO |

## TODO: image hashing

The Go bot hashes every image and video in the main server after 3 seconds (to let embeds load), stores perceptual hashes, and replies when a near-duplicate is posted. `/hash_server` back-fills a whole server. Revisit once the design is decided; the `img_hash` or `image_hasher` crates are the Rust equivalents of `goimagehash`.

## Future memory

Not ported. If it comes back, the idea is skill-based: per-user folders or tables that the model queries through a tool when it needs them, so nothing is injected into every prompt and the cache hit rate stays high.

## Go code that is not needed

Everything under `memory/`, `apis/wavespeed/`, `hasher/` (for now), `handler/memory_digest.go`, the `memory_*`, `draw`, `video` and `hash_server` commands, the `users` table, and the memory tables in `db/db.go`.
