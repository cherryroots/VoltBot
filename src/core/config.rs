//! `config.toml`: everything that isn't a secret. Secrets (tokens) live in `.env`.
//!
//! Each feature has its own `[features.<name>]` section. The core reads the gating keys
//! (`enabled`, `guilds`, `channels`, ...) from every section; the feature reads its own keys
//! with [`Config::feature`].

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serenity::all::{ChannelId, GuildId};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// User IDs allowed to run admin commands.
    pub admins: Vec<u64>,
    /// The SQLite file.
    pub database: String,
    /// voltgpt's database, imported on startup if it exists.
    pub old_database: String,
    pub logging: LoggingConfig,
    /// The raw `[features.<name>]` tables, read by [`Config::feature`].
    features: HashMap<String, toml::Table>,
    /// The gating keys of every feature section, parsed once at load.
    #[serde(skip)]
    gates: HashMap<String, Gate>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            admins: Vec::new(),
            database: "voltbot.db".to_string(),
            old_database: "old.db".to_string(),
            logging: LoggingConfig::default(),
            features: HashMap::new(),
            gates: HashMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    /// Whether the feature may handle an event from this server (`None` in DMs) and channel.
    pub fn allows(&self, guild: Option<GuildId>, channel: ChannelId) -> bool {
        let guild = guild.map(|g| g.get());
        let channel = channel.get();
        if !self.enabled {
            return false;
        }
        if !self.guilds.is_empty() && !guild.is_some_and(|g| self.guilds.contains(&g)) {
            return false;
        }
        if guild.is_some_and(|g| self.deny_guilds.contains(&g)) {
            return false;
        }
        if !self.channels.is_empty() && !self.channels.contains(&channel) {
            return false;
        }
        !self.deny_channels.contains(&channel)
    }
}

impl Config {
    /// Reads `path`. A missing file gives the defaults, so the bot can start without one.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Config> {
        let mut config: Config = toml::from_str(text)?;
        config.read_gates()?;
        Ok(config)
    }

    fn read_gates(&mut self) -> anyhow::Result<()> {
        for name in self.features.keys() {
            let gate = self.feature::<Gate>(name)?;
            self.gates.insert(name.clone(), gate);
        }
        Ok(())
    }

    /// The gate of a feature. Features without a config section are enabled everywhere.
    pub fn gate(&self, feature: &str) -> Gate {
        self.gates.get(feature).cloned().unwrap_or_default()
    }

    /// Reads a feature's `[features.<name>]` section into its own settings struct.
    /// Keys the struct doesn't know (like the gating keys) are ignored.
    pub fn feature<T: DeserializeOwned + Default>(&self, name: &str) -> anyhow::Result<T> {
        match self.features.get(name) {
            Some(table) => toml::Value::Table(table.clone())
                .try_into()
                .with_context(|| format!("invalid [features.{name}] in config.toml")),
            None => Ok(T::default()),
        }
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
        assert!(wheel.allows(Some(GuildId::new(1)), ChannelId::new(11)));
        assert!(!wheel.allows(Some(GuildId::new(1)), ChannelId::new(10)));
        assert!(!wheel.allows(Some(GuildId::new(2)), ChannelId::new(11)));
        assert!(!wheel.allows(None, ChannelId::new(11)));

        assert!(!config.gate("chat").allows(None, ChannelId::new(1)));
        assert!(config.gate("unknown").allows(None, ChannelId::new(1)));
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
        Config::parse(include_str!("../../config.example.toml")).unwrap();
    }
}
