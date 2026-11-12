//! The settings panel: what it lists, and what a key does to the setting it
//! is on.
//!
//! Nothing here draws or touches a file. The panel shows the same settings
//! `agent-meter config set` writes, so it works on a [`Config`] and leaves
//! reading and saving it to whoever holds the store.

use crate::account::ProviderKind;
use crate::config::{Config, MIN_POLL_SECS};
use crate::engine::Status;
use crate::timefmt;

/// One line of the panel.
#[derive(Debug, Clone, PartialEq)]
pub enum Setting {
    /// The usage at which an account is left.
    Threshold,
    /// How much better another account must be once all are past it.
    Margin,
    /// The shortest time between two automatic switches.
    Cooldown,
    /// How often usage is checked.
    Poll,
    /// What `watch` does once every account is spent.
    WaitForReset,
    /// Whether this agent's accounts are switched automatically.
    Switching(ProviderKind),
    /// A threshold for this agent alone.
    OwnThreshold(ProviderKind),
    /// Whether this model's own limit is judged as an account's.
    Model(ProviderKind, String),
}

/// What a key asks of the setting it is pressed on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Down,
    Up,
    /// Turn it over: on to off, counted to not.
    Toggle,
    /// Go back to having no value of its own.
    Clear,
}

/// How a setting is changed, which is what the footer has to say.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// Stepped up and down.
    Number,
    /// One of two.
    Choice,
}

const THRESHOLD: (f64, f64) = (1.0, 100.0);
const MARGIN: (f64, f64) = (0.0, 50.0);
/// A cooldown longer than this is switching turned off by another name.
const MAX_COOLDOWN_SECS: u64 = 60 * 60;
const MAX_POLL_SECS: u64 = 60 * 60;
const MINUTE: u64 = 60;

/// Every setting the panel lists, in the order it lists them: those every
/// agent shares, then each agent that has accounts.
///
/// A model is listed where some account's reading has a limit for it, since
/// that is the only way to know a provider meters it on its own — and where
/// the settings already count it, so it can always be turned off again.
pub fn list(config: &Config, statuses: &[Status]) -> Vec<Setting> {
    let mut settings = vec![
        Setting::Threshold,
        Setting::Margin,
        Setting::Cooldown,
        Setting::Poll,
        Setting::WaitForReset,
    ];
    for kind in ProviderKind::ALL {
        if !statuses.iter().any(|status| status.account.provider == kind) {
            continue;
        }
        settings.push(Setting::Switching(kind));
        settings.push(Setting::OwnThreshold(kind));
        settings.extend(
            models(config, statuses, kind)
                .into_iter()
                .map(|model| Setting::Model(kind, model)),
        );
    }
    settings
}

/// The models of `kind` that can be counted, by name, as the provider writes
/// them where it has.
fn models(config: &Config, statuses: &[Status], kind: ProviderKind) -> Vec<String> {
    let metered = statuses
        .iter()
        .filter(|status| status.account.provider == kind)
        .filter_map(|status| status.usage.as_ref())
        .flat_map(|usage| &usage.windows)
        .filter_map(|window| window.scope.as_deref());
    let mut models: Vec<String> = Vec::new();
    for model in metered.chain(config.models_for(kind).iter().map(String::as_str)) {
        let model = model.trim();
        if !model.is_empty() && !models.iter().any(|known| known.eq_ignore_ascii_case(model)) {
            models.push(model.to_string());
        }
    }
    models.sort_by_key(|model| model.to_lowercase());
    models
}

impl Setting {
    /// The agent this setting belongs to, or `None` for one they share.
    pub fn provider(&self) -> Option<ProviderKind> {
        match self {
            Setting::Switching(kind) | Setting::OwnThreshold(kind) | Setting::Model(kind, _) => Some(*kind),
            _ => None,
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Setting::Threshold
            | Setting::Margin
            | Setting::Cooldown
            | Setting::Poll
            | Setting::OwnThreshold(_) => Kind::Number,
            Setting::WaitForReset | Setting::Switching(_) | Setting::Model(..) => Kind::Choice,
        }
    }

    /// Whether this setting has a value of its own that [`Step::Clear`] would
    /// take away.
    pub fn can_clear(&self, config: &Config) -> bool {
        matches!(self, Setting::OwnThreshold(kind) if config.own_threshold(*kind).is_some())
    }

