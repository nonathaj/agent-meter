//! What spending a limit reset is said to do and to have done, in the same
//! words on the command line and in the interface.

use jiff::Timestamp;

use crate::account::ProviderKind;
use crate::engine::SpendReport;
use crate::provider::Spent;
use crate::usage::{ResetGrant, Usage, WindowKind};

/// What spending a reset of `grant` would do, a line at a time, as of the
/// reading `usage`: the provider's name for it, the limits it refills and how
/// full they are now, when it lapses, and anything the provider said that
/// stands in the way.
pub fn offer(
    provider: ProviderKind,
    grant: &ResetGrant,
    usage: Option<&Usage>,
    now: Timestamp,
) -> Vec<String> {
    let mut lines = vec![grant.title.clone().unwrap_or_else(|| "A limit reset".into())];
    let refills: Vec<String> = grant
        .refills
        .iter()
        .filter_map(|kind| {
            let name = match kind {
                WindowKind::FiveHour => "5h",
                WindowKind::Weekly => "weekly",
                WindowKind::Other => return None,
            };
            let used = usage.and_then(|usage| {
                usage
                    .windows
                    .iter()
                    .filter(|w| w.is_account_wide() && w.kind() == *kind)
                    .map(|w| w.used_at(now))
                    .reduce(f64::max)
            });
            Some(match used {
                Some(used) => format!("{name} (now {used:.0}%)"),
                None => name.to_string(),
            })
        })
        .collect();
    if !refills.is_empty() {
        lines.push(format!("refills {}", refills.join(" and ")));
    }
    if let Some(at) = grant.expires_at {
        lines.push(format!(
            "expires in {} ({})",
            crate::timefmt::until(now, at),
            local_moment(at)
        ));
    }
    if let Some(hindrance) = grant.hindrance() {
        lines.push(format!("{} says it is {hindrance}", vendor(provider)));
    }
    lines
}

/// What the provider said, as one sentence.
pub fn outcome(report: &SpendReport) -> String {
    let id = &report.id;
    match &report.answer {
        Spent::Reset { .. } => format!("Spent a limit reset on {id}"),
        Spent::NotNeeded => format!("{id} is at no limit a reset would clear, so none was spent"),
        Spent::AlreadyUsed => format!(
            "That reset on {id} was already used, so none was spent now; if an earlier try got \
             no answer, it went through then"
        ),
        Spent::Refused(why) => format!("No reset was spent on {id}: {why}"),
        Spent::Unconfirmed(why) => format!(
            "{why}, so whether one was spent on {id} is not known. Asking again within ten \
             minutes asks about the same request, and cannot spend a second"
        ),
    }
}

/// How the account stands now that the provider has answered.
pub fn after(report: &SpendReport, now: Timestamp) -> String {
    match &report.after {
        Ok(usage) => {
            let mut parts: Vec<String> = usage
                .windows
                .iter()
                .filter(|w| w.is_account_wide())
                .map(|w| format!("{} {:.0}%", w.label(), w.used_at(now)))
                .collect();
            if let Some(resets) = &usage.resets {
                parts.push(format!("{} limit reset(s) left", resets.left_at(now)));
            }
            format!("{} now: {}", report.id, parts.join(" · "))
        }
        Err(error) => format!("Could not read {} again: {error}", report.id),
    }
}

/// A moment in local time, such as `Nov 15 09:00`.
pub fn local_moment(at: Timestamp) -> String {
    at.to_zoned(jiff::tz::TimeZone::system())
        .strftime("%b %-d %H:%M")
        .to_string()
}

/// Who decides whether a reset is taken.
fn vendor(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "Anthropic",
        ProviderKind::Codex => "OpenAI",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{FIVE_HOURS, ONE_WEEK, Window};

    fn ts(s: i64) -> Timestamp {
        Timestamp::from_second(s).unwrap()
    }

    /// The offer names what is about to be refilled with how full it is, so
    /// nobody spends a reset on a limit that is nearly empty without seeing so.
    #[test]
    fn an_offer_says_what_it_refills_and_how_full_that_is_now() {
        let now = ts(1_800_000_000);
        let window = |secs, used, scope: Option<&str>| Window {
            window_secs: secs,
            scope: scope.map(Into::into),
            used_percent: used,
            resets_at: Some(ts(1_800_100_000)),
        };
        let usage = Usage {
            observed_at: now,
            windows: vec![
                window(FIVE_HOURS, 97.0, None),
                window(ONE_WEEK, 40.0, None),
                window(ONE_WEEK, 100.0, Some("Fable")),
            ],
            limit_reached: false,
            resets: None,
            ignored: Vec::new(),
        };
        let grant = ResetGrant {
            title: Some("Launch reset".into()),
            left: 1,
            expires_at: Some(ts(1_800_000_000 + 3 * 86_400)),
            refills: vec![WindowKind::FiveHour, WindowKind::Weekly],
            usable_now: Some(false),
            only_at_limit: true,
            ..ResetGrant::default()
        };
        let lines = offer(ProviderKind::Claude, &grant, Some(&usage), now);
        assert_eq!(lines[0], "Launch reset");
        assert_eq!(lines[1], "refills 5h (now 97%) and weekly (now 40%)");
        assert!(lines[2].starts_with("expires in 3d ("), "{}", lines[2]);
        assert_eq!(
            lines[3],
            "Anthropic says it is usable only once a limit is reached"
        );

        // Nothing said, nothing claimed.
        let bare = offer(ProviderKind::Codex, &ResetGrant::default(), None, now);
        assert_eq!(bare, ["A limit reset"]);
    }
}
