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

Planning. See `docs/feature-map.md` for the features being ported and the order.
