# VoltBot feature map

This maps every feature of the Go bot (voltgpt) to what VoltBot will do with it, which Discord events each feature listens to, and the order we port them in. It is the reference for every later stage.

## Decisions so far

| Topic | Decision |
|---|---|
| Discord library | `serenity` + `poise` (poise handles slash commands, serenity handles raw events) |
| AI provider | OpenAI first, behind a provider trait. Claude next, through the plain Messages API (not the Agent SDK), then maybe Gemini |
| Storage | One SQLite file (`voltbot.db`, WAL mode), raw SQL through `rusqlite` behind `tokio-rusqlite` (same idea as the Go bot: no ORM). Each feature owns its tables, named after it (`reminders`, `reminder_images`, `wheel_*`, `chat_turns`), and its own migrations, recorded in a shared `schema_migrations` table. Core tables shared by all features: `user_settings`, `legacy_imports` |
| Config | `.env` for secrets, `config.toml` for everything else (admin IDs, per-feature settings and guild/channel gating that Go hardcodes) |
| Async runtime | `tokio` (serenity already uses it) |

## Scope

| Go feature | Go location | VoltBot |
|---|---|---|
| AI chat (mention the bot, streamed reply) | `handler/messages.go`, `apis/openai/chat.go` | **Port** |
| Reminders (`@Vivy remind me in 2h ...`, `/reminders`) | `reminder/`, parts of `handler/` | **Port** |
| Movie wheel betting game | `gamble/`, `handler/gamble_status.go`, wheel commands, buttons, modal | **Port** |
| Image hashing and duplicate detection (`/hash_server`) | `hasher/` | **TODO** (redesign later) |
| Image and video generation (`/draw`, `/video`, Wavespeed) | `apis/wavespeed/` | Dropped |
| Long-term memory (notes, profiles, vector search, digest, admin commands) | `memory/`, `memory_*` commands | Replaced by a new design (see "Memory" below) |

## Architecture

The Go bot has one big `HandleMessage` function that does hashing, memory capture, reminders and chat in sequence, and chat has to know about reminders to stay out of their way. VoltBot is built so that adding a feature means adding one folder and one line, without editing any other feature.

### Rules

1. **Features never import each other.** A feature may use `core`, `ai` and `util`, nothing else. Features talk to each other in two ways only: chat tools (a feature offers a tool, chat calls it) and bot events (a feature publishes an event, others may listen).
2. **A feature declares everything it adds in one place**: commands, mention prefixes, component handlers, chat tools, migrations, background tasks and its `old.db` import. The dispatcher reads those declarations; nothing is registered by hand elsewhere.
3. **Logic, storage and Discord are separate files.** Pure logic (the wheel ledger, the reminder parser, the message splitter) takes plain values and returns plain values, so it is tested without Discord or a database. SQL lives in `store.rs` and is tested against an in-memory SQLite. Discord code only translates events into calls to those two.

### The `Feature` trait

```rust
#[async_trait]
pub trait Feature: Send + Sync {
    /// Also the config section, the custom ID prefix and the log label.
    fn name(&self) -> &'static str;

    // Declarations, read once at startup.
    fn commands(&self) -> Vec<poise::Command<Data, Error>> { vec![] }
    fn migrations(&self) -> Vec<Migration> { vec![] }
    fn mention_prefixes(&self) -> &'static [&'static str] { &[] }
    fn tools(&self) -> Vec<ToolDef> { vec![] }
    async fn stats(&self, ctx: &BotCtx) -> Vec<Stat> { vec![] }  // shown on the control panel

    // Lifecycle.
    async fn start(&self, ctx: &BotCtx) -> Result<()> { Ok(()) }   // background tasks
    async fn import_legacy(&self, ctx: &BotCtx, old: &OldDb) -> Result<usize> { Ok(0) }

    // Events. Every method has an empty default, so a feature only writes the ones it uses.
    async fn on_mention(&self, ctx: &BotCtx, msg: &Message, rest: &str) -> Result<()> { Ok(()) }
    async fn on_message(&self, ctx: &BotCtx, msg: &Message) -> Result<()> { Ok(()) }
    async fn on_reaction_add(&self, ctx: &BotCtx, r: &Reaction) -> Result<()> { Ok(()) }
    async fn on_component(&self, ctx: &BotCtx, i: &ComponentInteraction, id: &str) -> Result<()> { Ok(()) }
    async fn on_modal(&self, ctx: &BotCtx, i: &ModalInteraction, id: &str) -> Result<()> { Ok(()) }
    async fn on_bot_event(&self, ctx: &BotCtx, event: &BotEvent) -> Result<()> { Ok(()) }
    // A chat tool from `tools()`, run for the person who asked (`Asker`: user, guild,
    // channel, message).
    async fn run_tool(&self, ctx: &BotCtx, asker: &Asker, name: &str, args: &Value) -> Result<String> {
        Err(anyhow!("unknown tool {name}"))
    }
    // Text added after the asker's question for one answer, like memory's file list.
    // `fresh`: the model reads the conversation from the start, not continuing an answer.
    async fn chat_context(&self, ctx: &BotCtx, asker: &Asker, fresh: bool) -> Result<Option<String>> { Ok(None) }
}
```

The whole bot is the list in `features/mod.rs`:

```rust
pub fn all() -> Vec<Arc<dyn Feature>> {
    vec![
        Arc::new(reminders::Reminders::default()),
        Arc::new(wheel::Wheel::default()),
        Arc::new(control_panel::ControlPanel::default()),
        Arc::new(memory::Memory),
        Arc::new(chat::Chat::default()),  // last: answers mentions nobody else claimed
    ]
}
```

### How the dispatcher routes events

