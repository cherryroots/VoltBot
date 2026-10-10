//! `config.toml`: everything that isn't a secret. Secrets (tokens) live in `.env`.
//!
//! Each feature has its own `[features.<name>]` section. The core reads the gating keys
//! (`enabled`, `guilds`, `channels`, ...) from every section; the feature reads its own keys
//! with [`Config::feature`].
//!
//! Problems that don't stop the bot (a missing file, keys nothing reads) become warnings,
//! logged once the log channel is up: see [`Config::startup_warnings`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serenity::all::{ChannelId, GuildId};
use tracing::warn;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// User IDs allowed to run admin commands.
    pub admins: Vec<u64>,
    /// The server voltgpt ran in. voltgpt's movie wheel had one game for the whole bot, so
    /// the import puts it in this server.
    pub main_server: Option<u64>,
    /// The SQLite file.
    pub database: String,
    /// voltgpt's database, imported on startup if it exists.
    pub old_database: String,
    pub logging: LoggingConfig,
    /// Model settings, per provider.
    pub ai: crate::ai::AiConfig,
    /// The raw `[features.<name>]` tables, read by [`Config::feature`].
    features: HashMap<String, toml::Table>,
    /// The gating keys of every feature section, parsed once at load.
    #[serde(skip)]
    gates: HashMap<String, Gate>,
    /// Problems found while loading, for [`Config::startup_warnings`]. Loading happens
    /// before logging is set up, so they can't be logged right away.
    #[serde(skip)]
    warnings: Vec<String>,
    /// Unknown keys already warned about, like `features.chat.enable`, so a feature that
    /// reads its settings again doesn't repeat the warning.
    #[serde(skip)]
    warned: Mutex<HashSet<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            admins: Vec::new(),
            main_server: None,
            database: "voltbot.db".to_string(),
            old_database: "old.db".to_string(),
            logging: LoggingConfig::default(),
            ai: crate::ai::AiConfig::default(),
            features: HashMap::new(),
            gates: HashMap::new(),
            warnings: Vec::new(),
            warned: Mutex::new(HashSet::new()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Channel that receives warnings, errors and start/stop notices.
    pub discord_channel: Option<u64>,
    /// The lowest level posted to that channel: "error", "warn" or "info".
    pub discord_level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig {
            discord_channel: None,
            discord_level: "warn".to_string(),
        }
    }
}

/// Where a feature may run. Every list is optional; an empty list means "no restriction".
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Gate {
    pub enabled: bool,
    /// Only these servers.
    pub guilds: Vec<u64>,
    /// Never these servers.
    pub deny_guilds: Vec<u64>,
    /// Only these channels.
    pub channels: Vec<u64>,
    /// Never these channels.
    pub deny_channels: Vec<u64>,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            enabled: true,
            guilds: Vec::new(),
            deny_guilds: Vec::new(),
            channels: Vec::new(),
            deny_channels: Vec::new(),
        }
    }
}

impl Gate {
    /// Whether the feature may run in this server at all, for work that isn't tied to a
    /// channel, like a daily task.
    pub fn allows_guild(&self, guild: GuildId) -> bool {
        self.enabled
            && (self.guilds.is_empty() || self.guilds.contains(&guild.get()))
            && !self.deny_guilds.contains(&guild.get())
    }

    /// Whether the feature may handle an event from this server (`None` in DMs) and channel.
    /// In a thread, `parent` is the channel the thread is in: the thread counts as both, so
    /// listing a channel in `channels` also allows its threads, and denying a channel also
    /// denies its threads. [`BotCtx::allows`](super::BotCtx::allows) looks the parent up.
    pub fn allows(
        &self,
        guild: Option<GuildId>,
        channel: ChannelId,
        parent: Option<ChannelId>,
    ) -> bool {
        let guild = guild.map(|g| g.get());
        // The thread itself, then its parent channel.
        let ids: Vec<u64> = [Some(channel), parent]
            .into_iter()
            .flatten()
            .map(|c| c.get())
            .collect();
        if !self.enabled {
            return false;
        }
        if !self.guilds.is_empty() && !guild.is_some_and(|g| self.guilds.contains(&g)) {
            return false;
        }
        if guild.is_some_and(|g| self.deny_guilds.contains(&g)) {
            return false;
        }
        if !self.channels.is_empty() && !ids.iter().any(|id| self.channels.contains(id)) {
            return false;
        }
        !ids.iter().any(|id| self.deny_channels.contains(id))
    }
}