    pub fn label(&self) -> String {
        match self {
            Setting::Threshold | Setting::OwnThreshold(_) => "Switch at".into(),
            Setting::Margin => "Once all are past it, switch for".into(),
            Setting::Cooldown => "Between switches, wait".into(),
            Setting::Poll => "Check every".into(),
            Setting::WaitForReset => "With every account spent, watch".into(),
            Setting::Switching(_) => "Switch automatically".into(),
            Setting::Model(_, model) => format!("Count the {model} limit"),
        }
    }

    pub fn value(&self, config: &Config) -> String {
        match self {
            Setting::Threshold => format!("{:.0}% used", config.watch.threshold),
            Setting::Margin => format!("{:.0} points more room", config.watch.margin),
            Setting::Cooldown => match config.watch.cooldown_secs {
                0 => "no time at all".into(),
                secs => format!("at least {}", timefmt::duration(secs)),
            },
            Setting::Poll => timefmt::duration(config.watch.poll_secs),
            Setting::WaitForReset => if config.watch.wait_for_reset {
                "waits for a reset"
            } else {
                "stops"
            }
            .into(),
            Setting::Switching(kind) => if config.is_switching_on(*kind) {
                "on"
            } else {
                "off"
            }
            .into(),
            Setting::OwnThreshold(kind) => match config.own_threshold(*kind) {
                Some(threshold) => format!("{threshold:.0}% used"),
                None => format!("{:.0}% used, as every agent", config.watch.threshold),
            },
            Setting::Model(kind, model) => if config.counts_model(*kind, model) {
                "yes"
            } else {
                "no"
            }
            .into(),
        }
    }

    /// What the setting does, for whoever is deciding what to set it to.
    pub fn explanation(&self) -> String {
        match self {
            Setting::Threshold => "An account is left once any limit it is judged on is this full, \
                 for one that is not."
                .into(),
            Setting::Margin => "With every account past the mark there is only a least bad one. It \
                 is taken only if it has this much more left, so two alike are not traded back and \
                 forth."
                .into(),
            Setting::Cooldown => "The shortest time between two automatic switches of one agent.".into(),
            Setting::Poll => "How often usage is looked at. No provider is asked more often than \
                 it allows, however short this is."
                .into(),
            Setting::WaitForReset => "What `agent-meter watch` does when no account has anything \
                 left: wait for the first to come back, or stop."
                .into(),
            Setting::Switching(kind) => format!(
                "Whether {} is moved to another account on its own as a limit nears. The same \
                 as pressing w.",
                kind.display_name()
            ),
            Setting::OwnThreshold(kind) => format!(
                "A mark for {} alone, where it should differ from the one every agent shares.",
                kind.display_name()
            ),
            Setting::Model(_, model) => format!(
                "A model's own limit stops that model and nothing else, so it is left out. \
                 Counted, it is the account's: an account near its {model} limit is left, and \
                 one that has spent it is not switched to. For when {model} is what you work in."
            ),
        }
    }

    /// Applies `step` to this setting in `config`, and says whether that
    /// changed anything.
    pub fn change(&self, config: &mut Config, step: Step) -> bool {
        let before = config.clone();
        match (self, step) {
            (Setting::Threshold, Step::Up | Step::Down) => {
                config.watch.threshold = stepped(config.watch.threshold, step, THRESHOLD);
            }
            (Setting::Margin, Step::Up | Step::Down) => {
                config.watch.margin = stepped(config.watch.margin, step, MARGIN);
            }
            (Setting::Cooldown, Step::Up | Step::Down) => {
                config.watch.cooldown_secs = minutes(config.watch.cooldown_secs, step, 0, MAX_COOLDOWN_SECS);
            }
            (Setting::Poll, Step::Up | Step::Down) => {
                config.watch.poll_secs = minutes(config.watch.poll_secs, step, MIN_POLL_SECS, MAX_POLL_SECS);
            }
            (Setting::OwnThreshold(kind), Step::Up | Step::Down) => {
                let threshold = stepped(config.threshold_for(*kind), step, THRESHOLD);
                config.provider.entry(*kind).threshold = Some(threshold);
            }
            (Setting::OwnThreshold(kind), Step::Clear) if config.own_threshold(*kind).is_some() => {
                config.provider.entry(*kind).threshold = None;
            }
            (Setting::WaitForReset, Step::Up | Step::Down | Step::Toggle) => {
                config.watch.wait_for_reset = !config.watch.wait_for_reset;
            }
            (Setting::Switching(kind), Step::Up | Step::Down | Step::Toggle) => {
                let on = config.is_switching_on(*kind);
                config.provider.entry(*kind).enabled = Some(!on);
            }
            (Setting::Model(kind, model), Step::Up | Step::Down | Step::Toggle) => {
                let counted = config.counts_model(*kind, model);
                config.count_model(*kind, model, !counted);
            }
            _ => {}
        }
        *config != before
    }
}