| Event | Routing |
|---|---|
| Message that @-mentions the bot | The mention is stripped. The first feature whose `mention_prefixes` match the start of the text gets `on_mention` (Reminders claims `remind me`, `reminder`, `remind`). If none match, Chat gets it. Chat never needs to know reminders exist. |
| Any other message | `on_message` for every enabled feature. For observers like a future image hasher. |
| Reaction added | `on_reaction_add` for every enabled feature; each checks its own guard (Chat: reaction on its own reply, from the person who asked). |
| Button, select menu, modal | Custom IDs are `<feature name>:<action>:<args>`. The dispatcher strips the feature name and calls only that feature. Each feature parses the rest into its own action enum, so a typo is a compile error, not a silent no-op. |
| Slash command | poise, from the commands each feature declared. |
| `BotEvent` | Published by features through `ctx.events` (a `tokio::sync::broadcast` channel), delivered to every feature's `on_bot_event`. Starts small, for example `ReminderFired` and `WheelRoundResolved`. |

Every handler runs in its own tokio task, so a slow or failing feature never blocks the others, and a panic is logged instead of crashing the bot (the Go bot has no panic recovery). Errors are logged with the feature name; an error type that marks a message as safe to show is sent back to the user, anything else becomes a generic "something went wrong".

**Gating is config, not code.** Go hardcodes `MainServer` and a channel blacklist inside the handlers. VoltBot gives every feature an `enabled` flag and optional guild and channel allow/deny lists in its config section, and the dispatcher checks them before calling the feature:

```toml
[features.chat]
enabled = true

[features.wheel]
guilds = [122962330165313536]            # only on the main server

[features.image_hashing]                 # later
guilds = [122962330165313536]
deny_channels = [850179179281776670]
```

Secrets stay in `.env`; everything else goes in `config.toml`, and each feature reads its own `[features.<name>]` section into its own `serde` struct.

### Logging and error reporting

- **`tracing` everywhere.** The dispatcher opens a span for every event with the feature name, guild, channel, user and interaction or message ID, so every log line inside a handler carries that context without passing it around. `tracing-subscriber` writes readable lines in development and JSON in production, with the level set by `RUST_LOG` (for example `RUST_LOG=info,voltbot::features::chat=debug`).
- **Errors keep their cause.** Handlers return `anyhow::Result`, and errors get `.context("what we were doing")` where they happen. The dispatcher logs the whole chain once, at `error` level, with the span's context. A panic hook logs panics the same way.
- **Logs in Discord.** A small `tracing` layer forwards log events to the control panel's log channel (see "Control panel" below), rate limited and grouped so one broken feature doesn't flood it. An error message includes the feature, the error chain and a link to the triggering message.
- **Optional, later: Sentry.** The `sentry` crate with its `tracing` integration groups errors, counts them, and keeps the breadcrumbs that led up to each one. It turns on when `SENTRY_DSN` is set; GlitchTip is a self-hostable server that speaks the same protocol.
- **Running it.** A systemd service (`deploy/voltbot.service`): it restarts the bot on a crash and keeps the full logs in the journal (`journalctl -u voltbot -f`). Setup steps are in the README.

### Shared services on `BotCtx`

Features get everything shared through one context value: the serenity HTTP client and cache, `db`, `config`, `settings` (per-user values like the timezone), `ai`, `events`, and a shutdown `CancellationToken` that background tasks watch.

- **`db`** wraps `tokio-rusqlite`: plain `rusqlite` SQL, run on a dedicated thread through `db.call(|conn| ...)`, so the async code never blocks and a connection is never held across an `.await`.
- **`ai`** holds the provider registry. The provider trait lives in `ai`, not inside the chat feature, so any feature can make a one-off model call (a summary, a classification) through the same OpenAI or Claude setup.

### Folder layout

```
src/
  main.rs                 # load config, open db, build features, start serenity + poise
  core/                   # ctx, dispatcher, guards, config, db, events, custom_id, errors
  ai/                     # mod.rs (provider trait, Turn types), sse.rs, openai.rs, later claude.rs
  util/                   # split.rs, reply.rs (multi-message replies), media.rs, frames.rs, text.rs
  features/
    mod.rs                # the feature list
    reminders/
      mod.rs              # impl Feature: wiring only
      parse.rs            # winnow grammar (pure)
      store.rs            # SQL
      commands.rs         # /reminders, /timezone
      ui.rs               # embeds, buttons, menus
      tools.rs            # create/list/cancel_reminder
      import.rs           # from old.db
    wheel/                # ledger.rs (pure), store.rs, commands.rs, ui.rs, render.rs, tools.rs, import.rs
    chat/                 # mod.rs, answer.rs, history.rs, store.rs, tools.rs, prompt.md
    memory/               # folder.rs (pure), store.rs, tool.rs, commands.rs
```

Plain modules in one crate. A Cargo workspace with a crate per feature would let the compiler enforce rule 1, but it adds build setup that isn't worth it yet.

### Adding a feature

1. Create `src/features/<name>/` with a struct that implements `Feature`.
2. Fill in only the methods it needs: commands, mention prefixes, events, tools, migrations.
3. Add one line to `features::all()`.
4. Add a `[features.<name>]` section to `config.toml` if it needs settings.

Nothing else changes. Memory, for example, is a feature that declares its tables, offers the `memory` chat tool, and adds the list of its files to each question through `chat_context`.

## Feature details

### 1. AI chat

What it does: when someone @-mentions the bot, it builds a request from the message (text, attachment names, embed text, images, video frames), sends it to OpenAI's Responses API with web search and code interpreter turned on, and streams the answer into a Discord reply, editing it about once per second. Long answers are split across several messages. Files produced by the code interpreter are attached to the final message.

Conversation history: in Go, every Discord message ID of a bot reply is stored with its OpenAI response ID (`response_ids` table), and replying to a bot message continues from that ID. That only works for OpenAI, because Claude and Gemini keep no conversation state on their side. VoltBot keeps its own provider-neutral history instead (see "AI providers" below), and OpenAI's response ID becomes an optional shortcut stored next to it.