/// The keys every feature section may have, read into [`Gate`].
const GATE_KEYS: &[&str] = &[
    "enabled",
    "guilds",
    "deny_guilds",
    "channels",
    "deny_channels",
];

/// The gate key `key` looks like a typo of, like `enabled` for `enable` or
/// `deny_channels` for `deny_channel`.
fn gate_key_like(key: &str) -> Option<&'static str> {
    GATE_KEYS
        .iter()
        .find(|gate| gate.starts_with(key) || key.starts_with(*gate))
        .copied()
}

impl Config {
    /// Reads `path`. A missing file gives the defaults, so the bot can start without one,
    /// with a warning: the defaults turn every feature on everywhere and name no admins.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        if !path.exists() {
            let mut config = Config::default();
            config.warnings.push(format!(
                "{} not found: running with the defaults (every feature on everywhere, no admins). \
Copy config.example.toml to start one.",
                path.display()
            ));
            return Ok(config);
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Config> {
        // `serde_ignored` reports every key the structs don't read, anywhere in the file
        // (`[logging]`, `[ai.claude]`, ...), instead of dropping it silently. A typo is
        // only a warning: it never stops the bot.
        let mut ignored = Vec::new();
        let deserializer = toml::Deserializer::parse(text)?;
        let mut config: Config = serde_ignored::deserialize(deserializer, |path| {
            ignored.push(path.to_string());
        })?;
        for path in ignored {
            config
                .warnings
                .push(format!("config.toml: unknown key `{path}` is ignored"));
        }
        config.read_gates()?;
        Ok(config)
    }

    fn read_gates(&mut self) -> anyhow::Result<()> {
        let mut gates = HashMap::new();
        let mut warnings = Vec::new();
        for (name, table) in &self.features {
            let (gate, _) = self.read_section::<Gate>(name)?;
            gates.insert(name.clone(), gate);
            // The feature's own keys are only known when it reads them (see `feature`),
            // but a near miss of a gate key is a typo in any section.
            for key in table.keys() {
                if let Some(gate_key) = gate_key_like(key).filter(|g| g != key) {
                    let path = format!("features.{name}.{key}");
                    warnings.push(format!(
                        "config.toml: unknown key `{path}` is ignored (did you mean `{gate_key}`?)"
                    ));
                    self.warned.lock().unwrap().insert(path);
                }
            }
        }
        warnings.sort();
        self.warnings.extend(warnings);
        self.gates = gates;
        Ok(())
    }

    /// What to warn about once logging is up: problems found while loading, and
    /// `[features.<name>]` sections that match none of `features` (a typo in the name).
    pub fn startup_warnings(&self, features: &[&str]) -> Vec<String> {
        let mut warnings = self.warnings.clone();
        let mut unknown: Vec<&String> = self
            .features
            .keys()
            .filter(|name| !features.contains(&name.as_str()))
            .collect();
        unknown.sort();
        for name in unknown {
            warnings.push(format!(
                "config.toml: there is no feature named `{name}`, so [features.{name}] is ignored"
            ));
        }
        warnings
    }

    /// The gate of a feature. Features without a config section are enabled everywhere.
    pub fn gate(&self, feature: &str) -> Gate {
        self.gates.get(feature).cloned().unwrap_or_default()
    }

    /// Reads a feature's `[features.<name>]` section into its own settings struct.
    /// Keys that neither the struct nor the gate reads are typos: each is warned about once.
    pub fn feature<T: DeserializeOwned + Default>(&self, name: &str) -> anyhow::Result<T> {
        let (settings, ignored) = self.read_section::<T>(name)?;
        for path in ignored {
            let top = path.split('.').next().unwrap_or_default();
            if GATE_KEYS.contains(&top) {
                continue;
            }
            let path = format!("features.{name}.{path}");
            if self.warned.lock().unwrap().insert(path.clone()) {
                warn!("config.toml: unknown key `{path}` is ignored");
            }
        }
        Ok(settings)
    }

