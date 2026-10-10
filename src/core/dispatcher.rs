//! Routes Discord events to features.
//!
//! - A message that mentions the bot goes to the feature with the longest matching mention
//!   prefix ("remind me" beats "remind"). An empty prefix matches everything, so a feature
//!   that declares `""` (chat, later) gets every mention nobody else claimed.
//! - Every message also goes to every feature's `on_message`, and every reaction to
//!   `on_reaction_add`.
//! - Buttons, select menus and modals have IDs like `reminders:snooze:12:10`. The part before
//!   the first `:` picks the feature; the rest is passed to it.
//! - Slash commands are handled by poise. [`command_check`] applies the gates and
//!   [`on_error`] reports errors.
//!
//! Each handler runs in its own task, inside a tracing span that names the feature, server,
//! channel and message. If it fails, the error is logged and the user gets a short reply.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::{BoxFuture, CreateReply, FrameworkError};
use serenity::all::{
    ChannelId, ComponentInteraction, CreateAllowedMentions, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, CreateMessage, FullEvent,
    GuildId, Interaction, Message, MessageId, ModalInteraction, Reaction, UserId,
};
use tracing::{Instrument as _, Span, debug, error, info, info_span, warn};

use super::errors::{is_user_error, user_message};
use super::{BotCtx, Context, Error, Feature, Result};

/// Called by poise for every gateway event.
pub async fn handle_event(event: &FullEvent, bot: &BotCtx) -> Result<()> {
    match event {
        FullEvent::Message { new_message } => on_message(bot, new_message),
        FullEvent::ReactionAdd { add_reaction } => on_reaction_add(bot, add_reaction),
        FullEvent::InteractionCreate { interaction } => match interaction {
            Interaction::Component(i) => on_component(bot, i).await,
            Interaction::Modal(i) => on_modal(bot, i).await,
            // Slash commands and autocomplete are poise's.
            _ => {}
        },
        FullEvent::Ready { .. } => {
            // The first Ready is the start itself; later ones are full reconnects.
            static SEEN: AtomicBool = AtomicBool::new(false);
            if SEEN.swap(true, Ordering::Relaxed) {
                info!(target: "lifecycle", "🔌 Reconnected to Discord with a new session");
            }
        }
        FullEvent::Resume { .. } => info!(target: "lifecycle", "🔌 Reconnected to Discord"),
        _ => {}
    }
    Ok(())
}

fn on_message(bot: &BotCtx, msg: &Message) {
    if msg.author.bot {
        return;
    }
    let msg = Arc::new(msg.clone());

    for feature in enabled_features(bot, msg.guild_id, msg.channel_id) {
        let (bot2, msg2) = (bot.clone(), msg.clone());
        run(
            bot,
            feature.name(),
            message_span(feature.name(), &msg),
            ReplyTo::Nobody,
            async move { feature.on_message(&bot2, &msg2).await },
        );
    }

    if !msg.mentions_user_id(bot.bot_id) {
        return;
    }
    let text = strip_mentions(&msg.content, bot.bot_id);
    // Only features that are on here can claim a mention, so a turned-off feature's
    // prefix ("remind me ...") falls through to chat instead of getting no answer.
    let allowed = enabled_features(bot, msg.guild_id, msg.channel_id);
    let Some((feature, rest)) = route_mention(&allowed, &text) else {
        debug!("no feature claimed the mention {text:?}");
        return;
    };
    let (bot2, feature2, msg2, rest) =
        (bot.clone(), feature.clone(), msg.clone(), rest.to_string());
    run(
        bot,
        feature.name(),
        message_span(feature.name(), &msg),
        ReplyTo::Message(msg.channel_id, msg.id),
        async move { feature2.on_mention(&bot2, &msg2, &rest).await },
    );
}

fn on_reaction_add(bot: &BotCtx, reaction: &Reaction) {
    if reaction.user_id == Some(bot.bot_id) {
        return;
    }
    let reaction = Arc::new(reaction.clone());
    for feature in enabled_features(bot, reaction.guild_id, reaction.channel_id) {
        let span = info_span!(
            "reaction",
            feature = feature.name(),
            guild = reaction.guild_id.map(|g| g.get()),
            channel = reaction.channel_id.get(),
            message_id = reaction.message_id.get(),
            user = reaction.user_id.map(|u| u.get()),
        );
        let (bot2, reaction2) = (bot.clone(), reaction.clone());
        run(bot, feature.name(), span, ReplyTo::Nobody, async move {
            feature.on_reaction_add(&bot2, &reaction2).await
        });
    }
}