Progress feedback: Go adds a ⏳ reaction while the model works and swaps it for ✅ at the end. VoltBot drops the status reactions and shows the state in the reply itself, as a small line under the text using Discord's `-#` subtext markdown, updated with the same once-per-second edit that streams the answer:

- `-# 💭 Thinking…` before any text arrives
- `-# 🔧 Reading recent messages…` (one line per tool, named by the tool)
- `-# ✍️ Writing…` while text streams
- the line is removed when the answer is done, or replaced by `-# ⏹️ Stopped` or `-# ⚠️ Something went wrong: <short reason>`

When an answer spans several messages, only the last one carries the line. This also saves two reaction API calls per message part.

Events and guards: `on_mention` as the fallback for mentions no other feature claimed (bot authors are filtered out by the dispatcher); `on_reaction_add` for ❌/🔁; `on_message` for chiming in.

Chiming in (`chat/chime.rs`, Cherry's idea, 2026-10-09): after a message in a server, a roll with `chime_chance` (default 0.03) and a per-channel cooldown (`chime_cooldown_minutes`, default 60) decides whether Vivy reads along. Messages that mention her or reply to her are skipped, since they get a real answer. She reads the last 25 messages, gets the same system prompt and tools as an answer (so the cache is shared) plus her self notes and the memory file list, and answers `PASS`, `REACT <emoji>` or one short line. The instructions go in the question, not the system prompt. A posted line is stored as a question (the transcript) and an answer, so replying to it continues the conversation. The tool loop without Discord output is `ai::complete`, shared with memory's reflection. She may also ask a short question about what people are talking about when she doesn't know it (Cherry, 2026-10-09), and save the answer when it comes.

Follow-ups (`chat/follow_up.rs`, 2026-10-09): the `schedule_follow_up` tool (0.1 to 1440 hours, at most 5 pending per person) stores a row in `chat_follow_ups` (chat migration 2). One task sleeps until the next one is due (at most an hour; a new one wakes it), takes the due rows, and runs `chime::speak` in that channel with instructions to `PASS` if it was already discussed, otherwise to write one warm message starting with the person's mention (the only mention allowed). Each is tried once.

Server emoji (`chat/emoji.rs`, Cherry's design, 2026-10-09): a task waits 30 seconds after start, then once a day describes each custom emoji that has no row in `chat_emoji` yet: it downloads the picture from Discord's CDN (frames for an animated one) and asks the model for one line. So every emoji is described on the first start, and later only new ones. `list_server_emoji` returns the server's emoji codes with their descriptions.

Provider trait: the parts that differ per provider are building the input, streaming the output, continuing a conversation, and returning generated files. Those go behind a trait (see "AI providers" below). Discord streaming, message splitting, media extraction, the tool loop and the bot's own tools stay outside it so every provider reuses them.

Prompt caching: the system prompt is fully static. The Go bot appended the current time, channel name and memory context to the instructions, which come first in every request, so the cache broke there and the conversation history after it was never reused. VoltBot drops memory and gives the model tools to look up the time and channel instead. The tool list is also static and in a fixed order, since it is part of the cached prefix.

Reaction controls: ❌ on a bot reply cancels a running answer (through a cancellation token kept per reply) and deletes a finished one, and 🔁 regenerates it from the same input, editing the old answer's messages in place. Only the person who asked can use them. The bot removes the 🔁 again, so it can be used for the next try.

Config: model name, reasoning effort, verbosity and service tier come from `[ai.openai]` in `config.toml`, not constants in code. The key is `OPENAI_TOKEN` in `.env` (with an optional `OPENAI_BASE`); without it chat answers that it is turned off.

GIFs from Discord's picker (Klipy since Tenor's API closed, also Giphy) are a link to the GIF's page plus a `gifv` embed with an MP4 `video` and a still `thumbnail`; `util::media` reads the MP4 and skips the still, whatever the provider. Discord can add link previews in a later message update, so if a mention has links but no embeds yet, chat waits two seconds and fetches the message again before reading its media.

Message splitting: one splitter replaces the Go bot's two (`SplitParagraph` and `SplitMessageSlices`). It splits on paragraph, then line, then character boundaries, and re-opens code blocks it cuts. The Go code cuts at byte offsets; Rust panics when a string is sliced inside a multi-byte character such as an emoji, so the Rust version must only cut on `char` boundaries. This is a good function to write tests for first.

#### Chat tools

The provider's built-in tools stay on (web search, code interpreter). On top of those, the bot offers its own function tools, which work the same with every provider.

Each feature can contribute tools, the same way it subscribes to events, so reminder tools live in the reminders module and wheel tools in the movie wheel module. A tool runs as the person who asked: it only sees channels they can see and only changes their own reminders.

| Tool | Feature | What it returns or does |
|---|---|---|
| `get_current_time` | Chat | Current date and time in the asker's timezone (or one the model passes) |
| `get_channel_info` | Chat | Channel name, topic, and parent channel for threads |
| `list_channels` | Chat | Every channel the asker can see, by category, with topics |
| `get_user_info` | Chat | A member's display name, timezone, roles and join date |
| `read_recent_messages` | Chat | The last N messages in the current channel, as text with author names |
| `get_message` | Chat | One message from a Discord message link |
| `search_messages` | Chat | Discord's server-wide message search (words, author, channel, attachment type, dates), filtered to channels the asker can read, with message links |
| `get_pinned_messages` | Chat | The current channel's pins |
| `list_server_events` | Chat | The server's upcoming and ongoing scheduled events |
| `create_reminder` | Reminders | Creates a reminder; `when` is text like "in 2h" or "friday 3pm", parsed by the same parser as typed reminders, and a parse error is returned so the model can retry |
| `list_reminders` | Reminders | The asker's pending reminders |
| `cancel_reminder` | Reminders | Deletes one of the asker's reminders |
| `list_server_emoji` | Chat | The server's custom emoji, with a description of each picture |
| `schedule_follow_up` | Chat | Plans a check-in with someone after something they mentioned |
| `get_wheel_status` | Movie wheel | Current round, options, bets and balances (read only) |
| `memory` | Memory | View, create, edit, delete and rename notes under `/memories` (same commands as Anthropic's memory tool) |

The tool loop: when the model asks for a tool, the bot runs it, sends the result back, and keeps streaming. The status line under the reply shows which tool is running.

Go helpers and what replaces them. Most come from serenity, poise or a well-known crate; only a few small functions are written by hand:

| Go helper | In VoltBot |
|---|---|
| `ResolveMentions`, `CleanMessage` | `serenity::utils::content_safe` (turns `<@id>` into names) |
| `MessageMentionsUser`, `IsBotDirectedMessage` | `Message::mentions_user_id` |
| `GetReferencedMessage`, `IsReplyToUser` | `Message::referenced_message`, which Discord already sends with a reply; older turns come from `chat_turns` |
| `GetMessagesBefore`, `GetChannelMessages` | `ChannelId::messages` with `GetMessages::new().before(id).limit(n)` |
| `discord/discord.go` (send, edit, defer, followup, ephemeral) | poise: `ctx.defer_ephemeral()`, `ctx.send(CreateReply::default().ephemeral(true))`, and `.edit()` on the returned handle |
| `suppressLinkEmbeds` | the `SUPPRESS_EMBEDS` message flag on bot replies |
| `IsAdmin` | a poise `check` function on admin commands, reading admin IDs from config |
| Modal plumbing in `handler/modals.go` | poise's `Modal` derive and `execute_modal` (the wheel's bet amount) |
| `SplitParagraph`, `SplitMessageSlices` | the `text-splitter` crate's `MarkdownSplitter` (char-safe, prefers paragraph and line breaks, keeps code blocks whole when they fit) plus a small hand-written wrapper that closes and reopens a code fence it had to cut |
| `URLToExt`, `MediaType`, `IsImageURL`, `IsVideoURL` | `util::media::classify`: the attachment's own `content_type` from Discord first, then the URL's extension, checked against one small table of supported types |
| `DownloadBytes` | `reqwest` |
| `image.go` (decode, GIF frames, PNG grid, base64) | `util::frames`: one `ffmpeg` run per GIF or video picks about 3 frames per second and tiles them into PNG grids (`fps`, `scale`, `pad`, `tile` filters), so no image code is needed; the `base64` crate for data URLs |
| `video.go` (duration, frame at time) | the same `util::frames` run, with `ffprobe` for the duration, through `tokio::process::Command`. Go started one ffmpeg process per frame (up to 910); this is one per file. `ffmpeg-next` would need FFmpeg's C libraries at build time |
| `AttachmentText`, `EmbedText` | hand-written; a few lines each over serenity's types |
| `strings.go` | the standard library |
| YouTube and PDF URL handling | dropped, as the Go OpenAI path already ignores them. Claude reads PDFs, so this can return with the Claude provider |

Other crates that save hand-written code: `dotenvy` (`.env`), `tracing` + `tracing-subscriber` (logging, see "Logging and error reporting"), `anyhow` (errors), `tokio-util`'s `CancellationToken` (❌ stops an answer).

Storage: `chat_turns` and `chat_messages` (see "AI providers" below).

OpenAI is called over plain `reqwest` + `serde`, with a small SSE parser in `ai/sse.rs`, rather than a client crate. The Claude provider needs the same pieces, so they are written once, and the Responses API's events are easy to read straight from its JSON.

#### AI providers

**Claude: use the Claude API, not the Agent SDK.** There is no official Rust Agent SDK; Anthropic ships it for Python and TypeScript only. The Rust crates under that name are community projects, and they work by starting the Claude Code CLI as a subprocess, so the bot server would need Node.js and Claude Code installed and would start a process per message. The Agent SDK is built for coding agents: its built-in tools read and write files and run shell commands on the machine, which is the wrong thing to hand to Discord users. It also needs an API key, since Claude subscription logins aren't allowed for apps other people use, so it doesn't save money. Its sessions are local transcript files on disk resumed by session ID, which is a different model from Discord reply chains.

The plain Claude API (Messages API) has everything the bot needs: streaming, images, custom tools, server-side web search and web fetch, code execution, adaptive thinking with an effort setting, and prompt caching. There is no official Rust client SDK either, so the Claude provider calls the HTTP API with `reqwest` + `serde` and parses the SSE stream. That is a few hundred lines and doubles as a good learning exercise. (Community crates exist, but they tend to lag behind new API features.)

**Conversation state differs per provider.** OpenAI can keep the conversation on its side (`previous_response_id`). Claude and Gemini are stateless: every request sends the whole history, and prompt caching makes the repeated part cheap. The provider trait therefore always receives the full history, plus an optional continuation ID the provider may use instead:

```rust
enum Input {
    Full(Vec<Turn>),                                // provider-neutral, oldest first
    After { continuation: String, new: Vec<Turn> }, // only if this provider and model wrote it
}

struct ChatRequest {
    system: String,          // static, cache friendly
    input: Input,
    tools: Vec<ToolDef>,     // the bot's own tools, fixed order
    cache_key: String,
}

enum ChatEvent {
    TextDelta(String),
    Activity(Activity),      // thinking, searching the web, running code, writing
    Done(Done),              // continuation, tool calls, generated files, raw output
}

#[async_trait]
trait ChatProvider: Send + Sync {
    fn name(&self) -> &'static str;   // "openai", "claude", "gemini"
    fn model(&self) -> &str;
    async fn stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<Result<ChatEvent>>>;
    async fn download_file(&self, file: &GeneratedFile) -> Result<Vec<u8>>;
}
```

The tool loop lives outside the trait: when `Done` lists tool calls, the bot runs them, sends their results as the next input, and calls `stream` again (at most 8 rounds per answer). Each provider converts `Turn` into its own wire format and handles its own caching details (Claude needs `cache_control`; OpenAI caches automatically with `prompt_cache_key`).

**One history table, marked per provider.** Replaces Go's `response_ids`:

`chat_turns (id PK, parent_id, role, author_id, channel_id, content_json, native_json NULL, provider, model, continuation_id NULL, created_at)`
`chat_messages (message_id PK, turn_id)`

Every user message the bot answers and every bot answer gets a turn. A turn has its own ID rather than a Discord message ID, because a long answer spans several messages (all of them point at the turn through `chat_messages`, so replying to any part continues the conversation) and a regenerated answer reuses the old answer's messages. `content_json` holds provider-neutral content (text and media links). `provider` says which provider wrote the turn, and `continuation_id` holds OpenAI's response ID when there is one. When someone replies to a message, the bot walks `parent_id` up the chain (at most 40 turns); a replied-to message the bot hasn't seen, such as someone else's message or an old voltgpt answer, becomes a turn first. If the newest bot turn was written by the current provider and has a `continuation_id`, it sends that. Otherwise it sends the rebuilt history. This means switching providers in the middle of a conversation works, and it no longer depends on fetching old messages from Discord.

**Thinking is stored, but only replayed to the model that wrote it.** For bot turns, `native_json` keeps the provider's raw output for that turn exactly as it came back: Claude's thinking blocks (with their signatures), OpenAI's encrypted reasoning items (requested with `include: ["reasoning.encrypted_content"]`), and the tool calls and results in their original order. When the rebuilt history goes to the same provider and model, the bot sends `native_json` unchanged. That keeps the model's earlier reasoning available, keeps the request prefix byte-identical so the prompt cache still hits, and follows Claude's rule that thinking blocks must be passed back unmodified. For any other provider or model, the bot sends only the neutral `content_json`, because thinking blocks are tied to the model that produced them. (Stage 3 stores `native_json` but doesn't replay it yet: OpenAI continues through `previous_response_id`, which already carries the reasoning. Replaying starts with the Claude provider.) History is append-only: turns are never edited, and 🔁 regenerate adds a new sibling turn under the same parent instead of overwriting. Thinking is never shown in Discord. Image attachments are stored by URL; Discord CDN links expire, so the rebuild refreshes them through Discord's refresh-urls endpoint before sending. Only the newest four turns with media send it again; older ones say an image was there.

