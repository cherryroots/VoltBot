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

    // Tools this feature offers to the chat model (see "Chat tools").
    fn tools(&self) -> Vec<ToolDef> { Vec::new() }
    async fn call_tool(&self, ctx: &ToolCtx, name: &str, args: serde_json::Value) -> Result<String> {
        Err(anyhow!("unknown tool {name}"))
    }
}
```

Guards are small reusable functions (`is_from_bot`, `mentions_bot`, `is_reply_to_bot`, `reaction_on_bot_message`, `is_admin`) that a feature calls at the top of its handler. A central dispatcher receives each serenity event and hands it to every registered feature, so adding a feature means writing one module and adding one line to the feature list. Slash commands stay in poise, which already does registration and argument parsing for us.

Component and modal custom IDs keep the Go convention: `<feature_key>-<state>-<state>`, and the dispatcher routes on the part before the first `-`.

The exact shape of this gets settled in stage 1, with the smallest feature that proves it works.

## Events used

| Event | Who listens |
|---|---|
| Message created | Chat (mentions the bot), Reminders (mentions the bot and starts with "remind") |
| Reaction added | Chat (❌ stops and 🔁 regenerates a reply; guard = reaction is on a bot reply and from the person who asked) |
| Slash command | Reminders (`/reminders`, `/timezone`), Movie wheel (4 commands) |
| Button / select menu | Reminders (delete menu, snooze buttons), Movie wheel (5 components) |
| Modal submit | Movie wheel (bet amount) |
| Ready | All: load state from SQLite, re-arm reminder timers, log counts |

## Feature details

### 1. AI chat

What it does: when someone @-mentions the bot, it builds a request from the message (text, attachment names, embed text, images, video frames), sends it to OpenAI's Responses API with web search and code interpreter turned on, and streams the answer into a Discord reply, editing it about once per second. Long answers are split across several messages. Files produced by the code interpreter are attached to the final message.

Conversation history: every Discord message ID of a bot reply is stored with its OpenAI response ID (`response_ids` table). Replying to a bot message continues from that response ID. If no ID is found, the reply chain is fetched and rebuilt as input.

Progress feedback: a ⏳ reaction while the model works, removed when it finishes.

Events and guards: message created, guard = not from a bot, mentions the bot, not a reminder trigger.

Provider trait: the parts that differ per provider are building the input, streaming the output, continuing a conversation, running tool calls, and returning generated files. Those go behind a trait. Discord streaming, message splitting, media extraction and the bot's own tools stay outside it so every provider reuses them.

Prompt caching: the system prompt is fully static. The Go bot appended the current time, channel name and memory context to the instructions, which come first in every request, so the cache broke there and the conversation history after it was never reused. VoltBot drops memory and gives the model tools to look up the time and channel instead. The tool list is also static and in a fixed order, since it is part of the cached prefix.

Reaction controls: ❌ on a bot reply cancels a running answer (through a cancellation token kept per reply) and 🔁 regenerates it from the same input. Only the person who asked can use them.

Config: model name, reasoning effort and similar settings come from config, not constants in code.

Message splitting: one splitter replaces the Go bot's two (`SplitParagraph` and `SplitMessageSlices`). It splits on paragraph, then line, then character boundaries, and re-opens code blocks it cuts. The Go code cuts at byte offsets; Rust panics when a string is sliced inside a multi-byte character such as an emoji, so the Rust version must only cut on `char` boundaries. This is a good function to write tests for first.

#### Chat tools

The provider's built-in tools stay on (web search, code interpreter). On top of those, the bot offers its own function tools, which work the same with every provider.

Each feature can contribute tools, the same way it subscribes to events, so reminder tools live in the reminders module and wheel tools in the movie wheel module. A tool runs as the person who asked: it only sees channels they can see and only changes their own reminders.

| Tool | Feature | What it returns or does |
|---|---|---|
| `get_current_time` | Chat | Current date and time in the asker's timezone (or one the model passes) |
| `get_channel_info` | Chat | Channel name, topic, and parent channel for threads |
| `get_user_info` | Chat | A member's display name, timezone, roles and join date |
| `read_recent_messages` | Chat | The last N messages in the current channel, as text with author names |
| `get_message` | Chat | One message from a Discord message link |
| `create_reminder` | Reminders | Creates a reminder; `when` is text like "in 2h" or "friday 3pm", parsed by the same parser as typed reminders, and a parse error is returned so the model can retry |
| `list_reminders` | Reminders | The asker's pending reminders |
| `cancel_reminder` | Reminders | Deletes one of the asker's reminders |
| `get_wheel_status` | Movie wheel | Current round, options, bets and balances (read only) |

The tool loop: when the model asks for a tool, the bot runs it, sends the result back, and keeps streaming. Each tool call can show up briefly in the reply (for example "🔧 reading recent messages") so people can see what happened.

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

Events and guards: message created (guard = mentions the bot and text starts with `remind me`, `reminder` or `remind`), slash commands `/reminders` and `/timezone`, select menu `reminder`, snooze buttons, ready (start the scheduler).

Parsing: a real grammar written with the `winnow` parser-combinator crate, replacing the Go regex and prefix checks. Small parsers (`number`, `unit`, `duration`, `clock_time`, `date`, `weekday`, `timezone`) combine into one `when` parser, each with its own unit tests. When parsing fails, the error says which word it got stuck on. Supported forms:

- Relative: `in 2h30m`, `in 1 week 2 days`
- Clock times: `at 16:30`, `at 3pm`, `at noon`, `at midnight`
- Days: `tomorrow`, `friday`, `next friday`, `on 2026-12-24`, optionally with `at <time>`
- Timezone after a time: IANA names (`Europe/Oslo`) or common abbreviations. `CET`/`CEST` and similar map to a real zone so summer time is handled.
- The time can come before or after the message: `remind me in 2h to check the oven` and `remind me to check the oven in 2h`

Port the Go parser's test cases as the starting test suite.

Timezone: `/timezone <IANA name>` stores a per-user zone (new `user_settings` table). Times without a zone use it, then fall back to UTC.

Scheduler: one background task instead of a timer per reminder. It loads the next due reminder from SQLite, sleeps until then with `tokio::select!`, and is woken early through a `tokio::sync::Notify` whenever a reminder is added or deleted.

Delivery: a reminder is only deleted after it was sent. If sending fails, it stays and is retried with a growing delay. The fired message has snooze buttons (10m, 1h, tomorrow) that create a new reminder with the same text and images.

Storage: `reminders` table (user, channel, guild, message, images as BLOB or base64 JSON, fire time, created time, attempts), `user_settings` (user, timezone).

Likely crates: `winnow`, `chrono`, `chrono-tz`.

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
| 1 | Skeleton: config from `.env`, SQLite, poise setup, the feature dispatcher and guards, plus Reminders (parser, scheduler, timezone, snooze) | Reminders touch every event type (message, slash command, buttons, select menu, ready, timers), so they prove the skeleton |
| 2 | Shared helpers: message splitting, sending and editing, media extraction, downloads | Chat needs them and they are easy to test on their own |
| 3 | AI chat behind the provider trait, OpenAI implementation, tool loop and chat tools, reaction controls | The main feature; builds on stages 1 and 2, and reminder tools reuse the stage 1 parser |
| 4 | Movie wheel, plus its `get_wheel_status` tool | Self-contained; mostly embeds, buttons and pure money logic |
| Later | Claude provider, then Gemini | When credits arrive |
| Later | Image hashing redesign | TODO |

## TODO: image hashing

The Go bot hashes every image and video in the main server after 3 seconds (to let embeds load), stores perceptual hashes, and replies when a near-duplicate is posted. `/hash_server` back-fills a whole server. Revisit once the design is decided; the `img_hash` or `image_hasher` crates are the Rust equivalents of `goimagehash`.

## Future memory

Not ported. If it comes back, the idea is skill-based: per-user folders or tables that the model queries through a tool when it needs them, so nothing is injected into every prompt and the cache hit rate stays high.

## Go code that is not needed

Everything under `memory/`, `apis/wavespeed/`, `hasher/` (for now), `handler/memory_digest.go`, the `memory_*`, `draw`, `video` and `hash_server` commands, the `users` table, and the memory tables in `db/db.go`.