async fn on_component(bot: &BotCtx, i: &ComponentInteraction) {
    let Some((feature, action)) = route_custom_id(bot, &i.data.custom_id) else {
        // Most likely a button on a message from voltgpt.
        tell_user(bot, &ReplyTo::Component(Box::new(i.clone())), STALE).await;
        return;
    };
    let reply = ReplyTo::Component(Box::new(i.clone()));
    if !bot.gate(feature.name()).allows(i.guild_id, i.channel_id) {
        tell_user(bot, &reply, TURNED_OFF).await;
        return;
    }
    let span = info_span!(
        "component",
        feature = feature.name(),
        guild = i.guild_id.map(|g| g.get()),
        channel = i.channel_id.get(),
        message_id = i.message.id.get(),
        user = i.user.id.get(),
        custom_id = %i.data.custom_id,
    );
    let (bot2, i2, action) = (bot.clone(), i.clone(), action.to_string());
    run(bot, feature.name(), span, reply, async move {
        feature.on_component(&bot2, &i2, &action).await
    });
}

async fn on_modal(bot: &BotCtx, i: &ModalInteraction) {
    let reply = ReplyTo::Modal(Box::new(i.clone()));
    let Some((feature, action)) = route_custom_id(bot, &i.data.custom_id) else {
        tell_user(bot, &reply, STALE).await;
        return;
    };
    if !bot.gate(feature.name()).allows(i.guild_id, i.channel_id) {
        tell_user(bot, &reply, TURNED_OFF).await;
        return;
    }
    let span = info_span!(
        "modal",
        feature = feature.name(),
        guild = i.guild_id.map(|g| g.get()),
        channel = i.channel_id.get(),
        user = i.user.id.get(),
        custom_id = %i.data.custom_id,
    );
    let (bot2, i2, action) = (bot.clone(), i.clone(), action.to_string());
    run(bot, feature.name(), span, reply, async move {
        feature.on_modal(&bot2, &i2, &action).await
    });
}

