//! Deciding when to switch accounts, and to which one.
//!
//! This module is pure: it takes a snapshot of what is known and returns a
//! decision. That keeps the rules — which are the whole point of the tool —
//! testable without touching the network or the filesystem.

use jiff::Timestamp;

use crate::usage::Usage;

/// One account as the policy sees it.
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub id: &'a str,
    /// The most recent usage reading, if any poll has succeeded.
    pub usage: Option<&'a Usage>,
    /// True for the account the CLI is currently signed in to.
    pub active: bool,
    /// Accounts needing a fresh login cannot be switched to.
    pub usable: bool,
}

/// The knobs that shape a decision.
#[derive(Debug, Clone, Copy)]
pub struct Rules {
    /// Usage percentage at which the active account should be left.
    pub threshold: f64,
    /// Extra headroom a candidate needs before switching to it when every
    /// account is already past the threshold.
    pub margin: f64,
    /// A reading older than this is not trusted for a switch decision.
    pub max_reading_age_secs: i64,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            threshold: crate::config::DEFAULT_THRESHOLD,
            margin: crate::config::DEFAULT_MARGIN,
            // Long enough to cover a poll interval and a retry, short enough
            // that a switch is never made on yesterday's numbers.
            max_reading_age_secs: 30 * 60,
        }
    }
}

/// What the policy decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Keep using the current account.
    Stay(Stay),
    /// Switch to this account.
    Switch { to: String, reason: Reason },
    /// A switch is wanted but no account can serve it.
    Blocked(Blocked),
}

/// Why no switch was needed.
#[derive(Debug, Clone, PartialEq)]
pub enum Stay {
    /// The active account is below the threshold.
    BelowThreshold { used: f64 },
    /// Nothing is signed in, so there is nothing to switch away from.
    NoActiveAccount,
    /// The active account has no usable reading, so its load is unknown.
    UsageUnknown,
    /// A candidate exists but is not enough better to be worth the churn.
    NoBetterAccount { used: f64 },
}

/// Why a switch was chosen.
#[derive(Debug, Clone, PartialEq)]
pub enum Reason {
    /// The active account crossed the threshold; the target is below it.
    ThresholdCrossed { used: f64, target_used: f64 },
    /// Every account is past the threshold, but the target has enough more
    /// headroom to be worth moving to.
    BestOfExhausted { used: f64, target_used: f64 },
    /// The active account is refusing requests outright.
    ActiveExhausted { target_used: f64 },
}

/// Why a wanted switch could not happen.
#[derive(Debug, Clone, PartialEq)]
pub enum Blocked {
    /// Every account is fully used up.
    AllExhausted {
        /// When the roomiest account frees up, if any reset time is known.
        relief_at: Option<Timestamp>,
        /// The account that recovers first.
        relief_account: Option<String>,
    },
    /// No other account is available (none stored, all need a login, or none
    /// has a fresh enough reading).
    NoAlternative,
}

