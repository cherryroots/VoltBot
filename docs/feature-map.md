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

Conversation history: in Go, every Discord message ID of a bot reply is stored with its OpenAI response ID (`response_ids` table), and replying to a bot message continues from that ID. That only works for OpenAI, because Claude and Gemini keep no conversation state on their side. VoltBot keeps its own provider-neutral history instead (see "AI providers" below), and OpenAI's response ID becomes an optional shortcut stored next to it.

Progress feedback: a ⏳ reaction while the model works, removed when it finishes.

Events and guards: message created, guard = not from a bot, mentions the bot, not a reminder trigger.

Provider trait: the parts that differ per provider are building the input, streaming the output, continuing a conversation, and returning generated files. Those go behind a trait (see "AI providers" below). Discord streaming, message splitting, media extraction, the tool loop and the bot's own tools stay outside it so every provider reuses them.

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

Storage: `chat_turns` (see "AI providers" below).

Likely crates: `async-openai` (check it supports the Responses API and streaming; fall back to `reqwest` + `serde` + SSE if not), `reqwest`, `base64`, `image`.

#### AI providers

**Claude: use the Claude API, not the Agent SDK.** There is no official Rust Agent SDK; Anthropic ships it for Python and TypeScript only. The Rust crates under that name are community projects, and they work by starting the Claude Code CLI as a subprocess, so the bot server would need Node.js and Claude Code installed and would start a process per message. The Agent SDK is built for coding agents: its built-in tools read and write files and run shell commands on the machine, which is the wrong thing to hand to Discord users. It also needs an API key, since Claude subscription logins aren't allowed for apps other people use, so it doesn't save money. Its sessions are local transcript files on disk resumed by session ID, which is a different model from Discord reply chains.

The plain Claude API (Messages API) has everything the bot needs: streaming, images, custom tools, server-side web search and web fetch, code execution, adaptive thinking with an effort setting, and prompt caching. There is no official Rust client SDK either, so the Claude provider calls the HTTP API with `reqwest` + `serde` and parses the SSE stream. That is a few hundred lines and doubles as a good learning exercise. (Community crates exist, but they tend to lag behind new API features.)

**Conversation state differs per provider.** OpenAI can keep the conversation on its side (`previous_response_id`). Claude and Gemini are stateless: every request sends the whole history, and prompt caching makes the repeated part cheap. The provider trait therefore always receives the full history, plus an optional continuation ID the provider may use instead:

```rust
struct ChatRequest {
    system: String,                // static, cache friendly
    history: Vec<Turn>,            // provider-neutral, oldest first
    continuation: Option<String>,  // e.g. OpenAI response ID, only if this provider wrote it
    tools: Vec<ToolDef>,           // the bot's own tools, fixed order
}

enum ChatEvent {
    TextDelta(String),
    ToolCall { id: String, name: String, args: serde_json::Value },
    File(GeneratedFile),           // from code interpreter / code execution
    Done { continuation: Option<String> },
}

#[async_trait]
trait ChatProvider: Send + Sync {
    fn name(&self) -> &'static str;   // "openai", "claude", "gemini"
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatEvent>>>;
}
```

The tool loop lives outside the trait: when a `ToolCall` arrives, the bot runs the tool, appends the call and its result to the history, and calls `stream` again. Each provider converts `Turn` into its own wire format and handles its own caching details (Claude needs `cache_control`; OpenAI caches automatically with `prompt_cache_key`).

**One history table, marked per provider.** Replaces Go's `response_ids`:

`chat_turns (discord_message_id PK, parent_message_id, role, content_json, provider, model, continuation_id NULL, created_at)`

Every user message the bot answers and every bot reply gets a row. `content_json` holds provider-neutral content (text, image references, tool calls and results). `provider` says which provider wrote the row, and `continuation_id` holds OpenAI's response ID when there is one. When someone replies to a bot message, the bot walks `parent_message_id` up the chain. If the newest bot turn was written by the current provider and has a `continuation_id`, it sends that. Otherwise it sends the rebuilt history. This means switching providers in the middle of a conversation works, and it no longer depends on fetching old messages from Discord. Image attachments are stored by URL; Discord CDN links expire, so the rebuild refreshes them through Discord's refresh-urls endpoint before sending.

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

