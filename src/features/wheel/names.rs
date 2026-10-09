//! Display names for the picture, menus and chat tool. The wheel stores only user IDs, so
//! names are looked up when a round is shown. Without the members intent each name is a
//! request to Discord, and a server's worth of them can take longer than the 3 seconds an
//! interaction waits. So names are kept: a name older than a few minutes is still shown,
//! and refreshed in the background for next time. [`warm`] looks up the players of every
//! game when the bot starts.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serenity::all::{GuildId, UserId};
use tokio::task::JoinSet;

use super::ui::Names;
use crate::core::BotCtx;

/// After this a name is looked up again, in the background.
const FRESH: Duration = Duration::from_secs(10 * 60);

/// A name and when it was looked up, by server and user.
type Known = HashMap<(GuildId, u64), (String, Instant)>;

/// Names looked up so far.
static KNOWN: LazyLock<Mutex<Known>> = LazyLock::new(Default::default);

/// The server nickname (or global name, or username) of each user. Only users never seen
/// before are waited for.
pub async fn lookup(ctx: &BotCtx, guild: GuildId, users: &[u64]) -> Names {
    let mut names = Names::new();
    let mut missing = Vec::new();
    let mut stale = Vec::new();
    {
        let known = KNOWN.lock().expect("name cache poisoned");
        for &user in users {
            match known.get(&(guild, user)) {
                Some((name, at)) => {
                    names.insert(user, name.clone());
                    if at.elapsed() > FRESH && !stale.contains(&user) {
                        stale.push(user);
                    }
                }
                None if !missing.contains(&user) => missing.push(user),
                None => {}
            }
        }
    }

    if !stale.is_empty() {
        let ctx = ctx.clone();
        tokio::spawn(async move { fetch_all(&ctx, guild, stale).await });
    }
    names.extend(fetch_all(ctx, guild, missing).await);
    names
}

/// Looks the users up and remembers their names.
pub async fn warm(ctx: &BotCtx, guild: GuildId, users: Vec<u64>) {
    fetch_all(ctx, guild, users).await;
}

/// Looks the users up at the same time and remembers their names.
async fn fetch_all(ctx: &BotCtx, guild: GuildId, users: Vec<u64>) -> Names {
    let mut lookups = JoinSet::new();
    for user in users {
        let ctx = ctx.clone();
        lookups.spawn(async move { (user, fetch(&ctx, guild, user).await) });
    }
    let found = lookups.join_all().await;

    let mut known = KNOWN.lock().expect("name cache poisoned");
    for (user, name) in &found {
        known.insert((guild, *user), (name.clone(), Instant::now()));
    }
    found.into_iter().collect()
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