### 2. Reminders

What it does: `@Vivy remind me in 2h30m do the thing` or `@Vivy remind me at 16:30 CET do the thing` stores a reminder (with any attached images) and pings the user in the same channel when it is due, with a link back to the message that set it. Images are kept up to the bot's upload limit in that server, which follows its boost level (10 MB, 50 MB at level 2, 100 MB at level 3); anything bigger is named in the confirmation and counted as missing in the fired reminder. `/reminders` lists your pending reminders with a select menu to delete one.

Events and guards: `on_mention` through the mention prefixes `remind me`, `reminder` and `remind`; slash commands `/reminders` and `/timezone`; the delete menu and snooze buttons (`reminders:` custom IDs); `start` runs the scheduler; publishes `BotEvent::ReminderFired`.

Parsing: a real grammar written with the `winnow` parser-combinator crate, replacing the Go regex and prefix checks. Small parsers (`number`, `unit`, `duration`, `clock_time`, `date`, `weekday`, `timezone`) combine into one `when` parser, each with its own unit tests. When parsing fails, the error says which word it got stuck on. Supported forms:

- Relative: `in 2h30m`, `in 1 week 2 days`
- Clock times: `at 16:30`, `at 3pm`, `at noon`, `at midnight`
- Days: `tomorrow`, `friday`, `next friday`, `on 2026-12-24`, optionally with `at <time>`
- Timezone after a time: IANA names (`Europe/Oslo`) or common abbreviations. `CET`/`CEST` and similar map to a real zone so summer time is handled.
- The time can come before or after the message: `remind me in 2h to check the oven` and `remind me to check the oven in 2h`