Storage: the whole game is one JSON blob in `game_state`, loaded at startup and written after every change. Keep that, one row per guild and season: it maps directly to a `serde` struct behind a `tokio::sync::Mutex`.

Rules today, so the port keeps them exact: every round a player can claim 100. Before each payout, a player who bet less than 10% of their money loses 3% of it per missing percentage point (up to 30%). A winning bet pays `amount × (options − 1)`, where options are the wheel options left in that round; a losing bet loses its amount. A player can bet on at most half of the remaining options (rounded up). Balances are never stored: they are recomputed from the full list of rounds, claims and bets every time. Integer division truncates, as in Go.

Changes in the port:

- **One ledger function.** Go recomputes a player's money from round 0 for every row of the status embed. VoltBot computes a ledger once, a pure function that folds over the rounds and returns every player's balance, tax and payout per round. The embed, the bet checks and the `get_wheel_status` tool all read from it. It is the first thing to write, with unit tests.
- **Same numbers as today.** A one-time import reads the current `game_state` JSON from voltgpt, and a test checks that the ledger produces the same balances the Go bot shows, so the running game carries over.
- **Store user IDs, not user objects.** Go saves the whole Discord user in the JSON, so names and avatars go stale. VoltBot stores IDs and looks names up when it renders the embed.
- **Fix a wrong winner.** Once a winner is set, a new round starts and the old one can no longer be changed, so a mis-click is permanent. Admins get an "Undo winner" action on the latest resolved round, allowed while the new round has no bets yet.
- **Winner without bets.** Go refuses to set a winner when nobody bet ("No bets!"), which blocks the wheel if a movie was watched without bets. VoltBot allows it.
- **Seasons instead of a hard reset.** `/reset_wheel` deletes everything with no confirmation. VoltBot asks for confirmation with a button and archives the old game as a finished season, so past results stay viewable.
- **One game per server.** Go has a single global game. VoltBot keys the game by guild ID; it costs nothing and avoids surprises.
- **Don't hold the lock during Discord calls.** Go keeps `gamble.Mu` locked while it calls the Discord API. VoltBot locks the game state, makes the change, builds the embed, unlocks, and only then talks to Discord. With `tokio::sync::Mutex` this is easy to get wrong, so it's worth learning here.

Ideas for later, not part of the port: a `/wheel_spin` command that picks the winner randomly with an animated embed, and a per-player balance history.

## Port order

| Stage | Work | Why this order |
|---|---|---|
| 1 | Skeleton: config from `.env`, SQLite, poise setup, the feature dispatcher and guards, plus Reminders (parser, scheduler, timezone, snooze) | Reminders touch every event type (message, slash command, buttons, select menu, ready, timers), so they prove the skeleton |
| 2 | Shared helpers: message splitting, sending and editing, media extraction, downloads | Chat needs them and they are easy to test on their own |
| 3 | AI chat behind the provider trait, OpenAI implementation, tool loop and chat tools, reaction controls | The main feature; builds on stages 1 and 2, and reminder tools reuse the stage 1 parser |
| 4 | Movie wheel, plus its `get_wheel_status` tool | Self-contained; mostly embeds, buttons and pure money logic |
| Later | Claude provider (Messages API over `reqwest`), then Gemini | When credits arrive |
| Later | Image hashing redesign | TODO |

## TODO: image hashing

The Go bot hashes every image and video in the main server after 3 seconds (to let embeds load), stores perceptual hashes, and replies when a near-duplicate is posted. `/hash_server` back-fills a whole server. Revisit once the design is decided; the `img_hash` or `image_hasher` crates are the Rust equivalents of `goimagehash`.

## Future memory

Not ported. If it comes back, the idea is skill-based: per-user folders or tables that the model queries through a tool when it needs them, so nothing is injected into every prompt and the cache hit rate stays high.

## Go code that is not needed

Everything under `memory/`, `apis/wavespeed/`, `hasher/` (for now), `handler/memory_digest.go`, the `memory_*`, `draw`, `video` and `hash_server` commands, the `users` table, and the memory tables in `db/db.go`.
