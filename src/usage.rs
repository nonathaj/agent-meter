//! Usage readings: how much of each rate-limit window an account has consumed.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub const FIVE_HOURS: u64 = 5 * 3600;
pub const ONE_WEEK: u64 = 7 * 24 * 3600;
/// Providers report window lengths a little loosely, so they are matched with
/// a few minutes of slack rather than exactly.
const WINDOW_TOLERANCE: u64 = 300;

/// How long a limit's window is.
///
/// Both kinds are the same thing — a quota over a window — and the only
/// difference is how long the window is, so the shorter one comes back sooner.
/// That is what the difference is good for: not a rate against a budget, but
/// how soon an account pressed against this limit can work again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    FiveHour,
    Weekly,
    /// Some other length the provider reported.
    Other,
}

/// One rate-limit window, e.g. "5-hour session" or "weekly, Opus only".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Length of the rolling window in seconds.
    pub window_secs: u64,
    /// What the window is restricted to (a model name), or `None` for the
    /// account-wide limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Percent of the window consumed, 0-100 (may exceed 100 when over limit).
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<Timestamp>,
}

impl Window {
    /// What this window measures.
    pub fn kind(&self) -> WindowKind {
        let near = |length: u64| self.window_secs.abs_diff(length) <= WINDOW_TOLERANCE;
        match () {
            _ if near(FIVE_HOURS) => WindowKind::FiveHour,
            _ if near(ONE_WEEK) => WindowKind::Weekly,
            _ => WindowKind::Other,
        }
    }

    /// Whether this window limits the whole account rather than one model.
    ///
    /// The difference is what a limit costs: `weekly` at 100% stops the account,
    /// while `weekly Opus` at 100% only stops one model and leaves the account
    /// able to do most of its work — unless that model is the one the work is
    /// done in, which is for [`Usage::counts`] to say.
    pub fn is_account_wide(&self) -> bool {
        self.scope.is_none()
    }

    /// Short label such as `5h`, `weekly`, or `weekly Opus`.
    pub fn label(&self) -> String {
        let base = match self.window_secs {
            FIVE_HOURS => "5h".to_string(),
            ONE_WEEK => "weekly".to_string(),
            secs => crate::timefmt::duration(secs),
        };
        match &self.scope {
            Some(scope) => format!("{base} {scope}"),
            None => base,
        }
    }

    /// Percent used as of `now`: a window whose reset time has passed is empty.
    pub fn used_at(&self, now: Timestamp) -> f64 {
        match self.resets_at {
            Some(reset) if reset <= now => 0.0,
            _ => self.used_percent.max(0.0),
        }
    }

    /// How many points more of this window are spent than an even pace
    /// through it would have spent by `now`: used less the share of the
    /// window that has gone by.
    ///
    /// `None` when it is not ahead, and when nothing dates the window — no
    /// reset time, or one further off than the window is long — since then
    /// there is no clock to be ahead of.
    pub fn ahead_of_pace(&self, now: Timestamp) -> Option<f64> {
        let resets_at = self.resets_at?;
        let remaining = (resets_at.as_second() - now.as_second()).max(0) as f64;
        if self.window_secs == 0 || remaining > self.window_secs as f64 {
            return None;
        }
        let elapsed = 100.0 * (1.0 - remaining / self.window_secs as f64);
        let ahead = self.used_at(now) - elapsed;
        (ahead > 0.0).then_some(ahead)
    }
}

/// A usage snapshot for one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub observed_at: Timestamp,
    pub windows: Vec<Window>,
    /// The provider says requests are currently being refused.
    #[serde(default)]
    pub limit_reached: bool,
    /// The limit resets the account holds, or `None` where the provider did
    /// not say. Not saying is not the same as holding none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets: Option<Resets>,
    /// The models whose own limits the account is not judged on: the ones the
    /// settings say no work is done in.
    ///
    /// Not something a provider says, so it is no part of a stored reading.
    /// It is put on a reading as it is read back, by [`Usage::ignoring`], so
    /// that everything asking how full an account is gets the same answer.
    #[serde(skip)]
    pub ignored: Vec<String>,
}

