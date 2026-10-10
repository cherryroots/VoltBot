//! Vivy: load the config, open the database, connect to Discord, and hand events to the
//! features in `features::all()`.

mod ai;
mod core;
mod features;
mod util;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use chrono::Utc;
use poise::serenity_prelude as serenity;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{error, info, warn};

use crate::core::config::Config;
use crate::core::db::{self, Db};
use crate::core::legacy::{self, Outcome};
use crate::core::logging::{self, LogLine};
use crate::core::{BotCtx, Feature, dispatcher};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Secrets come from `.env` (or the real environment). A missing file is fine.
    let _ = dotenvy::dotenv();
    // VOLTBOT_CONFIG is the name from before the bot was renamed to Vivy.
    let config_path = std::env::var("VIVY_CONFIG")
        .or_else(|_| std::env::var("VOLTBOT_CONFIG"))
        .unwrap_or_else(|_| "config.toml".into());
    let config = Config::load(Path::new(&config_path))?;
    let log_queue = logging::init(&config.logging)?;

    if let Err(err) = run(config, log_queue).await {
        error!("startup failed: {err:#}");
        return Err(err);
    }
    Ok(())
}

async fn run(config: Config, log_queue: mpsc::Receiver<LogLine>) -> anyhow::Result<()> {
    let token =
        std::env::var("DISCORD_TOKEN").context("DISCORD_TOKEN is not set (put it in .env)")?;
    let config = Arc::new(config);
    let features = Arc::new(features::all());
    let web = reqwest::Client::builder()
        .user_agent(concat!("Vivy/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        // A response that sends nothing for this long fails instead of hanging forever.
        // Generous, since AI streams can go quiet while the model thinks (Claude sends a
        // ping now and then). Links from messages are downloaded with `media::safe_client`.
        .read_timeout(Duration::from_secs(5 * 60))
        .build()
        .context("creating the HTTP client")?;

    // Database: open, run every owner's migrations, then import voltgpt's data if present.
    if db::adopt_old_name(Path::new(&config.database)).context("renaming voltbot.db")? {
        info!("renamed voltbot.db to {}", config.database);
    }
    let db = Db::open(&config.database)
        .await
        .context("opening the database")?;
    let migration_features = features.clone();
    db.call(move |conn| {
        db::migrate(conn, "core", db::CORE_MIGRATIONS)?;
        // Claude's saved uploads (see ai/claude/uploads.rs).
        db::migrate(conn, "claude", ai::claude::UPLOAD_MIGRATIONS)?;
        for feature in migration_features.iter() {
            db::migrate(conn, feature.name(), feature.migrations())?;
        }
        Ok(())
    })
    .await
    .context("running migrations")?;
    let import_summary = import_legacy(&db, &features, &config).await;
    info!("database {} is ready", config.database);
    let ai = ai_from_env(&config, &web, &db);
    // With an Admin API key, Claude's spend comes from Anthropic's bill (see ai/spend.rs).
    let admin_key = std::env::var("ANTHROPIC_ADMIN_KEY").unwrap_or_default();
    if let Some(spend) = &ai.spend
        && !admin_key.trim().is_empty()
    {
        if config.ai.claude.billed_spend {
            let billing = ai::claude::billing::Billing::new(web.clone(), admin_key.trim().into());
            tokio::spawn(spend.clone().follow_bill(billing));
        } else {
            spend.set_admin_key(ai::AdminKey::Unused);
        }
    }

    // Slash commands. The category remembers which feature a command belongs to, for gating.
    let mut commands = Vec::new();
    for feature in features.iter() {
        for mut command in feature.commands() {
            set_category(&mut command, feature.name());
            commands.push(command);
        }
    }

    let shutdown = CancellationToken::new();
    let tasks = TaskTracker::new();
    let (events, _) = broadcast::channel(100);

    let options = poise::FrameworkOptions {
        commands,
        on_error: dispatcher::on_error,
        command_check: Some(dispatcher::command_check),
        event_handler: |framework, event| {
            Box::pin(dispatcher::handle_event(event, framework.user_data))
        },
        owners: config
            .admins
            .iter()
            .map(|&id| serenity::UserId::new(id))
            .collect(),
        initialize_owners: false,
        // There are no prefix commands: mentions go to the features through the dispatcher.
        // Without this, poise also reads "@Vivy remind me ..." as a command and warns that
        // it doesn't know "remind".
        prefix_options: poise::PrefixFrameworkOptions {
            mention_as_prefix: false,
            ..Default::default()
        },
        ..Default::default()
    };

    // `setup` runs once, when Discord says we're ready. It builds the `BotCtx` that every
    // handler receives and starts the features' background tasks.
    let setup_shutdown = shutdown.clone();
    let setup_tasks = tasks.clone();
    let framework = poise::Framework::builder()
        .options(options)
        // Owners come from `admins` in config.toml, not from the Discord application.
        .initialize_owners(false)
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                let bot = BotCtx {
                    http: ctx.http.clone(),
                    cache: ctx.cache.clone(),
                    shard_manager: framework.shard_manager().clone(),
                    db,
                    config: config.clone(),
                    ai,
                    web,
                    events,
                    shutdown: setup_shutdown,
                    tasks: setup_tasks,
                    features: features.clone(),
                    bot_id: ready.user.id,
                    started_at: Utc::now(),
                };
                if let Some(channel) = config.logging.discord_channel {
                    let channel = serenity::ChannelId::new(channel);
                    logging::spawn_discord_poster(
                        bot.http.clone(),
                        channel,
                        log_queue,
                        bot.shutdown.clone(),
                        &bot.tasks,
                    );
                }

                // `setup` only ever runs once, so if it failed the bot would stay online
                // but never answer. Slash commands failing to register isn't worth that:
                // the ones from the last start keep working.
                if let Err(err) = register_commands(ctx, ready, framework).await {
                    error!("{err:#}");
                }
                dispatcher::spawn_bot_events(&bot);

                let mut started = Vec::new();
                for feature in features.iter() {
                    if !bot.gate(feature.name()).enabled {
                        continue;
                    }
                    match feature.start(&bot).await {
                        Ok(()) => started.push(feature.name()),
                        Err(err) => error!(feature = feature.name(), "failed to start: {err:#}"),
                    }
                }

                info!(
                    target: "lifecycle",
                    "🟢 **Started** Vivy {} (`{}`) as {} · features: {}{}",
                    crate::core::VERSION,
                    crate::core::GIT_COMMIT,
                    ready.user.name,
                    started.join(", "),
                    import_summary,
                );
                Ok(bot)
            })
        })
        .build();

    let intents =
        serenity::GatewayIntents::non_privileged() | serenity::GatewayIntents::MESSAGE_CONTENT;
    let mut client = serenity::ClientBuilder::new(&token, intents)
        .framework(framework)
        .await
        .context("creating the Discord client")?;
    let shard_manager = client.shard_manager.clone();
    info!("connecting to Discord");

    tokio::select! {
        result = client.start() => result.context("the Discord connection stopped")?,
        () = shutdown_signal() => {
            info!(target: "lifecycle", "🔴 **Shutting down**");
            // Tell background tasks to stop, then give them a moment to finish (the control
            // panel marks itself offline, the log poster sends its last lines).
            shutdown.cancel();
            tasks.close();
            if tokio::time::timeout(Duration::from_secs(10), tasks.wait()).await.is_err() {
                warn!("background tasks didn't stop within 10 seconds");
            }
            shard_manager.shutdown_all().await;
        }
    }
    Ok(())
}