/// Delivers published [`super::BotEvent`]s to every enabled feature until shutdown.
pub fn spawn_bot_events(bot: &BotCtx) {
    let bot = bot.clone();
    let mut receiver = bot.events.subscribe();
    bot.tasks.clone().spawn(async move {
        loop {
            let event = tokio::select! {
                event = receiver.recv() => event,
                _ = bot.shutdown.cancelled() => break,
            };
            let event = match event {
                Ok(event) => Arc::new(event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("dropped {n} bot events because handlers were too slow");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            for feature in bot.features.iter().filter(|f| bot.gate(f.name()).enabled) {
                let (bot2, feature, event) = (bot.clone(), feature.clone(), event.clone());
                let span = info_span!("bot_event", feature = feature.name(), event = ?event);
                run(&bot, feature.name(), span, ReplyTo::Nobody, async move {
                    feature.on_bot_event(&bot2, &event).await
                });
            }
        }
    });
}

// ---------------------------------------------------------------------------------------
// Running handlers and reporting errors
// ---------------------------------------------------------------------------------------

const STALE: &str = "This button is from an older version of the bot and no longer works.";
const TURNED_OFF: &str = "This is turned off here.";

/// Where to tell the user that something went wrong.
enum ReplyTo {
    Nobody,
    Message(ChannelId, MessageId),
    Component(Box<ComponentInteraction>),
    Modal(Box<ModalInteraction>),
}

/// Runs a handler in its own task. A slow or failing handler never blocks the others, and
/// a panic ends only that task (the panic hook in `logging` logs it).
fn run<F>(bot: &BotCtx, feature: &'static str, span: Span, reply: ReplyTo, handler: F)
where
    F: Future<Output = Result<()>> + Send + 'static,
{
    let bot = bot.clone();
    tokio::spawn(
        async move {
            if let Err(err) = handler.await {
                report(&bot, feature, &reply, &err).await;
            }
        }
        .instrument(span),
    );
}

async fn report(bot: &BotCtx, feature: &str, reply: &ReplyTo, err: &Error) {
    if is_user_error(err) {
        debug!("{feature}: told the user: {err}");
    } else {
        error!("{feature} failed: {err:#}");
    }
    tell_user(bot, reply, &user_message(err)).await;
}

/// Tells the user something: privately (ephemeral) for interactions, as a reply for
/// messages.
async fn tell_user(bot: &BotCtx, reply: &ReplyTo, text: &str) {
    let http = &bot.http;
    let message = || {
        CreateInteractionResponseMessage::new()
            .content(text)
            .ephemeral(true)
    };
    let followup = || {
        CreateInteractionResponseFollowup::new()
            .content(text)
            .ephemeral(true)
    };
    // An interaction can only be responded to once. If the handler already did, follow up.
    let result = match reply {
        ReplyTo::Nobody => return,
        ReplyTo::Message(channel, id) => channel
            .send_message(
                http,
                CreateMessage::new()
                    .content(text)
                    .reference_message((*channel, *id))
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await
            .map(|_| ()),
        ReplyTo::Component(i) => match i
            .create_response(http, CreateInteractionResponse::Message(message()))
            .await
        {
            Ok(()) => Ok(()),
            Err(_) => i.create_followup(http, followup()).await.map(|_| ()),
        },
        ReplyTo::Modal(i) => match i
            .create_response(http, CreateInteractionResponse::Message(message()))
            .await
        {
            Ok(()) => Ok(()),
            Err(_) => i.create_followup(http, followup()).await.map(|_| ()),
        },
    };
    if let Err(err) = result {
        warn!("couldn't tell the user what went wrong: {err}");
    }
}

fn message_span(feature: &'static str, msg: &Message) -> Span {
    info_span!(
        "message",
        feature,
        guild = msg.guild_id.map(|g| g.get()),
        channel = msg.channel_id.get(),
        message_id = msg.id.get(),
        user = msg.author.id.get(),
    )
}

// ---------------------------------------------------------------------------------------
// Routing (pure functions, tested below)
// ---------------------------------------------------------------------------------------

fn enabled_features(
    bot: &BotCtx,
    guild: Option<GuildId>,
    channel: ChannelId,
) -> Vec<Arc<dyn Feature>> {
    bot.features
        .iter()
        .filter(|f| bot.gate(f.name()).allows(guild, channel))
        .cloned()
        .collect()
}

fn route_custom_id<'a>(bot: &BotCtx, custom_id: &'a str) -> Option<(Arc<dyn Feature>, &'a str)> {
    let (name, action) = custom_id.split_once(':')?;
    let feature = bot.features.iter().find(|f| f.name() == name)?;
    Some((feature.clone(), action))
}

/// Removes `<@bot>` and `<@!bot>` from a message.
pub fn strip_mentions(content: &str, bot: UserId) -> String {
    content
        .replace(&format!("<@{bot}>"), "")
        .replace(&format!("<@!{bot}>"), "")
        .trim()
        .to_string()
}

/// Picks the feature whose mention prefix matches the start of `text`, and returns the text
/// after the prefix. The longest prefix wins.
pub fn route_mention<'f, 't>(
    features: &'f [Arc<dyn Feature>],
    text: &'t str,
) -> Option<(&'f Arc<dyn Feature>, &'t str)> {
    let mut best: Option<(&Arc<dyn Feature>, usize, &str)> = None;
    for feature in features {
        for prefix in feature.mention_prefixes() {
            if let Some(rest) = strip_prefix_word(text, prefix)
                && best.is_none_or(|(_, len, _)| prefix.len() > len)
            {
                best = Some((feature, prefix.len(), rest));
            }
        }
    }
    best.map(|(feature, _, rest)| (feature, rest))
}

/// `strip_prefix` that ignores case and only matches whole words: "remind" matches
/// "remind me" and "Remind", but not "reminders".
fn strip_prefix_word<'t>(text: &'t str, prefix: &str) -> Option<&'t str> {
    if prefix.is_empty() {
        return Some(text);
    }
    let head = text.get(..prefix.len())?;
    let rest = &text[prefix.len()..];
    if head.eq_ignore_ascii_case(prefix)
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        Some(rest.trim_start())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------
// poise hooks
// ---------------------------------------------------------------------------------------

/// Runs before every slash command: a command is only allowed where its feature is.
pub fn command_check(ctx: Context<'_>) -> BoxFuture<'_, Result<bool>> {
    Box::pin(async move {
        let Some(feature) = ctx.command().category.as_deref() else {
            return Ok(true);
        };
        Ok(ctx
            .data()
            .gate(feature)
            .allows(ctx.guild_id(), ctx.channel_id()))
    })
}

/// Reports errors from slash commands the same way as from other handlers.
pub fn on_error(error: FrameworkError<'_, super::BotCtx, Error>) -> BoxFuture<'_, ()> {
    Box::pin(async move {
        match error {
            FrameworkError::Command { error, ctx, .. } => {
                let feature = ctx.command().category.as_deref().unwrap_or("core");
                let command = &ctx.command().name;
                if is_user_error(&error) {
                    debug!(feature, "/{command}: told the user: {error}");
                } else {
                    error!(
                        feature,
                        user = ctx.author().id.get(),
                        "/{command} failed: {error:#}"
                    );
                }
                let reply = CreateReply::default()
                    .content(user_message(&error))
                    .ephemeral(true);
                if let Err(err) = ctx.send(reply).await {
                    warn!("couldn't tell the user what went wrong: {err}");
                }
            }
            FrameworkError::CommandCheckFailed { ctx, .. } => {
                let _ = ctx
                    .send(CreateReply::default().content(TURNED_OFF).ephemeral(true))
                    .await;
            }
            other => {
                if let Err(err) = poise::builtins::on_error(other).await {
                    warn!("error while handling a poise error: {err}");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Prefixes(&'static str, &'static [&'static str]);

    #[async_trait]
    impl Feature for Prefixes {
        fn name(&self) -> &'static str {
            self.0
        }
        fn mention_prefixes(&self) -> &'static [&'static str] {
            self.1
        }
    }

    fn features() -> Vec<Arc<dyn Feature>> {
        vec![
            Arc::new(Prefixes("reminders", &["remind", "remind me", "reminder"])),
            Arc::new(Prefixes("chat", &[""])),
        ]
    }

    fn route(text: &str) -> (&'static str, String) {
        let features = features();
        let (feature, rest) = route_mention(&features, text).unwrap();
        (feature.name(), rest.to_string())
    }

    // Ported from voltgpt's TestTrigger.
    #[test]
    fn mention_routing() {
        assert_eq!(
            route("remind me in 2h do the thing"),
            ("reminders", "in 2h do the thing".into())
        );
        assert_eq!(
            route("reminder in 30m meeting"),
            ("reminders", "in 30m meeting".into())
        );
        assert_eq!(
            route("remind in 1h call dad"),
            ("reminders", "in 1h call dad".into())
        );
        assert_eq!(
            route("REMIND ME in 2h uppercase"),
            ("reminders", "in 2h uppercase".into())
        );
        assert_eq!(
            route("Reminder in 1h mixed case"),
            ("reminders", "in 1h mixed case".into())
        );
        assert_eq!(route("what time is it"), ("chat", "what time is it".into()));
        assert_eq!(
            route("remindme no space"),
            ("chat", "remindme no space".into())
        );
        assert_eq!(
            route("reminders are great"),
            ("chat", "reminders are great".into())
        );
        assert_eq!(route(""), ("chat", "".into()));
    }

    #[test]
    fn no_fallback_without_empty_prefix() {
        let features: Vec<Arc<dyn Feature>> = vec![Arc::new(Prefixes("reminders", &["remind"]))];
        assert!(route_mention(&features, "hello").is_none());
    }

    #[test]
    fn mention_stripping() {
        let bot = UserId::new(42);
        assert_eq!(strip_mentions("<@42> remind me", bot), "remind me");
        assert_eq!(strip_mentions("  <@!42>  hi <@7>", bot), "hi <@7>");
    }
}