    /// Reads a feature section into `T`, and returns the keys `T` didn't read.
    fn read_section<T: DeserializeOwned + Default>(
        &self,
        name: &str,
    ) -> anyhow::Result<(T, Vec<String>)> {
        let Some(table) = self.features.get(name) else {
            return Ok((T::default(), Vec::new()));
        };
        let mut ignored = Vec::new();
        let settings = serde_ignored::deserialize(toml::Value::Table(table.clone()), |path| {
            ignored.push(path.to_string());
        })
        .with_context(|| format!("invalid [features.{name}] in config.toml"))?;
        Ok((settings, ignored))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates() {
        let config = Config::parse(
            r#"
            [features.wheel]
            guilds = [1]
            deny_channels = [10]

            [features.chat]
            enabled = false
            "#,
        )
        .unwrap();

        let wheel = config.gate("wheel");
        let guild = Some(GuildId::new(1));
        assert!(wheel.allows(guild, ChannelId::new(11), None));
        assert!(!wheel.allows(guild, ChannelId::new(10), None));
        assert!(!wheel.allows(Some(GuildId::new(2)), ChannelId::new(11), None));
        assert!(!wheel.allows(None, ChannelId::new(11), None));
        // A thread (12) in the denied channel is denied too.
        assert!(!wheel.allows(guild, ChannelId::new(12), Some(ChannelId::new(10))));
        assert!(wheel.allows(guild, ChannelId::new(12), Some(ChannelId::new(11))));

        assert!(!config.gate("chat").allows(None, ChannelId::new(1), None));
        assert!(config.gate("unknown").allows(None, ChannelId::new(1), None));
    }

    #[test]
    fn threads_follow_their_channel() {
        let config = Config::parse(
            r#"
            [features.chat]
            channels = [10, 20]
            deny_channels = [21]
            "#,
        )
        .unwrap();
        let chat = config.gate("chat");
        let (here, other) = (Some(ChannelId::new(10)), Some(ChannelId::new(30)));
        // A thread is allowed when it or its channel is listed.
        assert!(chat.allows(None, ChannelId::new(11), here));
        assert!(chat.allows(None, ChannelId::new(20), other));
        assert!(!chat.allows(None, ChannelId::new(11), other));
        // Denied when either is denied, even if the other is listed.
        assert!(!chat.allows(None, ChannelId::new(21), here));
    }

    #[test]
    fn warns_about_unknown_keys() {
        let config = Config::parse(
            r#"
            [features.chat]
            enable = false
            deny_channel = [1]

            [features.reminder]
            "#,
        )
        .unwrap();
        let warnings = config.startup_warnings(&["chat", "reminders"]);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("`features.chat.deny_channel`"));
        assert!(warnings[0].contains("`deny_channels`"));
        assert!(warnings[1].contains("`features.chat.enable`"));
        assert!(warnings[2].contains("[features.reminder]"));
        // The typo doesn't turn the feature off.
        assert!(config.gate("chat").enabled);

        // A key the feature's settings don't read either.
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Settings {
            #[allow(dead_code)]
            interval: u64,
        }
        let config = Config::parse("[features.x]\nenabled = true\nintervall = 5").unwrap();
        config.feature::<Settings>("x").unwrap();
        assert!(
            config
                .warned
                .lock()
                .unwrap()
                .contains("features.x.intervall")
        );
        assert!(config.startup_warnings(&["x"]).is_empty());
    }

    #[test]
    fn unknown_keys_anywhere_only_warn() {
        let config = Config::parse(
            r#"
            admin = [1]

            [logging]
            discord_levl = "info"

            [ai]
            provder = "claude"

            [ai.claude]
            modle = "x"

            [ai.openai]
            verbose = "high"
            "#,
        )
        .unwrap();
        let warnings = config.startup_warnings(&[]);
        for path in [
            "admin",
            "logging.discord_levl",
            "ai.provder",
            "ai.claude.modle",
            "ai.openai.verbose",
        ] {
            let warning = format!("config.toml: unknown key `{path}` is ignored");
            assert!(warnings.contains(&warning), "{path}: {warnings:?}");
        }
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        // The keys that are right still count.
        assert_eq!(config.logging.discord_level, "warn");
    }

    #[test]
    fn missing_file_warns() {
        let config = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
        let warnings = config.startup_warnings(&[]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not found"));
    }

    #[test]
    fn ai_provider() {
        use crate::ai::Provider;
        let default = Config::parse("").unwrap();
        assert_eq!(default.ai.provider, Provider::Openai);
        let claude = Config::parse("[ai]\nprovider = \"claude\"").unwrap();
        assert_eq!(claude.ai.provider, Provider::Claude);
        assert!(Config::parse("[ai]\nprovider = \"gemini\"").is_err());
    }

    #[test]
    fn feature_settings() {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Settings {
            interval: u64,
        }
        let config = Config::parse("[features.x]\nenabled = true\ninterval = 5").unwrap();
        assert_eq!(config.feature::<Settings>("x").unwrap().interval, 5);
        assert_eq!(config.feature::<Settings>("y").unwrap().interval, 0);
    }

    #[test]
    fn example_config_parses() {
        let config = Config::parse(include_str!("../../config.example.toml")).unwrap();
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }
}
