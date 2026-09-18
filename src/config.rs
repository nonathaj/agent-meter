//! User configuration: `config.toml` in the data directory.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::account::ProviderKind;
use crate::fsutil;

/// Usage percentage at which the watcher looks for a better account.
pub const DEFAULT_THRESHOLD: f64 = 90.0;
/// Seconds between watcher polls.
pub const DEFAULT_POLL_SECS: u64 = 60;
/// How much roomier a candidate must be before a switch is worthwhile, in
/// percentage points. Prevents flapping between two similarly loaded accounts.
pub const DEFAULT_MARGIN: f64 = 5.0;
/// Minimum seconds between automatic switches of the same provider.
pub const DEFAULT_COOLDOWN_SECS: u64 = 300;

/// The shortest cycle allowed.
///
/// This is how often the watcher *wakes*, not how often any one account is
/// asked: each provider states the closest together its own endpoint may be
/// polled, and the slower of the two wins. Waking more often than once a minute
/// would buy nothing, because no provider is polled faster than that.
pub const MIN_POLL_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub watch: WatchConfig,
    /// Per-provider overrides, keyed by provider name.
    #[serde(skip_serializing_if = "Overrides::is_empty")]
    pub provider: Overrides,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct WatchConfig {
    /// Switch away from an account once any of its windows reaches this percent.
    pub threshold: f64,
    /// Seconds between checks.
    ///
    /// How often the watcher wakes. Each provider is polled no faster than its
    /// own endpoint tolerates, so raising this slows everything down but
    /// lowering it cannot speed any provider past its own rate.
    pub poll_secs: u64,
    /// Extra headroom a candidate needs before switching when every account is
    /// already past the threshold.
    pub margin: f64,
    /// Minimum seconds between automatic switches for one provider.
    pub cooldown_secs: u64,
    /// When every account is exhausted, wait for the soonest reset instead of
    /// giving up. Disable to have `watch` exit in that case.
    pub wait_for_reset: bool,
}

/// Per-provider overrides of the watch settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<ProviderConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<ProviderConfig>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.claude.is_none() && self.codex.is_none()
    }

    fn get(&self, provider: ProviderKind) -> Option<&ProviderConfig> {
        match provider {
            ProviderKind::Claude => self.claude.as_ref(),
            ProviderKind::Codex => self.codex.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProviderConfig {
    /// Leave unset to watch this provider whenever it has accounts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            poll_secs: DEFAULT_POLL_SECS,
            margin: DEFAULT_MARGIN,
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
            wait_for_reset: true,
        }
    }
}

impl Config {
    /// Reads `config.toml` from `dir`, falling back to defaults when absent.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::path(dir);
        let Some(bytes) =
            fsutil::read_optional(&path).with_context(|| format!("reading {}", path.display()))?
        else {
            return Ok(Self::default());
        };
        let text =
            String::from_utf8(bytes).with_context(|| format!("{} is not valid UTF-8", path.display()))?;
        let config: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        self.validate()?;
        let path = Self::path(dir);
        let text = toml::to_string_pretty(self).context("serializing the configuration")?;
        fsutil::write_atomic(&path, text.as_bytes(), fsutil::Mode::Private)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn path(dir: &Path) -> std::path::PathBuf {
        dir.join("config.toml")
    }

    fn validate(&self) -> Result<()> {
        check_threshold(self.watch.threshold)?;
        for provider in ProviderKind::ALL {
            if let Some(threshold) = self.provider.get(provider).and_then(|p| p.threshold) {
                check_threshold(threshold)?;
            }
        }
        if !(0.0..=50.0).contains(&self.watch.margin) {
            bail!("watch.margin must be between 0 and 50, got {}", self.watch.margin);
        }
        if self.watch.poll_secs < MIN_POLL_SECS {
            bail!(
                "watch.poll-secs must be at least {MIN_POLL_SECS}. It is how often the watcher \
                 wakes, and no provider is polled faster than its own endpoint tolerates, so a \
                 shorter cycle would wake more often without reading anything new."
            );
        }
        Ok(())
    }

    /// The switch threshold for `provider`.
    pub fn threshold_for(&self, provider: ProviderKind) -> f64 {
        self.provider
            .get(provider)
            .and_then(|p| p.threshold)
            .unwrap_or(self.watch.threshold)
    }

    /// Whether the watcher should manage `provider`.
    pub fn is_enabled(&self, provider: ProviderKind) -> bool {
        self.provider
            .get(provider)
            .and_then(|p| p.enabled)
            .unwrap_or(true)
    }