/// Limit resets: grants a provider hands out now and then which, when spent,
/// refill an account's windows before they would turn over on their own.
///
/// Anthropic calls one a "limit reset" and OpenAI a "rate limit reset credit";
/// they are the same thing, and both lapse if they are not used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resets {
    /// How many the provider says the account can spend.
    pub available: u32,
    /// What each one is, or `None` where the provider counted them without
    /// describing them. Only grants that still have a reset to spend are kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants: Option<Vec<ResetGrant>>,
}

/// One grant of limit resets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetGrant {
    /// The provider's name for it, cleaned of anything that could repaint the
    /// line it is drawn on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Resets left to spend in this grant.
    pub left: u32,
    /// When the grant lapses, used or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// The account's windows spending one refills; empty where the provider
    /// does not say.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refills: Vec<WindowKind>,
}

impl ResetGrant {
    fn is_live_at(&self, now: Timestamp) -> bool {
        self.left > 0 && self.expires_at.is_none_or(|at| at > now)
    }
}

impl Resets {
    /// None held, as a definite answer rather than an unknown one.
    pub fn none() -> Self {
        Self {
            available: 0,
            grants: Some(Vec::new()),
        }
    }

    /// Builds the count from the grants themselves.
    pub fn from_grants(grants: Vec<ResetGrant>) -> Self {
        Self {
            available: grants.iter().map(|g| g.left).sum(),
            grants: Some(grants),
        }
    }

    /// Resets that can still be spent as of `now`.
    ///
    /// A reading can be hours old, and a grant that lapsed since it was taken
    /// is gone even though the reading still lists it.
    pub fn left_at(&self, now: Timestamp) -> u32 {
        match &self.grants {
            Some(grants) => grants.iter().filter(|g| g.is_live_at(now)).map(|g| g.left).sum(),
            None => self.available,
        }
    }

    /// The grants still worth showing as of `now`, soonest to lapse first.
    pub fn live_grants_at(&self, now: Timestamp) -> Vec<&ResetGrant> {
        let mut live: Vec<_> = self
            .grants
            .iter()
            .flatten()
            .filter(|g| g.is_live_at(now))
            .collect();
        // A grant with no end date sorts last: it is the one that can wait.
        live.sort_by_key(|g| (g.expires_at.is_none(), g.expires_at));
        live
    }

    /// When the first of the resets still held lapses.
    pub fn next_expiry_at(&self, now: Timestamp) -> Option<Timestamp> {
        self.live_grants_at(now).iter().find_map(|g| g.expires_at)
    }
}

impl Usage {
    /// This reading, with the limits of `models` left out of every judgment.
    pub fn ignoring(mut self, models: &[String]) -> Self {
        self.ignored = models.to_vec();
        self
    }

    /// Whether `window` is one the account is judged on.
    ///
    /// Every limit is, a model's own included: to somebody working in that
    /// model, an account that has spent it is spent, and nothing here knows
    /// which model the work is in. The account's own limits always count; a
    /// model's stops counting only where the settings name that model as one
    /// no work is done in.
    pub fn counts(&self, window: &Window) -> bool {
        match &window.scope {
            None => true,
            Some(scope) => !self
                .ignored
                .iter()
                .any(|model| model.trim().eq_ignore_ascii_case(scope.trim())),
        }
    }