/// Registers the slash commands globally, and removes per-server commands that voltgpt
/// registered under the same application, so they don't show up twice.
/// Marks `command` and its subcommands as `feature`'s. The gate check looks at the command
/// that runs, which for `/memory show` is `show`, so subcommands need it too.
fn set_category(command: &mut poise::Command<BotCtx, crate::core::Error>, feature: &'static str) {
    command.category = Some(feature.into());
    for sub in &mut command.subcommands {
        set_category(sub, feature);
    }
}

async fn register_commands(
    ctx: &serenity::Context,
    ready: &serenity::Ready,
    framework: &poise::Framework<BotCtx, crate::core::Error>,
) -> anyhow::Result<()> {
    poise::builtins::register_globally(ctx, &framework.options().commands)
        .await
        .context("registering slash commands")?;
    for guild in &ready.guilds {
        if let Err(err) = guild.id.set_commands(&ctx.http, Vec::new()).await {
            warn!("couldn't clear old commands in server {}: {err}", guild.id);
        }
    }
    Ok(())
}

/// Sets up the AI providers for chat: the one `provider` under `[ai]` names, and the
/// `fallback` one if it's set. Chat is off when the main provider's key is missing from
/// `.env`; a missing fallback key only means there's no fallback.
fn ai_from_env(config: &Config, web: &reqwest::Client, db: &Db) -> ai::Ai {
    // Claude's spend is tracked whether it's the main provider or the fallback.
    let spend = ai::Spend::new(db.clone(), config.ai.claude.monthly_budget);
    let make = |provider: ai::Provider| -> Option<Arc<dyn ai::ChatProvider>> {
        let name = match provider {
            ai::Provider::Claude => "ANTHROPIC_API_KEY",
            ai::Provider::Openai => "OPENAI_TOKEN",
        };
        let Some(key) = std::env::var(name)
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
        else {
            warn!("{name} is not set in .env, so {provider:?} can't chat");
            return None;
        };
        Some(match provider {
            ai::Provider::Claude => Arc::new(ai::Claude::new(
                web.clone(),
                key,
                config.ai.claude.clone(),
                Some(spend.clone()),
            )),
            ai::Provider::Openai => Arc::new(ai::OpenAi::new(
                web.clone(),
                key,
                std::env::var("OPENAI_BASE").ok().as_deref(),
                config.ai.openai.clone(),
            )),
        })
    };
    let main = make(config.ai.provider);
    let backup = config
        .ai
        .fallback
        .filter(|&fallback| fallback != config.ai.provider)
        .and_then(make);
    if let Some(main) = &main {
        info!("chat uses {} ({})", main.name(), main.model());
    }
    if let Some(backup) = &backup {
        info!("chat falls back to {} ({})", backup.name(), backup.model());
    }
    let uses_claude = [config.ai.provider, config.ai.fallback.unwrap_or_default()]
        .contains(&ai::Provider::Claude);
    ai::Ai::new(main, backup, uses_claude.then_some(spend))
}

/// Imports voltgpt's `old.db` if it's there. Returns a summary for the start notice.
async fn import_legacy(db: &Db, features: &[Arc<dyn Feature>], config: &Arc<Config>) -> String {
    let path = Path::new(&config.old_database);
    match legacy::import(db, features, path, config.clone()).await {
        Ok(None) => String::new(),
        Ok(Some(outcomes)) if outcomes.is_empty() => String::new(),
        Ok(Some(outcomes)) => {
            let mut parts = Vec::new();
            for outcome in outcomes {
                match outcome {
                    Outcome::Imported { part, rows } => parts.push(format!("{part} ({rows} rows)")),
                    Outcome::Failed { part, error } => {
                        error!("importing {part} from {} failed: {error}", path.display());
                        parts.push(format!("{part} failed"));
                    }
                }
            }
            format!(" · imported from {}: {}", path.display(), parts.join(", "))
        }
        Err(err) => {
            error!("importing {} failed: {err:#}", path.display());
            String::new()
        }
    }
}

/// Waits for Ctrl+C, or SIGTERM from `systemctl stop`.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
