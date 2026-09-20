//! Deciding when to switch accounts, and to which one.
//!
//! This module is pure: it takes a snapshot of what is known and returns a
//! decision. That keeps the rules — which are the whole point of the tool —
//! testable without touching the network or the filesystem.

use jiff::Timestamp;

use crate::usage::{Usage, WindowKind};

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
    /// How large this account's quota is relative to the base tier, when the
    /// provider says. See [`weights`] for what it changes.
    pub capacity: Option<u32>,
}

/// What moving to another account costs the person using it.
///
/// This is the one fact that changes the arithmetic, and it comes from whether
/// the agent CLI re-reads its credential or has to be restarted — so a new
/// provider inherits the behaviour by answering that question rather than by
/// being named here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disruption {
    /// A running session picks the new account up on its own (Claude Code).
    Seamless,
    /// Running sessions must be restarted to see the change (Codex).
    RestartsSessions,
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
    /// What a switch costs on this provider.
    pub disruption: Disruption,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            threshold: crate::config::DEFAULT_THRESHOLD,
            margin: crate::config::DEFAULT_MARGIN,
            // Long enough to cover a poll interval and a retry, short enough
            // that a switch is never made on yesterday's numbers.
            max_reading_age_secs: 30 * 60,
            // The cautious default: a provider is assumed to cost a restart
            // until it says otherwise.
            disruption: Disruption::RestartsSessions,
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
    /// Every account is past the threshold, and on this provider moving would
    /// interrupt running sessions — too high a price for picking the least bad.
    NotWorthARestart { used: f64 },
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
    /// Nothing is wrong with the account in use, but another account's weekly
    /// allowance expires sooner and would otherwise be thrown away unspent.
    /// Only taken where a switch interrupts nothing.
    WeekExpiresSooner { target_used: f64 },
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

/// A recovery this much sooner is worth a switch; anything closer is noise.
const RECOVERY_MARGIN_SECS: i64 = 5 * 60;