Port the Go parser's test cases as the starting test suite. Existing reminders are imported from `old.db` (see "Importing from voltgpt").

Timezone: `/timezone <IANA name>` stores a per-user zone (new `user_settings` table). Times without a zone use it, then fall back to UTC.

Scheduler: one background task instead of a timer per reminder. It loads the next due reminder from SQLite, sleeps until then with `tokio::select!`, and is woken early through a `tokio::sync::Notify` whenever a reminder is added or deleted.

Delivery: a reminder is only marked sent after Discord accepted it. If sending with images fails, it is sent again without them, with an `[image missing]` hint (the link to the original message still has them). If that fails too, it stays and is retried with a growing delay (1 minute, doubling up to 6 hours); after 10 failed sends it is dropped with an error in the log channel that includes its text. The fired message has snooze buttons (10m, 1h, tomorrow) that create a new reminder with the same text and images; sent reminders are kept for a week so those buttons keep working, then purged.

Storage: `reminders` (id, user, channel, guild, message, fire time, created time, source message, missing image count, next try time, attempts, sent time), `reminder_images` (reminder id, filename, data as a BLOB instead of Go's base64 JSON), and the shared `user_settings` (user, timezone).

Likely crates: `winnow`, `chrono`, `chrono-tz`.

### 3. Movie wheel

What it does: a betting game for movie night. Admins add options to the wheel, players claim 100 per round, bet on which option wins, and admins set the winner. Players who bet under 10% of their money get taxed. A status embed shows the round with buttons for claim, bet and winner (in VoltBot, a rendered picture).

Commands: `/wheel_status`, `/wheel_add` (admin), `/insert_bet` (admin), `/reset_wheel` (admin).
Components: `button_currentround`, `button_claim`, `button_bet`, `button_winner`, `menu_bet` (place, remove, winner). Modal: `modal_bet` (amount). In VoltBot these become actions of one `wheel:` custom ID enum (`wheel:claim:<round>`, `wheel:bet:<round>`, and so on).

Events and guards: slash commands, buttons, select menus, modal submit; admin guard on the admin actions.

Storage: Go saves the whole game as one JSON blob in `game_state` and rewrites it after every change. VoltBot uses small tables instead, because seasons, undo, admin edits and the chat tool all need to find or change one claim or bet at a time:

```sql
CREATE TABLE wheel_seasons (id INTEGER PRIMARY KEY, guild_id INTEGER NOT NULL,
                            started_at INTEGER NOT NULL, ended_at INTEGER);
CREATE TABLE wheel_options (season_id INTEGER NOT NULL REFERENCES wheel_seasons(id),
                            user_id INTEGER NOT NULL, PRIMARY KEY (season_id, user_id));
CREATE TABLE wheel_rounds  (id INTEGER PRIMARY KEY, season_id INTEGER NOT NULL REFERENCES wheel_seasons(id),
                            number INTEGER NOT NULL, winner_id INTEGER, resolved_at INTEGER,
                            UNIQUE (season_id, number));
CREATE TABLE wheel_claims  (round_id INTEGER NOT NULL REFERENCES wheel_rounds(id),
                            user_id INTEGER NOT NULL, PRIMARY KEY (round_id, user_id));
CREATE TABLE wheel_bets    (round_id INTEGER NOT NULL REFERENCES wheel_rounds(id),
                            by_id INTEGER NOT NULL, on_id INTEGER NOT NULL,
                            amount INTEGER NOT NULL CHECK (amount > 0),
                            PRIMARY KEY (round_id, by_id, on_id));
```

The keys enforce rules for free: one claim per player per round, one bet per player per option per round (placing it again updates the amount with `INSERT ... ON CONFLICT DO UPDATE`). The active season is the one with `ended_at IS NULL`; a reset sets `ended_at` and starts a new one. Players are everyone who claimed or bet in the season, so there is no separate players table. Balances are still never stored: the bot loads the season's rows into plain structs and runs the ledger function over them. Each change is one small `INSERT`, `UPDATE` or `DELETE` inside a transaction, so nothing has to be kept in memory between commands and no global game mutex is needed.

Rules today, so the port keeps them exact: every round a player can claim 100. Before each payout, a player who bet less than 10% of their money loses 3% of it per missing percentage point (up to 30%). A winning bet pays `amount × (options − 1)`, where options are the wheel options left in that round; a losing bet loses its amount. A player can bet on at most half of the remaining options (rounded up). Integer division truncates, as in Go.

Changes in the port:

- **One ledger function.** Go recomputes a player's money from round 0 for every row of the status embed. VoltBot computes a ledger once, a pure function that folds over the rounds and returns every player's balance, tax and payout per round. The embed, the bet checks and the `get_wheel_status` tool all read from it. It is the first thing to write, with unit tests.
- **Same numbers as today.** A one-time import reads the current `game_state` JSON from voltgpt into the tables as the first season, and a test checks that the ledger produces the same balances the Go bot shows, so the running game carries over.
- **Store user IDs, not user objects.** Go saves the whole Discord user in the JSON, so names and avatars go stale. VoltBot stores IDs and looks names up when it renders the embed.
- **Fix a wrong winner.** Once a winner is set, a new round starts and the old one can no longer be changed, so a mis-click is permanent. Admins get an "Undo winner" action on the latest resolved round, allowed while the new round has no bets yet.
- **Winner without bets.** Go refuses to set a winner when nobody bet ("No bets!"), which blocks the wheel if a movie was watched without bets. VoltBot allows it.
- **Seasons instead of a hard reset.** `/reset_wheel` deletes everything with no confirmation. VoltBot asks for confirmation with a button and archives the old game as a finished season, so past results stay viewable.
- **One game per server.** Go has a single global game. VoltBot keys the game by guild ID; it costs nothing and avoids surprises.
- **No lock held during Discord calls.** Go keeps `gamble.Mu` locked while it calls the Discord API. With the tables above, each action is a short database transaction, and the bot only talks to Discord after it commits.

How it was built (stage 4), where it differs from the plan above:

- **Tables** as above, plus `ON DELETE CASCADE` on every child table and a partial unique index on `wheel_seasons (guild_id) WHERE ended_at IS NULL`, so a server can't have two active seasons.
- **IDs**: buttons `wheel:claim|bet|unbet|winner|undo:<round id>` and `wheel:current`; the menus `wheel:pick:<place|remove|winner>:<round id>:<status message id>`; the bet slip buttons `wheel:stake:<round id>:<option user id>:<status message id>:<percent>` and `wheel:other:<round id>:<option user id>:<status message id>`; the modal `wheel:amount:<round id>:<option user id>:<status message id>`; the reset confirmation `wheel:reset:<0|1>`; Change Name `wheel:rename:<round id>` and its modal `wheel:name:<round id>`. Round IDs are database IDs, so a button on an old message can't change a newer round.
- **Undo winner** is a button on the resolved round's status message, shown while the round after it has no bets. It deletes that new round (with its claims) and reopens the old one.
- **Seasons**: `/reset_wheel keep_options` asks for confirmation with a button, ends the season and starts the next one. `/wheel_status season:<n> round:<n>` shows any round of any season; past seasons have no buttons.
- **Admins** are `admins` in `config.toml`, checked inside each admin command and button (a poise `check` would answer with the "turned off" message).
- **The bet amount modal** is a plain serenity `CreateModal`, because it opens from the bet slip's button, not from a slash command.
- **Names** are looked up when a round is shown (nickname, then global name, then username) and remembered for 10 minutes.
- **The status is a picture**, not an embed: `render.rs` lays the round out as SVG and `resvg` draws it as a PNG, with the Inter font built into the binary (system fonts are the fallback, for emoji in names). An open round shows the standings with each player's tax if the round ended now, the bets grouped by option, and who hasn't claimed or bet yet. A resolved round shows the winner and a table of before, bets, tax, after and change. Resolved rounds are posted as `SPOILER_` files, so the winner is hidden as voltgpt's `||spoiler||` did.
- **After Claim!** the private reply gives the player's money for the round and the smallest bet that avoids the tax.
- **Small differences from voltgpt**: the player list is everyone who claimed or bet this season (voltgpt also listed people who only opened the bet menu).
- **Pool betting** (Cherry, 2026-10-09): with fixed `amount × (options − 1)` payouts, early rounds paid ×10 and more and a lost claim cost nothing, so everyone bet everything every round. Seasons now have `rules` (`classic` or `pool`, migration 2). Under pool rules every bet and tax of a round goes into a pot, the bets on the winner share it by stake (integer division; the rounding rest stays in the pot), and a pot nobody won carries over to the next round. The picture shows the pot and each option's current payout. New seasons use pool; seasons from before, including voltgpt's import, stay classic.
- **Bet cap** (Cherry, 2026-10-09): under pool rules a player's bets in one round add up to at most 50% of their money (`BET_CAP`). A simulation of 12-player games (`wheel::sim`, an ignored test) showed half of the all-in players ending broke without it and none with it; taxes stay in the pot. A percentage typed as a bet amount is of the round's money, so `10%` always avoids the tax.
- **Bet slip** (Cherry, 2026-10-09): Discord modals can't update while typing, so picking an option in Place Bet shows a private slip instead: the option's current odds and pot, the player's money, limit and bet, what the bet would return if the round ended now, and buttons for 10/25/50% (classic: also 100%). Buttons over the limit are disabled; each click places the bet and redraws the slip. "Other amount…" opens the old modal, whose label now shows the limit.
- **Change Name** (Cherry, 2026-10-09): a button in a second row with Help opens a modal for the name the wheel shows for you (up to 32 characters, without @, <, > or backticks). Names are kept per server in `wheel_names` (migration 3) and win over the Discord name in `names::lookup`, so the picture, menus, bet slip and chat tool all use them. An empty name deletes the row.
- **Help button** under the status picture: the season's rules, as a private reply. The chat tool gives the same text.
- **Import**: voltgpt's game becomes the active season of `main_server` from `config.toml`; without it the import waits. If that server already has a game, the import is filed as an ended season instead.

Ideas for later, not part of the port: a `/wheel_spin` command that picks the winner randomly with an animated embed, and a per-player balance history.

### 4. Control panel

What it does: gives admins two channels to watch the bot without logging in to the server.

**Log channel.** The `tracing` layer posts log events here:

- `warn` and `error` always, with the feature, the error chain, and a link to the message that triggered it
- lifecycle lines at `info`: 🟢 started (version, git commit, features loaded, `old.db` import results), 🔴 shutting down, 🔌 gateway reconnected
- the minimum level is configurable, and a burst of the same error is grouped into one message with a count ("×12 in the last minute") instead of one post each

**Status channel.** One message that the bot keeps editing every 60 seconds (well inside Discord's rate limits). Its ID is saved in the database, so after a restart the bot edits the same message instead of posting a new one. It shows:

- 🟢 Online, uptime, version and git commit, gateway latency, server count
- memory use, database size, errors in the last hour and the last 24 hours, and when the last error happened
- one block per feature from its `stats()`: for example, pending reminders and the next one due; chat requests today, tokens used and prompt cache hit rate; the current wheel round and its bet count
- "Updated <t:…:R>" at the bottom. Discord renders that as "12 seconds ago" and keeps counting on its own, so a crashed bot is obvious even though it can't edit the message any more. On a clean shutdown the bot changes the header to 🔴 Offline before it exits.

Config:

```toml
[logging]
discord_channel = 123456789012345678
discord_level = "warn"        # lifecycle lines are always posted

[features.control_panel]
status_channel = 123456789012345678
status_interval_secs = 60
```

Events: `start` runs the refresh loop, which marks the message 🔴 Offline when the shutdown token is cancelled. The log channel is part of the core logging setup (hence its own `[logging]` section), so it works even with the control panel turned off, and errors from startup, before any feature runs, still reach Discord.

Storage: `control_panel_state` (key, value) for the status message ID. Chat records each request's token usage, including cached input tokens, in `chat_usage`, which is where the cache hit rate comes from.

Crates: `sysinfo` (memory use) and a small `build.rs` (git commit in the binary).

## Port order

| Stage | Work | Why this order |
|---|---|---|
| 1 | Core: config (`.env` + `config.toml`), `BotCtx`, SQLite and migrations, the `old.db` importer, poise setup, the dispatcher (mention routing, custom ID routing, gating, error reporting), bot events, plus the control panel (log and status channels) and Reminders (parser, scheduler, timezone, snooze, import) | Reminders touch every event type (message, slash command, buttons, select menu, ready, timers), so they prove the skeleton |
| 2 | Shared helpers: message splitting, sending and editing, media extraction, downloads | Chat needs them and they are easy to test on their own |
| 3 | AI chat behind the provider trait, OpenAI implementation, tool loop and chat tools, reaction controls | The main feature; builds on stages 1 and 2, and reminder tools reuse the stage 1 parser |
| 4 | Movie wheel (ledger, tables, import), plus its `get_wheel_status` tool | Self-contained; mostly embeds, buttons and pure money logic |
| 5 | Memory: a folder of notes per server (and per person in DMs) behind a `memory` tool that matches Anthropic's memory tool | Asked for after stage 4; built on chat's tools |
| Later | Claude provider (Messages API over `reqwest`), then Gemini | When credits arrive |
| Later | Image hashing redesign | TODO |

## Importing from voltgpt (`old.db`)

Copy voltgpt's `voltgpt.db` next to the bot as `old.db` and start the bot. On startup it checks for `old.db`, opens it read-only, and imports what VoltBot uses, each part in one transaction. A `legacy_imports (part, imported_at, rows)` table records each finished part so a restart never imports twice. When every part is done, the file is renamed to `old.db.imported` and kept, so image hashes can be imported later when hashing comes back.

Each feature owns its import function (`reminders::import_legacy`, `wheel::import_legacy`), so the reminders import ships in stage 1 and the wheel import in stage 4. A part whose feature isn't ported yet is simply skipped and picked up on a later start.

| Old table | What happens |
|---|---|
| `reminders` | Imported. IDs become integers, the base64 image JSON is decoded into the new image storage, and `fire_at`/`created_at` are kept. Reminders that came due while the bot was down fire right after startup, same as Go. |
| `game_state` | Imported as the first, still active season of the wheel tables: options, rounds, winners, claims and bets, with user objects reduced to IDs. The Go game has no guild, so it goes to the server in `main_server` in `config.toml`. A test checks that the imported season shows the same balances as the Go bot. |
| `response_ids` | Skipped. They hold only OpenAI response IDs with no message text, and OpenAI drops stored responses after 30 days, so they would rarely still work. Replying to an old bot message starts a fresh conversation that includes the replied-to message. |
| `image_hashes` | Skipped for now; kept in `old.db.imported` for when hashing returns. |
| `users`, memory tables (`guild_user_profiles`, `interaction_notes`, `note_participants`, `channel_buffers`, `memory_job_runs`, `vec_notes`) | Skipped; the new memory starts empty. |

## TODO: image hashing

The Go bot hashes every image and video in the main server after 3 seconds (to let embeds load), stores perceptual hashes, and replies when a near-duplicate is posted. `/hash_server` back-fills a whole server. Revisit once the design is decided; the `img_hash` or `image_hasher` crates are the Rust equivalents of `goimagehash`.

## Memory

voltgpt's memory captured every message, summarized it into notes and profiles, and pasted the matches into the instructions of every request. That bloated the prompt and broke the cache. VoltBot's memory is a folder of text files the model manages itself through one chat tool, `memory`, and nothing is pasted into the instructions.

- **Compatible with Claude.** The tool's commands (`view`, `create`, `str_replace`, `insert`, `delete`, `rename`), arguments and reply texts follow Anthropic's memory tool (`memory_20250818`). On OpenAI it is a normal function tool. The Claude provider will send `{"type": "memory_20250818", "name": "memory"}` instead of the function definition and route the calls to the same code.
- **Folders.** One `/memories` folder per server, shared by everyone in it, and a private one per person in DMs (scope `server:<id>` or `dm:<id>`). The tool suggests a folder per person, `/memories/users/<user id>/`, with `about.md` (name first) and one file per topic (`games.md`, `movies.md`), and a server folder with one file per topic: `/memories/server/channels.md`, `culture.md`, and more as needed (Cherry's idea, 2026-10-09). The model fills the server files from conversations, `search_messages` and `list_channels`, so it learns the environment it's in. `/memories/vivy/` holds Vivy's notes about herself in that server (`personality.md`, `interests.md`, under 2K together), so each server grows its own Vivy; the system prompt tells her to be the Vivy those notes describe (Cherry's idea, 2026-10-09). Small topic files let the model open only what the conversation needs. Chat's `<user>` tags carry the user ID for this.
- **Who decides.** Anyone can add notes about anyone (Cherry's choice, 2026-10-09). A person's own word about themselves replaces what others said, and notes from others name who said them.
- **Caching.** The `chat_context` hook adds `<vivy_self>` (her own notes, at most 3000 characters, only when a conversation starts fresh, since a continued one still has them) and `<memory_files>` after the newest question: the asker's and the server's files with sizes, and one line per other person's folder (at most 50 lines), for that request only. The instructions, tool list and earlier turns stay the same.
- **Limits and safety.** 8 KB per file and 256 KB per folder, paths must stay under `/memories` (no `..`), each command runs in one transaction, and every change is written to `memory_changes` with who asked, the old text and the new text.
- **Daily reflection** (`memory/reflect.rs`). An hourly check finds server folders that changed since their last reflection, at most once a day each (`memory_reflections`, migration 2). Vivy gets the folder listing, her self notes and the list of changed files, with only the memory tool and its own small system prompt, and tidies the folder and updates `/memories/vivy/`. Her changes are logged under the bot's user ID. DMs don't reflect. She also rewrites `/memories/vivy/mood.md` (`mood:` and `status:` lines); its `status:` line becomes her Discord custom status (at most 128 characters). Presence is the same in every server, so the server that reflected last sets it, and on start the newest `mood.md` is used.
- **Weekly diary** (`memory/diary.rs`, 2026-10-09). The same hourly loop posts a diary entry in each channel of `diary_channels` (`[features.memory]`) once a week (`memory_diaries`, migration 3). She gets her self notes, the folder listing and the text of the files changed that week (at most 12K characters), may edit her own notes, and writes a short first-person entry, posted with no pings. A week where nothing changed has no entry.
- **Control.** `/memory show [path]` shows all of your files by default, `/memory forget [file]` deletes one of your files (autocompleted) or your whole folder (in DMs, everything), and admins can `/memory delete` any file or folder.

Tables: `memory_files (scope, path, content, updated_at, updated_by)` and `memory_changes (scope, path, at, user_id, before, after)`. `folder.rs` holds the commands as pure functions on a `BTreeMap` of paths, tested without a database.

## Go code that is not needed

Everything under `memory/` (replaced by the new design), `apis/wavespeed/`, `hasher/` (for now), `handler/memory_digest.go`, the `memory_*`, `draw`, `video` and `hash_server` commands, the `users` table, and the memory tables in `db/db.go`.