/// `value` a whole point up or down, within `bounds`. A value somebody wrote
/// between two whole ones moves to the next whole one in that direction.
fn stepped(value: f64, step: Step, (least, most): (f64, f64)) -> f64 {
    let next = match step {
        Step::Up => (value + 1.0).floor(),
        _ => (value - 1.0).ceil(),
    };
    next.clamp(least, most)
}

/// `secs` a whole minute up or down, between `least` and `most`.
fn minutes(secs: u64, step: Step, least: u64, most: u64) -> u64 {
    let next = match step {
        Step::Up => (secs / MINUTE + 1) * MINUTE,
        _ => secs.saturating_sub(1).div_euclid(MINUTE) * MINUTE,
    };
    next.clamp(least, most)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::sample_statuses;
    use crate::usage::{ONE_WEEK, Usage, Window};

    /// A reading with a week of its own and one for each of `models`.
    fn metering(models: &[&str]) -> Usage {
        let window = |scope: Option<&str>| Window {
            window_secs: ONE_WEEK,
            scope: scope.map(Into::into),
            used_percent: 10.0,
            resets_at: None,
        };
        Usage {
            observed_at: jiff::Timestamp::from_second(0).unwrap(),
            windows: std::iter::once(window(None))
                .chain(models.iter().map(|model| window(Some(model))))
                .collect(),
            limit_reached: false,
            resets: None,
            counted: Vec::new(),
        }
    }

    /// The panel lists what every agent shares, then each agent that has
    /// accounts, with a line for every model one of them is metered on.
    #[test]
    fn the_panel_lists_each_agent_with_the_models_it_is_metered_on() {
        let mut statuses = sample_statuses(3);
        statuses[0].usage = Some(metering(&["Opus", "Fable"]));
        statuses[1].usage = Some(metering(&["Fable"]));
        statuses[2].account.provider = ProviderKind::Codex;
        let mut config = Config::default();

        let claude = ProviderKind::Claude;
        let codex = ProviderKind::Codex;
        assert_eq!(
            list(&config, &statuses),
            [
                Setting::Threshold,
                Setting::Margin,
                Setting::Cooldown,
                Setting::Poll,
                Setting::WaitForReset,
                Setting::Switching(claude),
                Setting::OwnThreshold(claude),
                Setting::Model(claude, "Fable".into()),
                Setting::Model(claude, "Opus".into()),
                Setting::Switching(codex),
                Setting::OwnThreshold(codex),
            ]
        );

        // A model the settings count stays listed while no reading has it, so
        // it can be turned off, and is not listed twice when one does.
        config.set("provider.claude.models", "fable,Sonnet").unwrap();
        let listed: Vec<Setting> = list(&config, &statuses)
            .into_iter()
            .filter(|setting| matches!(setting, Setting::Model(..)))
            .collect();
        assert_eq!(
            listed,
            [
                Setting::Model(claude, "Fable".into()),
                Setting::Model(claude, "Opus".into()),
                Setting::Model(claude, "Sonnet".into()),
            ]
        );
        assert_eq!(listed[0].value(&config), "yes");
        assert_eq!(listed[1].value(&config), "no");

        // With no accounts there is no agent to have settings.
        assert_eq!(list(&config, &[]).len(), 5);
    }

    #[test]
    fn numbers_step_within_what_the_settings_file_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        let press = |config: &mut Config, setting: &Setting, step: Step, times: usize| {
            let changed = (0..times).filter(|_| setting.change(config, step)).count();
            config
                .save(dir.path())
                .expect("a step never leaves a file that will not read");
            changed
        };

        assert_eq!(press(&mut config, &Setting::Threshold, Step::Down, 5), 5);
        assert_eq!(config.watch.threshold, 85.0);
        assert_eq!(Setting::Threshold.value(&config), "85% used");
        assert_eq!(press(&mut config, &Setting::Threshold, Step::Up, 40), 15);
        assert_eq!(config.watch.threshold, 100.0);
        press(&mut config, &Setting::Threshold, Step::Down, 400);
        assert_eq!(config.watch.threshold, 1.0);

        // One somebody wrote between two whole numbers goes to the next one.
        config.watch.threshold = 87.5;
        press(&mut config, &Setting::Threshold, Step::Up, 1);
        assert_eq!(config.watch.threshold, 88.0);
        config.watch.threshold = 87.5;
        press(&mut config, &Setting::Threshold, Step::Down, 1);
        assert_eq!(config.watch.threshold, 87.0);

        press(&mut config, &Setting::Margin, Step::Down, 400);
        assert_eq!(config.watch.margin, 0.0);
        press(&mut config, &Setting::Margin, Step::Up, 400);
        assert_eq!(config.watch.margin, 50.0);

        // Checking faster than once a minute reads nothing new.
        assert_eq!(press(&mut config, &Setting::Poll, Step::Down, 3), 0);
        assert_eq!(config.watch.poll_secs, 60);
        press(&mut config, &Setting::Poll, Step::Up, 2);
        assert_eq!(Setting::Poll.value(&config), "3m");
        config.watch.poll_secs = 90;
        press(&mut config, &Setting::Poll, Step::Down, 1);
        assert_eq!(config.watch.poll_secs, 60);

        press(&mut config, &Setting::Cooldown, Step::Down, 400);
        assert_eq!(Setting::Cooldown.value(&config), "no time at all");
        press(&mut config, &Setting::Cooldown, Step::Up, 1);
        assert_eq!(Setting::Cooldown.value(&config), "at least 1m");
        press(&mut config, &Setting::Cooldown, Step::Up, 400);
        assert_eq!(config.watch.cooldown_secs, 3600);

        // A key that means nothing to a number changes nothing.
        assert_eq!(press(&mut config, &Setting::Threshold, Step::Toggle, 1), 0);
        assert_eq!(press(&mut config, &Setting::Threshold, Step::Clear, 1), 0);
    }

    #[test]
    fn an_agent_takes_the_shared_threshold_until_it_is_given_its_own() {
        let mut config = Config::default();
        let own = Setting::OwnThreshold(ProviderKind::Codex);
        assert_eq!(own.value(&config), "90% used, as every agent");
        assert!(!own.can_clear(&config));
        assert!(!own.change(&mut config, Step::Clear));

        // Its own starts from the one it had been sharing.
        assert!(own.change(&mut config, Step::Up));
        assert_eq!(config.threshold_for(ProviderKind::Codex), 91.0);
        assert_eq!(config.threshold_for(ProviderKind::Claude), 90.0);
        assert_eq!(own.value(&config), "91% used");
        assert!(own.can_clear(&config));

        assert!(own.change(&mut config, Step::Clear));
        assert_eq!(config.own_threshold(ProviderKind::Codex), None);
        assert_eq!(own.value(&config), "90% used, as every agent");
    }

    #[test]
    fn choices_turn_over_on_any_key_that_changes_them() {
        let mut config = Config::default();
        let claude = ProviderKind::Claude;

        // Unset is off here, as it is for `w`: opening a screen is not an
        // instruction to start moving credentials about.
        let switching = Setting::Switching(claude);
        assert_eq!(switching.value(&config), "off");
        assert!(switching.change(&mut config, Step::Toggle));
        assert!(config.is_switching_on(claude));
        assert!(switching.change(&mut config, Step::Down));
        assert!(!config.is_enabled(claude), "turned off, not merely unset");

        let fable = Setting::Model(claude, "Fable".into());
        assert_eq!(fable.value(&config), "no");
        assert!(fable.change(&mut config, Step::Toggle));
        assert_eq!(config.models_for(claude), ["Fable"]);
        assert_eq!(fable.value(&config), "yes");
        assert!(fable.change(&mut config, Step::Up));
        assert!(config.models_for(claude).is_empty());

        let wait = Setting::WaitForReset;
        assert_eq!(wait.value(&config), "waits for a reset");
        assert!(wait.change(&mut config, Step::Toggle));
        assert_eq!(wait.value(&config), "stops");
    }
}