/// Decides what to do for one provider.
pub fn decide(candidates: &[Candidate<'_>], rules: &Rules, now: Timestamp) -> Decision {
    let Some(active) = candidates.iter().find(|c| c.active) else {
        return Decision::Stay(Stay::NoActiveAccount);
    };
    // Deliberately not gated on age. The account being used up is the one that
    // gets hammered, so it is the first the provider rate-limits — and if a
    // failed poll made it unreadable rather than spent, switching would stop
    // working at exactly the moment it is needed. A provider declining to
    // answer is not a provider retracting what it said, and a reading that has
    // passed its reset already counts as empty rather than full.
    let Some(active_usage) = active.usage else {
        return Decision::Stay(Stay::UsageUnknown);
    };

    let used = active_usage.used_at(now);
    let exhausted = active_usage.is_exhausted_at(now);

    // Only accounts that can actually serve traffic are worth moving to.
    let mut usable: Vec<(&Candidate<'_>, &Usage)> = candidates
        .iter()
        .filter(|c| !c.active && c.usable)
        .filter_map(|c| Some((c, fresh(c, rules, now)?)))
        .filter(|(_, usage)| !usage.is_exhausted_at(now))
        .collect();

    // Where a switch interrupts nothing, there is no reason to wait for the
    // threshold before spending capacity that is about to expire: whatever is
    // left in a weekly window when it resets is thrown away.
    //
    // The trigger is a strictly sooner weekly reset and nothing else, which is
    // what keeps it from oscillating. A weekly reset is fixed for the life of
    // its window — it does not drift as the account is used — so whichever
    // account wins stays the winner until its week actually turns over.
    // Triggering on headroom, or on the rolling five-hour boundary, would flip
    // the answer on every poll: free in restarts, and still a credential write
    // and a moving marker every few minutes.
    if rules.disruption == Disruption::Seamless {
        let active_week = active_usage.resets_at(WindowKind::Weekly);
        let sooner = usable
            .iter()
            .copied()
            // The threshold is respected in both directions: an account over
            // the mark is being left, so it is not somewhere to move to.
            .filter(|(_, usage)| usage.used_at(now) < rules.threshold)
            .filter(
                |(_, usage)| match (usage.resets_at(WindowKind::Weekly), active_week) {
                    (Some(candidate), Some(active)) => candidate < active,
                    // An account with no weekly window has nothing perishable to
                    // spend, and one whose current week is unknown is not a reason
                    // to move off it.
                    _ => false,
                },
            );
        if let Some((candidate, usage)) = soonest_to_recover(sooner, now) {
            return Decision::Switch {
                to: candidate.id.to_string(),
                reason: Reason::WeekExpiresSooner {
                    target_used: usage.used_at(now),
                },
            };
        }
    }

    if used < rules.threshold && !exhausted {
        return Decision::Stay(Stay::BelowThreshold { used });
    }

    // How much work each account can still do, rather than what fraction of
    // itself it has left: a percentage is relative to that account's own quota,
    // and quotas differ.
    let weighted = quotas_all_known(active, &usable);
    let remaining =
        |candidate: &Candidate<'_>, usage: &Usage| usage.headroom_at(now) * weight(candidate, weighted);

    rank(&mut usable, rules, now, &remaining);

    match usable.first().copied() {
        Some((candidate, usage)) => {
            let target_used = usage.used_at(now);
            let reason = if exhausted {
                Reason::ActiveExhausted { target_used }
            } else if target_used < rules.threshold {
                Reason::ThresholdCrossed { used, target_used }
            } else if rules.disruption == Disruption::RestartsSessions {
                // Everything is past the threshold, so this would only be
                // picking the least bad — an optimisation. Paying for one with
                // somebody's running sessions is the wrong trade. The exception
                // above is the account being left having nothing to give: a
                // restart avoided by stranding someone on a wall is a restart
                // they will have to take anyway.
                return Decision::Stay(Stay::NotWorthARestart { used });
            } else if worth_taking(active_usage, usage, rules, now) {
                // Everything is past the threshold, so this is a choice between
                // accounts that are comparably placed — worth making only for
                // a meaningfully sooner recovery, or more left afterwards.
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

/// Puts the accounts worth switching to in the order they would be taken.
///
/// Accounts under the threshold come first, and among them the one whose weekly
/// allowance expires soonest — see [`recovery_order`]. Past the threshold there
/// is no room left to allocate and the only question is which is least bad, so
/// those rank by what they can still do.
fn rank(
    usable: &mut [(&Candidate<'_>, &Usage)],
    rules: &Rules,
    now: Timestamp,
    remaining: &impl Fn(&Candidate<'_>, &Usage) -> f64,
) {
    usable.sort_by(|a, b| {
        let over = |(_, usage): &(&Candidate<'_>, &Usage)| usage.used_at(now) >= rules.threshold;
        over(a).cmp(&over(b)).then_with(|| {
            if over(a) {
                free_order(a.1, b.1, rules, now).then_with(|| a.0.id.cmp(b.0.id))
            } else {
                recovery_order(*a, *b, remaining)
            }
        })
    });
}

/// Ranks two accounts that are both past the threshold, by when each can work
/// again.
///
/// The soonest window an account is pressed against is the one that frees it,
/// not the worst of them: at 99% of five hours and 95% of a week it is held by
/// both, and the short window hands it a fresh allowance within the hour.
/// Ranking on the worst figure would call that five days, because 95 is the
/// larger number.
///
/// Where two recover at about the same time, the one with more left in its
/// slowest window has more to give afterwards.
fn free_order(a: &Usage, b: &Usage, rules: &Rules, now: Timestamp) -> std::cmp::Ordering {
    let frees = |usage: &Usage| usage.recovers_at(now, rules.threshold).unwrap_or(Timestamp::MAX);
    let slowest = |usage: &Usage| usage.slowest_headroom_at(now).unwrap_or(0.0);
    frees(a)
        .cmp(&frees(b))
        .then_with(|| slowest(b).total_cmp(&slowest(a)))
}

/// Whether moving to `best` is worth it when both are past the threshold.
///
/// The margins exist to stop pointless shuffling between accounts that are
/// comparably placed, which is why they are measured against what a switch
/// buys: recovering meaningfully sooner is worth it, recovering later is not,
/// and between the two it comes down to what is left afterwards.
fn worth_taking(current: &Usage, best: &Usage, rules: &Rules, now: Timestamp) -> bool {
    let frees = |usage: &Usage| usage.recovers_at(now, rules.threshold);
    let sooner = match (frees(current), frees(best)) {
        (Some(current), Some(best)) => current.as_second() - best.as_second(),
        // One of them is not pressed against anything, so there is nothing to
        // compare in time; fall through to what each has left.
        _ => 0,
    };
    if sooner >= RECOVERY_MARGIN_SECS {
        return true;
    }
    if sooner <= -RECOVERY_MARGIN_SECS {
        return false;
    }
    match (best.slowest_headroom_at(now), current.slowest_headroom_at(now)) {
        (Some(best), Some(current)) => best - current >= rules.margin,
        _ => false,
    }
}

/// The order these accounts would be taken in, best next first.
///
/// The same ranking [`decide`] chooses from, exposed so an interface can show
/// the queue itself rather than an arrangement of its own invention — and so
/// the order somebody reads is the order that will actually happen.
///
/// The account in use leads, because it is the one in use. After it come the
/// accounts that could be switched to, then those that could not: spent, or
/// with no reading, or needing a login.
pub fn queue<'a>(candidates: &'a [Candidate<'a>], rules: &Rules, now: Timestamp) -> Vec<&'a str> {
    let active = candidates.iter().find(|c| c.active);
    let mut usable: Vec<(&Candidate<'_>, &Usage)> = candidates
        .iter()
        .filter(|c| !c.active && c.usable)
        .filter_map(|c| Some((c, fresh(c, rules, now)?)))
        .filter(|(_, usage)| !usage.is_exhausted_at(now))
        .collect();

    let weighted = active.is_some_and(|active| quotas_all_known(active, &usable));
    let remaining =
        |candidate: &Candidate<'_>, usage: &Usage| usage.headroom_at(now) * weight(candidate, weighted);
    rank(&mut usable, rules, now, &remaining);

    let ranked: Vec<&str> = active
        .map(|c| c.id)
        .into_iter()
        .chain(usable.iter().map(|(c, _)| c.id))
        .collect();
    // Whatever the ranking had no place for still belongs on screen, after it.
    let rest = candidates.iter().map(|c| c.id).filter(|id| !ranked.contains(id));
    ranked.iter().copied().chain(rest).collect()
}

/// Ranks two accounts that both have room, by when they recover.
///
/// Not by headroom, because a five-hour percentage and a weekly percentage are
/// not comparable: five hours is a **rate**, and an account held back only by
/// it is available again in hours, while a week is a **budget** whose remainder
/// is thrown away at reset. Ranking on the worse of the two sends work to an
/// account whose week is genuinely scarcer, for the sake of a limit that clears
/// itself by lunchtime.
///
/// So the soonest-expiring weekly allowance is spent first — earliest deadline
/// first on a perishable resource, which is better or neutral and never worse.
/// The five-hour reset breaks ties between accounts whose weeks expire
/// together, since that is the one that can start working again sooner, and the
/// work each can still do breaks ties after that.
fn recovery_order(
    a: (&Candidate<'_>, &Usage),
    b: (&Candidate<'_>, &Usage),
    remaining: &impl Fn(&Candidate<'_>, &Usage) -> f64,
) -> std::cmp::Ordering {
    // An account that states no window of a kind has nothing expiring, so it
    // sorts last rather than first.
    let reset = |usage: &Usage, kind| usage.resets_at(kind).unwrap_or(Timestamp::MAX);
    reset(a.1, WindowKind::Weekly)
        .cmp(&reset(b.1, WindowKind::Weekly))
        .then_with(|| reset(a.1, WindowKind::FiveHour).cmp(&reset(b.1, WindowKind::FiveHour)))
        .then_with(|| remaining(b.0, b.1).total_cmp(&remaining(a.0, a.1)))
        .then_with(|| a.0.id.cmp(b.0.id))
}

/// The account of those given whose allowance expires soonest.
fn soonest_to_recover<'a>(
    candidates: impl Iterator<Item = (&'a Candidate<'a>, &'a Usage)>,
    now: Timestamp,
) -> Option<(&'a Candidate<'a>, &'a Usage)> {
    // Headroom cannot break ties here: this runs before the ranking that knows
    // whether every quota size is stated, so it compares plain percentages.
    let plain = |_: &Candidate<'_>, usage: &Usage| usage.headroom_at(now);
    candidates.min_by(|a, b| recovery_order(*a, *b, &plain))
}

/// Whether the size of every quota in the comparison is known.
///
/// Weighting is all-or-nothing on purpose: treating an unknown quota as the
/// base tier would rank a perfectly good account last for a fact the provider
/// simply did not state.
fn quotas_all_known(active: &Candidate<'_>, usable: &[(&Candidate<'_>, &Usage)]) -> bool {
    active.capacity.is_some() && usable.iter().all(|(c, _)| c.capacity.is_some())
}

/// What one point of an account's headroom is worth against the others.
///
/// A percentage says how full an account is, never how big it is, and two
/// accounts on the same plan can differ several-fold: half of a 5x seat is an
/// eighth of half of a 20x one. Where every quota in play is known, headroom is
/// weighted by quota size, so ranking compares the work each account can still
/// do rather than the fraction of itself it has left.
fn weight(candidate: &Candidate<'_>, weighted: bool) -> f64 {
    match candidate.capacity {
        Some(times) if weighted => f64::from(times),
        _ => 1.0,
    }
}

/// The candidate's reading, if it is recent enough to act on.
fn fresh<'a>(candidate: &Candidate<'a>, rules: &Rules, now: Timestamp) -> Option<&'a Usage> {
    candidate
        .usage
        .filter(|usage| usage.age_secs(now) <= rules.max_reading_age_secs)
        // Once every window it described has turned over it states no usage,
        // which is not the same as stating there is room to switch into.
        .filter(|usage| !usage.states_nothing_at(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{FIVE_HOURS, ONE_WEEK, Window};

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
            capacity: None,
        }
    }

    /// The same account, on a quota `times` the base tier.
    fn sized<'a>(id: &'a str, usage: Option<&'a Usage>, active: bool, times: u32) -> Candidate<'a> {
        Candidate {
            capacity: Some(times),
            ..candidate(id, usage, active)
        }
    }

    /// An account with both windows the providers report: a five-hour rate and
    /// a weekly budget, each with its own reset.
    fn metered(five_hour: f64, five_hour_in: i64, weekly: f64, weekly_in_days: i64) -> Usage {
        Usage {
            observed_at: now(),
            windows: vec![
                Window {
                    window_secs: FIVE_HOURS,
                    scope: None,
                    used_percent: five_hour,
                    resets_at: Timestamp::from_second(NOW + five_hour_in).ok(),
                },
                Window {
                    window_secs: ONE_WEEK,
                    scope: None,
                    used_percent: weekly,
                    resets_at: Timestamp::from_second(NOW + weekly_in_days * 86_400).ok(),
                },
            ],
            limit_reached: false,
        }
    }

    /// An account pressed against its week, which turns over in `weekly_in`
    /// seconds, with its five-hour window comfortable.
    fn pressed(weekly_used: f64, weekly_in: i64) -> Usage {
        Usage {
            observed_at: now(),
            windows: vec![
                Window {
                    window_secs: FIVE_HOURS,
                    scope: None,
                    used_percent: 20.0,
                    resets_at: Timestamp::from_second(NOW + 3600).ok(),
                },
                Window {
                    window_secs: ONE_WEEK,
                    scope: None,
                    used_percent: weekly_used,
                    resets_at: Timestamp::from_second(NOW + weekly_in).ok(),
                },
            ],
            limit_reached: false,
        }
    }

    fn seamless() -> Rules {
        Rules {
            disruption: Disruption::Seamless,
            ..Rules::default()
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

    /// The margin only ever arises where a switch is free; where it costs a
    /// restart the provider stays put instead. So this is a seamless provider.
    #[test]
    fn when_everything_is_busy_it_moves_only_for_a_worthwhile_gain() {
        let rules = seamless();

        // 93 -> 91 is only 2 points: not worth the churn.
        let (a, b) = (usage(93.0), usage(91.0));
        let decision = decide(
            &[
                candidate("claude-1", Some(&a), true),
                candidate("claude-2", Some(&b), false),
            ],
            &rules,
            now(),
        );
        assert_eq!(decision, Decision::Stay(Stay::NoBetterAccount { used: 93.0 }));

        // 99 -> 91 clears the margin.
        let (a, b) = (usage(99.0), usage(91.0));
        let decision = decide(
            &[
                candidate("claude-1", Some(&a), true),
                candidate("claude-2", Some(&b), false),
            ],
            &rules,
            now(),
        );
        assert_eq!(
            decision,
            Decision::Switch {
                to: "claude-2".into(),
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

    /// The account that gets hammered is the spent one, so it is the first to
    /// be rate-limited — and if a failed poll made it *unreadable* rather than
    /// *spent*, switching would stop working exactly when it is needed. What
    /// the endpoint said before still stands: a provider declining to answer is
    /// not a provider retracting what it said.
    #[test]
    fn a_stale_reading_still_counts_against_the_account_in_use() {
        let rules = Rules::default();
        let long_ago = rules.max_reading_age_secs * 10;
        let spent = usage_aged(100.0, long_ago);
        let fresh_other = usage(5.0);

        let decision = decide(
            &[
                candidate("claude-1", Some(&spent), true),
                candidate("claude-2", Some(&fresh_other), false),
            ],
            &rules,
            now(),
        );
        assert!(
            matches!(&decision, Decision::Switch { to, .. } if to == "claude-2"),
            "a spent account that stopped answering must still be left: {decision:?}"
        );
    }

    /// The same reading does not get to claim headroom once it is old: an
    /// account is only switched *to* on figures that are still current.
    #[test]
    fn stale_readings_are_not_acted_on() {
        let rules = Rules::default();
        let stale_active = usage_aged(95.0, rules.max_reading_age_secs + 1);
        let fresh_other = usage(5.0);
        assert!(
            matches!(
                decide(
                    &[
                        candidate("claude-1", Some(&stale_active), true),
                        candidate("claude-2", Some(&fresh_other), false)
                    ],
                    &rules,
                    now()
                ),
                Decision::Switch { .. }
            ),
            "the account in use is judged on what is known about it"
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
                    usable: false,
                    ..candidate("claude-2", Some(&b), false)
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

    /// Five hours is a rate and a week is a budget, so ranking on whichever
    /// window happens to be worse sends work to the account whose week is
    /// genuinely scarcer, for the sake of a limit that clears by lunchtime.
    #[test]
    fn the_scarcer_week_outranks_a_worse_five_hour_window() {
        // `b` looks worse — 80% of its five-hour rate against 10% — but that
        // window rolls in an hour, and its week lasts three days longer.
        let current = metered(95.0, 3600, 50.0, 5);
        let a = metered(10.0, 3600, 40.0, 5);
        let b = metered(80.0, 3600, 40.0, 2);

        let decision = decide(
            &[
                candidate("claude-1", Some(&current), true),
                candidate("claude-2", Some(&a), false),
                candidate("claude-3", Some(&b), false),
            ],
            &Rules::default(),
            now(),
        );
        assert!(
            matches!(&decision, Decision::Switch { to, .. } if to == "claude-3"),
            "the soonest-expiring week should be spent first: {decision:?}"
        );
    }

    /// Weekly capacity is perishable, and a switch that interrupts nothing has
    /// no reason to wait for the threshold before spending it.
    #[test]
    fn a_free_switch_spends_the_soonest_expiring_week_without_waiting() {
        let current = metered(20.0, 3600, 20.0, 6);
        let expiring = metered(30.0, 3600, 30.0, 1);

        let decision = decide(
            &[
                candidate("claude-1", Some(&current), true),
                candidate("claude-2", Some(&expiring), false),
            ],
            &seamless(),
            now(),
        );
        assert!(
            matches!(
                &decision,
                Decision::Switch { to, reason: Reason::WeekExpiresSooner { .. } } if to == "claude-2"
            ),
            "{decision:?}"
        );

        // Where the same move would restart somebody's sessions, it waits.
        assert_eq!(
            decide(
                &[
                    candidate("codex-1", Some(&current), true),
                    candidate("codex-2", Some(&expiring), false)
                ],
                &Rules::default(),
                now()
            ),
            Decision::Stay(Stay::BelowThreshold { used: 20.0 })
        );
    }

    /// The trigger is a strictly sooner weekly reset and nothing else, because
    /// a weekly reset does not move as the account is used. Anything that
    /// drifts with usage would flip the answer on every poll.
    #[test]
    fn a_free_switch_does_not_oscillate() {
        let mine = metered(20.0, 3600, 20.0, 3);
        let same_week_roomier = metered(1.0, 60, 1.0, 3);
        let same_week_emptier_later = metered(0.0, 60, 0.0, 4);

        for other in [&same_week_roomier, &same_week_emptier_later] {
            let decision = decide(
                &[
                    candidate("claude-1", Some(&mine), true),
                    candidate("claude-2", Some(other), false),
                ],
                &seamless(),
                now(),
            );
            assert_eq!(
                decision,
                Decision::Stay(Stay::BelowThreshold { used: 20.0 }),
                "only a sooner week may move a free switch: {decision:?}"
            );
        }
    }

    /// Past the threshold there is no room to allocate and a switch is only
    /// picking the least bad — not worth interrupting somebody's work for.
    #[test]
    fn a_costly_switch_does_not_shuffle_between_accounts_that_are_all_busy() {
        let (current, other) = (usage(99.0), usage(92.0));
        assert_eq!(
            decide(
                &[
                    candidate("codex-1", Some(&current), true),
                    candidate("codex-2", Some(&other), false)
                ],
                &Rules::default(),
                now()
            ),
            Decision::Stay(Stay::NotWorthARestart { used: 99.0 }),
        );

        // Unless staying means staying on a wall: the restart being spared is
        // one the person has to take anyway.
        let spent = usage(100.0);
        let decision = decide(
            &[
                candidate("codex-1", Some(&spent), true),
                candidate("codex-2", Some(&other), false),
            ],
            &Rules::default(),
            now(),
        );
        assert!(
            matches!(&decision, Decision::Switch { to, .. } if to == "codex-2"),
            "{decision:?}"
        );
    }

    /// A percentage is a fraction of an account's own quota, so the roomiest
    /// account by percentage is not the one that can do the most work. Half of
    /// a 5x seat is an eighth of half of a 20x one.
    #[test]
    fn the_account_with_the_most_work_left_wins_not_the_emptiest_one() {
        let (active, small, large) = (usage(95.0), usage(10.0), usage(60.0));
        let decision = decide(
            &[
                sized("claude-1", Some(&active), true, 5),
                // 90% left of a 5x quota: 4.5 units.
                sized("claude-2", Some(&small), false, 5),
                // 40% left of a 20x quota: 8 units, despite looking fuller.
                sized("claude-3", Some(&large), false, 20),
            ],
            &Rules::default(),
            now(),
        );
        assert_eq!(
            decision,
            Decision::Switch {
                to: "claude-3".into(),
                reason: Reason::ThresholdCrossed {
                    used: 95.0,
                    target_used: 60.0
                },
            }
        );
    }

    /// Weighting is all-or-nothing: an account whose quota the provider never
    /// stated must not be ranked last for it.
    #[test]
    fn one_unknown_quota_falls_back_to_comparing_percentages() {
        let (active, small, large) = (usage(95.0), usage(10.0), usage(60.0));
        let decision = decide(
            &[
                sized("claude-1", Some(&active), true, 5),
                sized("claude-2", Some(&small), false, 5),
                // Same readings as the test above, but this one's size is
                // unknown, so percentages decide and the emptiest wins.
                candidate("claude-3", Some(&large), false),
            ],
            &Rules::default(),
            now(),
        );
        assert!(
            matches!(&decision, Decision::Switch { to, .. } if to == "claude-2"),
            "{decision:?}"
        );
    }

    /// With every account busy, what a switch buys is a sooner return to
    /// work — so that is what the margin is measured in.
    #[test]
    fn past_the_threshold_the_account_that_frees_up_first_wins() {
        let rules = seamless();
        // Both are pressed against their week. One turns over in an hour, the
        // other in four days.
        let soon = pressed(95.0, 3600);
        let late = pressed(95.0, 4 * 86_400);

        // The account in use is the one waiting four days: move.
        let candidates = [
            candidate("claude-1", Some(&late), true),
            candidate("claude-2", Some(&soon), false),
        ];
        assert!(
            matches!(
                decide(&candidates, &rules, now()),
                Decision::Switch { ref to, .. } if to == "claude-2"
            ),
            "an hour of waiting beats four days"
        );

        // The other way round there is nothing to gain by moving.
        let candidates = [
            candidate("claude-1", Some(&soon), true),
            candidate("claude-2", Some(&late), false),
        ];
        assert_eq!(
            decide(&candidates, &rules, now()),
            Decision::Stay(Stay::NoBetterAccount { used: 95.0 })
        );
    }

    /// The soonest window an account is held by is the one that frees it, not
    /// the worst figure it carries.
    #[test]
    fn recovery_is_the_soonest_window_pressed_against_not_the_worst_one() {
        let now = now();
        // 99% of five hours and 95% of a week: held by both, freed by the
        // short one within the hour.
        let both = metered(99.0, 3600, 95.0, 5);
        assert_eq!(
            both.recovers_at(now, 90.0)
                .map(|at| at.as_second() - now.as_second()),
            Some(3600),
            "the worst figure is the week, but the hour is when work resumes"
        );

        // Nothing pressing against it: nothing to wait for.
        let free = metered(10.0, 3600, 10.0, 5);
        assert_eq!(free.recovers_at(now, 90.0), None);
    }

    /// Accounts that come back at the same moment are separated by what they
    /// have left once they do.
    #[test]
    fn accounts_that_recover_together_are_separated_by_what_is_left_after() {
        let rules = seamless();
        // Both freed by the same five-hour window in an hour; one has far more
        // of its week left for afterwards.
        let thin = metered(95.0, 3600, 88.0, 5);
        let deep = metered(95.0, 3600, 20.0, 5);

        let candidates = [
            candidate("claude-1", Some(&thin), true),
            candidate("claude-2", Some(&deep), false),
        ];
        assert!(
            matches!(
                decide(&candidates, &rules, now()),
                Decision::Switch { ref to, .. } if to == "claude-2"
            ),
            "same wait, more to give afterwards"
        );

        // Reversed, the gain is not there and the churn is not worth it.
        let candidates = [
            candidate("claude-1", Some(&deep), true),
            candidate("claude-2", Some(&thin), false),
        ];
        assert!(
            matches!(decide(&candidates, &rules, now()), Decision::Stay(_)),
            "nothing to gain"
        );
    }

    /// A per-model limit is not the account's: one capped model must not make
    /// an otherwise free account look spent, or unusable to switch to.
    #[test]
    fn a_capped_model_does_not_make_the_account_spent() {
        let now = now();
        let mut reading = metered(10.0, 3600, 20.0, 5);
        reading.windows.push(Window {
            window_secs: ONE_WEEK,
            scope: Some("Fable".into()),
            used_percent: 100.0,
            resets_at: Timestamp::from_second(NOW + 5 * 86_400).ok(),
        });

        assert_eq!(reading.worst_at(now), 100.0, "worth showing");
        assert_eq!(reading.used_at(now), 20.0, "not worth refusing the account for");
        assert!(!reading.is_exhausted_at(now));
    }

    /// A reading whose every window has turned over states no usage, which is
    /// not the same as stating there is room.
    #[test]
    fn a_reading_that_has_wholly_expired_is_not_evidence_of_room() {
        let spent = usage(100.0);
        let now = now();
        assert!(!spent.states_nothing_at(now));

        // An hour later every window it described has reset.
        let later = Timestamp::from_second(NOW + 7200).unwrap();
        assert!(spent.states_nothing_at(later));

        let fresh = usage(10.0);
        let candidates = [
            candidate("claude-1", Some(&fresh), true),
            candidate("claude-2", Some(&spent), false),
        ];
        // It must not be offered as the roomiest thing available.
        let order = queue(&candidates, &Rules::default(), later);
        assert_eq!(order, ["claude-1", "claude-2"]);
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