    /// Applies a `key = value` setting, as `agent-meter config set` does.
    ///
    /// Keys are the dotted paths of the file: `watch.threshold`,
    /// `provider.codex.enabled`, and so on.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let parse = |what: &str| -> Result<f64> {
            value
                .parse()
                .with_context(|| format!("{what} must be a number, got {value:?}"))
        };
        let parse_u64 = |what: &str| -> Result<u64> {
            value
                .parse()
                .with_context(|| format!("{what} must be a whole number, got {value:?}"))
        };
        let parse_bool = |what: &str| -> Result<bool> {
            value
                .parse()
                .with_context(|| format!("{what} must be true or false, got {value:?}"))
        };

        match key.split('.').collect::<Vec<_>>().as_slice() {
            ["watch", "threshold"] => self.watch.threshold = parse(key)?,
            ["watch", "poll-secs"] => self.watch.poll_secs = parse_u64(key)?,
            ["watch", "margin"] => self.watch.margin = parse(key)?,
            ["watch", "cooldown-secs"] => self.watch.cooldown_secs = parse_u64(key)?,
            ["watch", "wait-for-reset"] => self.watch.wait_for_reset = parse_bool(key)?,
            ["provider", name, field] => {
                let provider =
                    ProviderKind::parse(name).with_context(|| format!("unknown provider {name:?}"))?;
                let slot = match provider {
                    ProviderKind::Claude => &mut self.provider.claude,
                    ProviderKind::Codex => &mut self.provider.codex,
                };
                let entry = slot.get_or_insert_with(ProviderConfig::default);
                match *field {
                    "threshold" => entry.threshold = Some(parse(key)?),
                    "enabled" => entry.enabled = Some(parse_bool(key)?),
                    _ => bail!("unknown setting {key:?}"),
                }
            }
            _ => bail!(
                "unknown setting {key:?}. Known keys: watch.threshold, watch.poll-secs, \
                 watch.margin, watch.cooldown-secs, watch.wait-for-reset, \
                 provider.<claude|codex>.threshold, provider.<claude|codex>.enabled"
            ),
        }
        self.validate()
    }
}

fn check_threshold(value: f64) -> Result<()> {
    if !(1.0..=100.0).contains(&value) {
        bail!("threshold must be between 1 and 100, got {value}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(dir.path()).unwrap(), Config::default());
        let mut config = Config::default();
        config.watch.threshold = 80.0;
        config.provider.codex = Some(ProviderConfig {
            enabled: Some(false),
            threshold: Some(95.0),
        });
        config.save(dir.path()).unwrap();
        assert_eq!(Config::load(dir.path()).unwrap(), config);
    }

    #[test]
    fn per_provider_overrides_apply() {
        let config: Config = toml::from_str(
            r#"
            [watch]
            threshold = 85.0
            [provider.codex]
            threshold = 95.0
            enabled = false
            "#,
        )
        .unwrap();
        assert_eq!(config.threshold_for(ProviderKind::Claude), 85.0);
        assert_eq!(config.threshold_for(ProviderKind::Codex), 95.0);
        assert!(config.is_enabled(ProviderKind::Claude));
        assert!(!config.is_enabled(ProviderKind::Codex));
    }

    #[test]
    fn set_applies_and_validates_dotted_keys() {
        let mut config = Config::default();
        config.set("watch.threshold", "75").unwrap();
        config.set("provider.codex.enabled", "false").unwrap();
        config.set("provider.codex.threshold", "95").unwrap();
        assert_eq!(config.watch.threshold, 75.0);
        assert!(!config.is_enabled(ProviderKind::Codex));
        assert_eq!(config.threshold_for(ProviderKind::Codex), 95.0);

        for (key, value) in [
            ("watch.threshold", "500"),
            ("watch.threshold", "high"),
            // Faster than once a minute collects refusals, not readings.
            ("watch.poll-secs", "30"),
            ("watch.poll-secs", "5"),
            ("provider.gemini.threshold", "90"),
            ("watch.nonsense", "1"),
        ] {
            assert!(
                config.clone().set(key, value).is_err(),
                "{key}={value} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_out_of_range_and_unknown_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.watch.threshold = 120.0;
        assert!(config.save(dir.path()).is_err());
        assert!(toml::from_str::<Config>("[watch]\nthreshhold = 90.0").is_err());
    }
}