    /// The limits the account is judged on. Every judgment is made on these.
    fn account_windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter().filter(|w| self.counts(w))
    }

    /// The window the account is judged on that is closest to its limit as of
    /// `now`.
    pub fn binding_window(&self, now: Timestamp) -> Option<&Window> {
        self.account_windows().max_by(|a, b| {
            a.used_at(now)
                .total_cmp(&b.used_at(now))
                .then_with(|| a.resets_at.cmp(&b.resets_at))
        })
    }

    /// How much of the account's own quota is gone, as of `now`.
    pub fn used_at(&self, now: Timestamp) -> f64 {
        self.binding_window(now).map_or(0.0, |w| w.used_at(now))
    }

    /// The worst figure across every window, every per-model one included.
    ///
    /// For showing rather than deciding: "something here is capped" is true and
    /// worth saying, even where it does not change what the account is good
    /// for.
    pub fn worst_at(&self, now: Timestamp) -> f64 {
        self.windows.iter().map(|w| w.used_at(now)).fold(0.0, f64::max)
    }

    /// Percent left before the account's tightest own window is exhausted.
    pub fn headroom_at(&self, now: Timestamp) -> f64 {
        (100.0 - self.used_at(now)).max(0.0)
    }

    /// Percent left in the window that takes longest to come back.
    ///
    /// The tie-break between accounts that recover at about the same time:
    /// having spent the same short window, the one with more of its long window
    /// left is the one with more to give afterwards.
    ///
    /// Where several windows are that long — a week, and a model's week that
    /// counts — it is the tightest of them that says what is left.
    pub fn slowest_headroom_at(&self, now: Timestamp) -> Option<f64> {
        let longest = self.account_windows().map(|w| w.window_secs).max()?;
        self.account_windows()
            .filter(|w| w.window_secs.abs_diff(longest) <= WINDOW_TOLERANCE)
            .map(|w| (100.0 - w.used_at(now)).max(0.0))
            .min_by(f64::total_cmp)
    }

    /// Whether the account can do no work at all right now.
    ///
    /// Only the windows the account is judged on count.
    pub fn is_exhausted_at(&self, now: Timestamp) -> bool {
        let spent = self.account_windows().any(|w| w.used_at(now) >= 100.0);
        if spent {
            return true;
        }
        // A "limit reached" flag is only trusted until the next window resets.
        self.limit_reached && self.windows.iter().all(|w| w.resets_at.is_none_or(|r| r > now))
    }

    /// When this account is next able to do more work than it can now.
    ///
    /// The **soonest** of the windows it is pressed against, not the worst of
    /// them. An account at 99% of five hours and 95% of a week is held by both,
    /// and in an hour the short window hands it a fresh allowance — so an hour
    /// is when it recovers. Ranking on the worst window would call that five
    /// days, because 95 is the larger number.
    ///
    /// `None` when nothing is pressing against it: there is nothing to wait for.
    pub fn recovers_at(&self, now: Timestamp, threshold: f64) -> Option<Timestamp> {
        self.account_windows()
            .filter(|w| w.used_at(now) >= threshold)
            .filter_map(|w| w.resets_at)
            .min()
    }

    /// When the soonest window of `kind` the account is judged on resets.
    pub fn resets_at(&self, kind: WindowKind) -> Option<Timestamp> {
        self.account_windows()
            .filter(|w| w.kind() == kind)
            .filter_map(|w| w.resets_at)
            .min()
    }

    /// When the account becomes usable again: the instant every exhausted
    /// window has reset. `None` if not exhausted or the reset time is unknown.
    pub fn next_relief(&self, now: Timestamp) -> Option<Timestamp> {
        self.account_windows()
            .filter(|w| w.used_at(now) >= 100.0)
            .filter_map(|w| w.resets_at)
            .max()
    }

    /// Whether this reading has stopped saying anything.
    ///
    /// Once every window it described has turned over, it states no usage —
    /// which is not the same as stating that there is room. An absent
    /// constraint must never be read as headroom.
    pub fn states_nothing_at(&self, now: Timestamp) -> bool {
        !self.windows.is_empty() && self.windows.iter().all(|w| w.resets_at.is_some_and(|r| r <= now))
    }

    /// Age of the reading in seconds.
    pub fn age_secs(&self, now: Timestamp) -> i64 {
        (now.as_second() - self.observed_at.as_second()).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: i64) -> Timestamp {
        Timestamp::from_second(s).unwrap()
    }

    fn window(secs: u64, used: f64, reset: i64) -> Window {
        Window {
            window_secs: secs,
            scope: None,
            used_percent: used,
            resets_at: Some(ts(reset)),
        }
    }

    #[test]
    fn binding_window_and_headroom() {
        let u = Usage {
            observed_at: ts(0),
            windows: vec![window(FIVE_HOURS, 40.0, 1000), window(ONE_WEEK, 70.0, 5000)],
            limit_reached: false,
            resets: None,
            ignored: Vec::new(),
        };
        assert_eq!(u.binding_window(ts(10)).unwrap().window_secs, ONE_WEEK);
        assert_eq!(u.headroom_at(ts(10)), 30.0);
        assert!(!u.is_exhausted_at(ts(10)));
    }

    #[test]
    fn reset_windows_count_as_empty() {
        let u = Usage {
            observed_at: ts(0),
            windows: vec![window(FIVE_HOURS, 100.0, 1000)],
            limit_reached: true,
            resets: None,
            ignored: Vec::new(),
        };
        assert!(u.is_exhausted_at(ts(999)));
        assert_eq!(u.next_relief(ts(999)), Some(ts(1000)));
        assert!(!u.is_exhausted_at(ts(1000)));
        assert_eq!(u.used_at(ts(1000)), 0.0);
    }

    fn grant(left: u32, expires: Option<i64>) -> ResetGrant {
        ResetGrant {
            title: None,
            left,
            expires_at: expires.map(ts),
            refills: Vec::new(),
        }
    }

    /// A reading can be hours old; a reset that lapsed since is not one the
    /// account can still spend, even though the reading lists it.
    #[test]
    fn a_reset_past_its_deadline_is_no_longer_held() {
        let resets = Resets::from_grants(vec![grant(1, Some(2000)), grant(2, Some(1000))]);
        assert_eq!(resets.available, 3);
        assert_eq!(resets.left_at(ts(500)), 3);
        assert_eq!(resets.next_expiry_at(ts(500)), Some(ts(1000)));
        assert_eq!(resets.left_at(ts(1000)), 1);
        assert_eq!(resets.next_expiry_at(ts(1000)), Some(ts(2000)));
        assert_eq!(resets.left_at(ts(2000)), 0);
        assert_eq!(resets.next_expiry_at(ts(2000)), None);
    }

    #[test]
    fn grants_are_listed_soonest_to_lapse_first_and_open_ended_ones_last() {
        let resets = Resets::from_grants(vec![grant(1, None), grant(1, Some(900)), grant(1, Some(100))]);
        let order: Vec<_> = resets
            .live_grants_at(ts(0))
            .iter()
            .map(|g| g.expires_at)
            .collect();
        assert_eq!(order, [Some(ts(100)), Some(ts(900)), None]);
    }

    /// Codex counts its credits before it describes them; the count stands on
    /// its own until then.
    #[test]
    fn a_count_without_a_description_is_still_a_count() {
        let resets = Resets {
            available: 2,
            grants: None,
        };
        assert_eq!(resets.left_at(ts(0)), 2);
        assert_eq!(resets.next_expiry_at(ts(0)), None);
        assert!(resets.live_grants_at(ts(0)).is_empty());
    }

    /// Readings cached before resets were read have no field for them, and
    /// must load as "not said", never as "none held".
    #[test]
    fn a_reading_cached_before_resets_were_read_says_nothing_about_them() {
        let old: Usage =
            serde_json::from_str(r#"{"observed_at":"2026-09-01T00:00:00Z","windows":[]}"#).unwrap();
        assert_eq!(old.resets, None);

        let mut usage = old.clone();
        usage.resets = Some(Resets::from_grants(vec![ResetGrant {
            title: Some("Full reset".into()),
            left: 1,
            expires_at: Some(ts(5000)),
            refills: vec![WindowKind::FiveHour, WindowKind::Weekly],
        }]));
        let json = serde_json::to_string(&usage).unwrap();
        assert!(json.contains(r#""refills":["five_hour","weekly"]"#), "{json}");
        assert_eq!(serde_json::from_str::<Usage>(&json).unwrap(), usage);
    }

    /// Half a week gone with 60% of it spent is ten points ahead of an even
    /// pace; anything not ahead, or with nothing to date the window, says
    /// nothing at all.
    #[test]
    fn pace_is_measured_against_the_clock_of_the_window() {
        let now = ts(1_000_000);
        let half_gone = |used| window(ONE_WEEK, used, 1_000_000 + ONE_WEEK as i64 / 2);
        assert_eq!(half_gone(60.0).ahead_of_pace(now), Some(10.0));
        assert_eq!(half_gone(50.0).ahead_of_pace(now), None, "on pace is not ahead");
        assert_eq!(half_gone(40.0).ahead_of_pace(now), None);

        let undated = Window {
            resets_at: None,
            ..half_gone(90.0)
        };
        assert_eq!(undated.ahead_of_pace(now), None);
        // A reset further off than the window is long dates nothing.
        assert_eq!(
            window(ONE_WEEK, 90.0, 1_000_000 + 2 * ONE_WEEK as i64).ahead_of_pace(now),
            None
        );
        assert_eq!(window(0, 90.0, 1_000_100).ahead_of_pace(now), None);
        // A window that has turned over is empty, and so on no pace at all.
        assert_eq!(window(ONE_WEEK, 90.0, 999_000).ahead_of_pace(now), None);
    }

    /// A model's limit is judged in every way the account's own limits are,
    /// except where its model is ignored.
    #[test]
    fn a_models_limit_is_judged_unless_it_is_ignored() {
        let now = ts(10);
        let scoped = |scope: &str, used, reset| Window {
            scope: Some(scope.into()),
            ..window(ONE_WEEK, used, reset)
        };
        let reading = Usage {
            observed_at: ts(0),
            windows: vec![
                window(FIVE_HOURS, 40.0, 1000),
                // A provider's week is not always a week to the second.
                window(ONE_WEEK + 60, 30.0, 5000),
                scoped("Fable", 100.0, 4000),
                scoped("Opus", 55.0, 6000),
            ],
            limit_reached: false,
            resets: None,
            ignored: Vec::new(),
        };
        // Everything counts: the spent Fable week holds the account.
        assert_eq!(reading.used_at(now), 100.0);
        assert_eq!(reading.worst_at(now), 100.0);
        assert!(reading.is_exhausted_at(now));
        assert_eq!(reading.next_relief(now), Some(ts(4000)));
        assert_eq!(reading.recovers_at(now, 90.0), Some(ts(4000)));
        assert_eq!(reading.resets_at(WindowKind::Weekly), Some(ts(4000)));
        // Of the three weeks it is judged on, the tightest says what is left.
        assert_eq!(reading.slowest_headroom_at(now), Some(0.0));
        // Once its week has turned over it holds the account no longer.
        assert!(!reading.is_exhausted_at(ts(4000)));

        let opus = reading.clone().ignoring(&["Fable".to_string()]);
        assert_eq!(opus.used_at(now), 55.0);
        assert_eq!(opus.binding_window(now).unwrap().scope.as_deref(), Some("Opus"));
        assert_eq!(opus.slowest_headroom_at(now), Some(45.0));
        assert!(!opus.is_exhausted_at(now));
        assert_eq!(opus.resets_at(WindowKind::Weekly), Some(ts(5000)));

        // Matched however it is cased and spaced.
        let neither = reading
            .clone()
            .ignoring(&["fable".to_string(), " OPUS ".to_string()]);
        assert_eq!(neither.used_at(now), 40.0);
        assert_eq!(
            neither.worst_at(now),
            100.0,
            "what is shown is not what is judged"
        );
        assert_eq!(neither.slowest_headroom_at(now), Some(70.0));
        assert!(!neither.is_exhausted_at(now));

        // Which models are ignored is a setting, not part of what was read.
        let stored = serde_json::to_string(&neither).unwrap();
        assert!(!stored.contains("ignored"), "{stored}");
        assert_eq!(serde_json::from_str::<Usage>(&stored).unwrap(), reading);
    }

    #[test]
    fn labels() {
        let mut w = window(ONE_WEEK, 0.0, 0);
        assert_eq!(w.label(), "weekly");
        w.scope = Some("Opus".into());
        assert_eq!(w.label(), "weekly Opus");
    }
}