/// Decides what to do for one provider.
pub fn decide(candidates: &[Candidate<'_>], rules: &Rules, now: Timestamp) -> Decision {
    let Some(active) = candidates.iter().find(|c| c.active) else {
        return Decision::Stay(Stay::NoActiveAccount);
    };
    let Some(active_usage) = fresh(active, rules, now) else {
        return Decision::Stay(Stay::UsageUnknown);
    };

    let used = active_usage.used_at(now);
    let exhausted = active_usage.is_exhausted_at(now);
    if used < rules.threshold && !exhausted {
        return Decision::Stay(Stay::BelowThreshold { used });
    }

    // Only accounts that can actually serve traffic are worth moving to.
    let mut usable: Vec<(&Candidate<'_>, &Usage)> = candidates
        .iter()
        .filter(|c| !c.active && c.usable)
        .filter_map(|c| Some((c, fresh(c, rules, now)?)))
        .filter(|(_, usage)| !usage.is_exhausted_at(now))
        .collect();

    // Least used first, so the roomiest account leads. Ties go to the id, which
    // keeps the choice deterministic no matter what order accounts arrive in.
    usable.sort_by(|(a_candidate, a), (b_candidate, b)| {
        a.used_at(now)
            .total_cmp(&b.used_at(now))
            .then_with(|| a_candidate.id.cmp(b_candidate.id))
    });

    match usable.first().copied() {
        Some((candidate, usage)) => {
            let target_used = usage.used_at(now);
            let reason = if exhausted {
                Reason::ActiveExhausted { target_used }
            } else if target_used < rules.threshold {
                Reason::ThresholdCrossed { used, target_used }
            } else if target_used + rules.margin <= used {
                // Everything is past the threshold: only move for a worthwhile
                // gain, otherwise two busy accounts ping-pong.
                Reason::BestOfExhausted { used, target_used }
            } else {
                return Decision::Stay(Stay::NoBetterAccount { used });
            };
            Decision::Switch {
                to: candidate.id.to_string(),
                reason,
            }
        }
        None => {
            let alternatives: Vec<_> = candidates
                .iter()
                .filter(|c| !c.active && c.usable)
                .filter_map(|c| Some((c, fresh(c, rules, now)?)))
                .collect();
            if alternatives.is_empty() {
                return Decision::Blocked(Blocked::NoAlternative);
            }
            // Every alternative is exhausted; report when the first one recovers
            // so the caller can wait rather than give up.
            let soonest = alternatives
                .iter()
                .chain(std::iter::once(&(active, active_usage)))
                .filter_map(|(c, usage)| Some((c.id, usage.next_relief(now)?)))
                .min_by_key(|(_, at)| *at);
            Decision::Blocked(Blocked::AllExhausted {
                relief_at: soonest.map(|(_, at)| at),
                relief_account: soonest.map(|(id, _)| id.to_string()),
            })
        }
    }
}

/// The candidate's reading, if it is recent enough to act on.
fn fresh<'a>(candidate: &Candidate<'a>, rules: &Rules, now: Timestamp) -> Option<&'a Usage> {
    candidate
        .usage
        .filter(|u| u.age_secs(now) <= rules.max_reading_age_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{FIVE_HOURS, Window};

    const NOW: i64 = 1_800_000_000;

    fn now() -> Timestamp {
        Timestamp::from_second(NOW).unwrap()
    }

    fn usage(used: f64) -> Usage {
        usage_aged(used, 0)
    }

    fn usage_aged(used: f64, age_secs: i64) -> Usage {
        Usage {
            observed_at: Timestamp::from_second(NOW - age_secs).unwrap(),
            windows: vec![Window {
                window_secs: FIVE_HOURS,
                scope: None,
                used_percent: used,
                resets_at: Timestamp::from_second(NOW + 3600).ok(),
            }],
            limit_reached: false,
        }
    }

    fn candidate<'a>(id: &'a str, usage: Option<&'a Usage>, active: bool) -> Candidate<'a> {
        Candidate {
            id,
            usage,
            active,
            usable: true,
        }
    }

    #[test]
    fn stays_below_the_threshold() {
        let (a, b) = (usage(50.0), usage(10.0));
        let decision = decide(
            &[
                candidate("claude-1", Some(&a), true),
                candidate("claude-2", Some(&b), false),
            ],
            &Rules::default(),
            now(),
        );
        assert_eq!(decision, Decision::Stay(Stay::BelowThreshold { used: 50.0 }));
    }

    #[test]
    fn switches_to_the_roomiest_account_past_the_threshold() {
        let (a, b, c) = (usage(92.0), usage(40.0), usage(12.0));
        let decision = decide(
            &[
                candidate("claude-1", Some(&a), true),
                candidate("claude-2", Some(&b), false),
                candidate("claude-3", Some(&c), false),
            ],
            &Rules::default(),
            now(),
        );
        assert_eq!(
            decision,
            Decision::Switch {
                to: "claude-3".into(),
                reason: Reason::ThresholdCrossed {
                    used: 92.0,
                    target_used: 12.0
                },
            }
        );
    }

    #[test]
    fn when_everything_is_busy_it_moves_only_for_a_worthwhile_gain() {
        let rules = Rules::default();

        // 93 -> 91 is only 2 points: not worth the churn.
        let (a, b) = (usage(93.0), usage(91.0));
        let decision = decide(
            &[
                candidate("codex-1", Some(&a), true),
                candidate("codex-2", Some(&b), false),
            ],
            &rules,
            now(),
        );
        assert_eq!(decision, Decision::Stay(Stay::NoBetterAccount { used: 93.0 }));

        // 99 -> 91 clears the margin.
        let (a, b) = (usage(99.0), usage(91.0));
        let decision = decide(
            &[
                candidate("codex-1", Some(&a), true),
                candidate("codex-2", Some(&b), false),
            ],
            &rules,
            now(),
        );
        assert_eq!(
            decision,
            Decision::Switch {
                to: "codex-2".into(),
                reason: Reason::BestOfExhausted {
                    used: 99.0,
                    target_used: 91.0
                },
            }
        );
    }

    #[test]
    fn an_exhausted_active_account_moves_even_to_a_busy_one() {
        let (a, b) = (usage(100.0), usage(96.0));
        let decision = decide(
            &[
                candidate("claude-1", Some(&a), true),
                candidate("claude-2", Some(&b), false),
            ],
            &Rules::default(),
            now(),
        );
        assert_eq!(
            decision,
            Decision::Switch {
                to: "claude-2".into(),
                reason: Reason::ActiveExhausted { target_used: 96.0 },
            }
        );
    }

    #[test]
    fn reports_when_every_account_is_exhausted() {
        let a = usage(100.0);
        let mut b = usage(100.0);
        b.windows[0].resets_at = Timestamp::from_second(NOW + 600).ok();
        let decision = decide(
            &[
                candidate("claude-1", Some(&a), true),
                candidate("claude-2", Some(&b), false),
            ],
            &Rules::default(),
            now(),
        );
        assert_eq!(
            decision,
            Decision::Blocked(Blocked::AllExhausted {
                relief_at: Timestamp::from_second(NOW + 600).ok(),
                relief_account: Some("claude-2".into()),
            })
        );
    }

    #[test]
    fn stale_readings_are_not_acted_on() {
        let rules = Rules::default();
        let stale_active = usage_aged(95.0, rules.max_reading_age_secs + 1);
        let fresh_other = usage(5.0);
        assert_eq!(
            decide(
                &[
                    candidate("claude-1", Some(&stale_active), true),
                    candidate("claude-2", Some(&fresh_other), false)
                ],
                &rules,
                now()
            ),
            Decision::Stay(Stay::UsageUnknown)
        );

        // A stale candidate is not a switch target either.
        let active = usage(95.0);
        let stale_other = usage_aged(5.0, rules.max_reading_age_secs + 1);
        assert_eq!(
            decide(
                &[
                    candidate("claude-1", Some(&active), true),
                    candidate("claude-2", Some(&stale_other), false)
                ],
                &rules,
                now()
            ),
            Decision::Blocked(Blocked::NoAlternative)
        );
    }

    #[test]
    fn accounts_needing_a_login_are_skipped() {
        let (a, b) = (usage(95.0), usage(5.0));
        let decision = decide(
            &[
                candidate("claude-1", Some(&a), true),
                Candidate {
                    id: "claude-2",
                    usage: Some(&b),
                    active: false,
                    usable: false,
                },
            ],
            &Rules::default(),
            now(),
        );
        assert_eq!(decision, Decision::Blocked(Blocked::NoAlternative));
    }

    #[test]
    fn nothing_signed_in_is_not_an_error() {
        let a = usage(10.0);
        assert_eq!(
            decide(
                &[candidate("claude-1", Some(&a), false)],
                &Rules::default(),
                now()
            ),
            Decision::Stay(Stay::NoActiveAccount)
        );
    }

    #[test]
    fn ties_are_broken_deterministically() {
        let (a, b, c) = (usage(95.0), usage(20.0), usage(20.0));
        let first = decide(
            &[
                candidate("claude-1", Some(&a), true),
                candidate("claude-2", Some(&b), false),
                candidate("claude-3", Some(&c), false),
            ],
            &Rules::default(),
            now(),
        );
        let reordered = decide(
            &[
                candidate("claude-3", Some(&c), false),
                candidate("claude-1", Some(&a), true),
                candidate("claude-2", Some(&b), false),
            ],
            &Rules::default(),
            now(),
        );
        assert_eq!(first, reordered);
    }
}
