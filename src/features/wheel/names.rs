//! Display names for the embed, menus and chat tool. The wheel stores only user IDs, so
//! names are looked up when a round is shown, and remembered for a few minutes so a busy
//! round doesn't ask Discord for the same members on every click.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serenity::all::{GuildId, UserId};
use tokio::task::JoinSet;

use super::ui::Names;
use crate::core::BotCtx;

/// How long a looked-up name is reused.
const KEEP: Duration = Duration::from_secs(10 * 60);

/// A name and when it was looked up, by server and user.
type Known = HashMap<(GuildId, u64), (String, Instant)>;

/// Names looked up recently.
static KNOWN: LazyLock<Mutex<Known>> = LazyLock::new(Default::default);

/// The server nickname (or global name, or username) of each user.
pub async fn lookup(ctx: &BotCtx, guild: GuildId, users: &[u64]) -> Names {
    let mut names = Names::new();
    let mut missing = Vec::new();
    {
        let known = KNOWN.lock().expect("name cache poisoned");
        for &user in users {
            match known.get(&(guild, user)) {
                Some((name, at)) if at.elapsed() < KEEP => {
                    names.insert(user, name.clone());
                }
                _ if !missing.contains(&user) => missing.push(user),
                _ => {}
            }
        }
    }

    // Look the rest up at the same time.
    let mut lookups = JoinSet::new();
    for user in missing {
        let ctx = ctx.clone();
        lookups.spawn(async move { (user, fetch(&ctx, guild, user).await) });
    }
    let found = lookups.join_all().await;

    let mut known = KNOWN.lock().expect("name cache poisoned");
    for (user, name) in found {
        known.insert((guild, user), (name.clone(), Instant::now()));
        names.insert(user, name);
    }
    names
}

async fn fetch(ctx: &BotCtx, guild: GuildId, user: u64) -> String {
    let id = UserId::new(user);
    // Checks the cache first, then asks Discord.
    if let Ok(member) = guild.member((&ctx.cache, ctx.http.as_ref()), id).await {
        return member.display_name().to_string();
    }
    // Someone who left the server.
    match ctx.http.get_user(id).await {
        Ok(user) => user.display_name().to_string(),
        Err(_) => format!("Unknown ({user})"),
    }
}
