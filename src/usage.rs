//! Usage readings: how much of each rate-limit window an account has consumed.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub const FIVE_HOURS: u64 = 5 * 3600;
pub const ONE_WEEK: u64 = 7 * 24 * 3600;

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
}

/// A usage snapshot for one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub observed_at: Timestamp,
    pub windows: Vec<Window>,
    /// The provider says requests are currently being refused.
    #[serde(default)]
    pub limit_reached: bool,
}

impl Usage {
    /// The window closest to its limit as of `now` (ties go to the one that
    /// resets last, since it blocks longest).
    pub fn binding_window(&self, now: Timestamp) -> Option<&Window> {
        self.windows.iter().max_by(|a, b| {
            a.used_at(now)
                .total_cmp(&b.used_at(now))
                .then_with(|| a.resets_at.cmp(&b.resets_at))
        })
    }

    /// Highest percent used across all windows as of `now`.
    pub fn used_at(&self, now: Timestamp) -> f64 {
        self.binding_window(now).map_or(0.0, |w| w.used_at(now))
    }

    /// Percent left before the tightest window is exhausted.
    pub fn headroom_at(&self, now: Timestamp) -> f64 {
        (100.0 - self.used_at(now)).max(0.0)
    }

    /// Whether the account cannot serve requests right now.
    pub fn is_exhausted_at(&self, now: Timestamp) -> bool {
        if self.used_at(now) >= 100.0 {
            return true;
        }
        // A "limit reached" flag is only trusted until the next window resets.
        self.limit_reached && self.windows.iter().all(|w| w.resets_at.is_none_or(|r| r > now))
    }

    /// When the account becomes usable again: the instant every exhausted
    /// window has reset. `None` if not exhausted or the reset time is unknown.
    pub fn next_relief(&self, now: Timestamp) -> Option<Timestamp> {
        self.windows
            .iter()
            .filter(|w| w.used_at(now) >= 100.0)
            .filter_map(|w| w.resets_at)
            .max()
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
        };
        assert!(u.is_exhausted_at(ts(999)));
        assert_eq!(u.next_relief(ts(999)), Some(ts(1000)));
        assert!(!u.is_exhausted_at(ts(1000)));
        assert_eq!(u.used_at(ts(1000)), 0.0);
    }

    #[test]
    fn labels() {
        let mut w = window(ONE_WEEK, 0.0, 0);
        assert_eq!(w.label(), "weekly");
        w.scope = Some("Opus".into());
        assert_eq!(w.label(), "weekly Opus");
    }
}
