//! Rendering. Nothing here mutates anything but the frame.
//!
//! An account is a block rather than a row, because a row can only show the
//! worst of its limits and the worst limit is not the whole story: an account
//! at 5% of its five hours and 98% of its week is nearly spent, and one the
//! other way round is fine in an hour. Every window is on screen for every
//! account, so two accounts can be compared without selecting either.

use jiff::Timestamp;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::app::{App, Mode, ProviderAction, Row};
use crate::account::ProviderKind;
use crate::engine::Status;
use crate::timefmt;
use crate::usage::Window;

/// Usage at which a window stops looking comfortable.
const WARN_PERCENT: f64 = 75.0;
/// Usage at which it is nearly spent.
const HIGH_PERCENT: f64 = 90.0;
/// How much of a window may be spent ahead of the clock before saying so.
/// A few points ahead is ordinary; a quarter of the window is a trend.
const PACE_SLACK: f64 = 25.0;

/// Width of the meter drawn for each window.
const BAR: usize = 28;

pub fn draw(frame: &mut Frame, app: &mut App) {
    // The keys keep a line of their own. Sharing it with whatever just
    // happened meant that every time something happened, the way to do the
    // next thing disappeared.
    let [header, body, message, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    draw_body(frame, body, app);
    draw_message(frame, message, app);
    draw_keys(frame, keys, app);

    match app.mode.clone() {
        Mode::Help => draw_help(frame),
        Mode::ChooseProvider(action) => draw_provider_chooser(frame, action),
        Mode::ConfirmRemove { id, name } => draw_confirm_remove(frame, &id, &name),
        Mode::Browse => {}
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(
            " agent-meter ",
            Style::new()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(app.filter_label(), Style::new().fg(Color::Cyan)),
        Span::raw("   "),
    ];
    // Per harness, because they are switched on separately and one word for
    // both would be a lie about whichever is off.
    for (kind, on) in &app.switching {
        spans.push(Span::styled(
            format!("{} ", kind.display_name()),
            Style::new().fg(Color::DarkGray),
        ));
        spans.push(if *on {
            Span::styled(
                format!(" auto {:.0}% ", app.threshold),
                Style::new().bg(Color::Green).fg(Color::Black),
            )
        } else {
            Span::styled("manual", Style::new().fg(Color::DarkGray))
        });
        spans.push(Span::raw("   "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.rows.is_empty() {
        let text = Paragraph::new(vec![
            Line::raw(""),
            Line::raw("No accounts yet."),
            Line::raw(""),
            Line::raw("Press i to import the account an agent CLI is already signed in to,"),
            Line::raw("or a to log in to another one."),
        ])
        .alignment(Alignment::Center);
        frame.render_widget(text, area);
        return;
    }

    let now = Timestamp::now();
    let mut lines = Vec::new();
    for (index, row) in app.rows.iter().enumerate() {
        match row {
            Row::Provider(kind) => lines.push(provider_heading(*kind, app, lines.is_empty())),
            Row::Account { status, position } => {
                let selected = app.selected_row() == Some(index);
                lines.extend(account_block(
                    status,
                    *position,
                    selected,
                    app,
                    now,
                    area.width as usize,
                ));
            }
        }
    }

    // Keep the selected block in view without a scrollbar: the list is short
    // enough that a moving window is less to read than a bar beside it.
    app.visible_height = area.height as usize;
    let first = app.scroll.min(lines.len().saturating_sub(1));
    let shown: Vec<Line> = lines.into_iter().skip(first).take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(shown), area);
}

fn provider_heading(kind: ProviderKind, app: &App, first: bool) -> Line<'static> {
    let count = app
        .rows
        .iter()
        .filter(|row| matches!(row, Row::Account { status, .. } if status.account.provider == kind))
        .count();
    let mut spans = Vec::new();
    if !first {
        spans.push(Span::raw(""));
    }
    spans.push(Span::styled(
        format!("{} ", kind.display_name()),
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!("({count})  "),
        Style::new().fg(Color::DarkGray),
    ));

    // Naming it here answers "which account is this agent on" without reading
    // down the list hunting for a marker.
    match app.active_of(kind) {
        Some(status) => {
            spans.push(Span::styled("using ", Style::new().fg(Color::DarkGray)));
            spans.push(Span::styled(
                status.account.display_name().to_string(),
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            ));
        }
        None => spans.push(Span::styled(
            "not signed in to a stored account",
            Style::new().fg(Color::Yellow),
        )),
    }
    if app.switching_on(kind) {
        spans.push(Span::styled(
            "   in the order they will be taken",
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ));
    }
    Line::from(spans)
}

/// One account: who it is, then every limit it has.
fn account_block(
    status: &Status,
    position: usize,
    selected: bool,
    app: &App,
    now: Timestamp,
    width: usize,
) -> Vec<Line<'static>> {
    let account = &status.account;
    let identity = &account.identity;
    let marker = if selected { "▌" } else { " " };
    let number = Style::new().fg(if selected { Color::White } else { Color::DarkGray });

    let mut head = vec![
        Span::styled(marker.to_string(), Style::new().fg(Color::Cyan)),
        Span::styled(format!("{position} "), number),
        Span::styled(
            account.display_name().to_string(),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(org) = &identity.workspace_name {
        head.push(Span::styled(
            format!("  [{org}]"),
            Style::new().fg(Color::DarkGray),
        ));
    }
    if let Some(plan) = identity.plan_label() {
        head.push(Span::styled(format!("  {plan}"), Style::new().fg(Color::Blue)));
    }
    head.push(Span::raw("  "));
    head.push(standing(status, position, app, now));

    let mut lines = vec![Line::from(head)];
    match (&account.needs_login, &status.error, &status.usage) {
        (Some(reason), _, _) => lines.push(note(format!("sign in again — {reason}"), Color::Red, width)),
        (None, Some(error), None) => lines.push(note(format!("no reading — {error}"), Color::Yellow, width)),
        (None, error, Some(usage)) => {
            for window in &usage.windows {
                lines.push(window_line(window, now));
            }
            if let Some(error) = error {
                lines.push(note(format!("not refreshed — {error}"), Color::Yellow, width));
            }
        }
        (None, None, None) => lines.push(note("no reading yet — press r".into(), Color::DarkGray, width)),
    }
    lines.push(Line::raw(""));
    lines
}

/// What this account is, in one word: in use, next, or spent.
fn standing(status: &Status, position: usize, app: &App, now: Timestamp) -> Span<'static> {
    if status.active {
        // A filled badge rather than a word among words: this is the one fact
        // somebody opens the interface to find.
        return Span::styled(
            " IN USE ",
            Style::new()
                .bg(Color::Green)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        );
    }
    if status.account.needs_login.is_some() {
        return Span::styled("login needed", Style::new().fg(Color::Red));
    }
    if status
        .usage
        .as_ref()
        .is_some_and(|usage| usage.is_exhausted_at(now))
    {
        return Span::styled("spent", Style::new().fg(Color::Red));
    }
    // Only meaningful when something is actually choosing: with switching off
    // the order is just an order.
    if app.switching_on(status.account.provider) && position == 2 {
        return Span::styled("next", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    }
    Span::raw("")
}

/// One limit: its name, a meter, the figure, and when it turns over.
fn window_line(window: &Window, now: Timestamp) -> Line<'static> {
    let used = window.used_at(now);
    let colour = severity(used);
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(
            format!("{:<14}", truncate(&window.label(), 14)),
            Style::new().fg(Color::Gray),
        ),
        Span::styled(meter(used, BAR), Style::new().fg(colour)),
        Span::styled(
            format!("{used:>4.0}%  "),
            Style::new().fg(colour).add_modifier(Modifier::BOLD),
        ),
    ];

    match window.resets_at {
        Some(at) if at > now => spans.push(Span::styled(
            format!("resets {}", timefmt::until(now, at)),
            Style::new().fg(Color::DarkGray),
        )),
        Some(_) => spans.push(Span::styled(
            "resetting".to_string(),
            Style::new().fg(Color::DarkGray),
        )),
        None => {}
    }
    // Spending faster than the window refills is the thing a percentage alone
    // cannot say: 60% of a week is fine on day five and a warning on day two.
    if let Some(over) = ahead_of_pace(window, now) {
        spans.push(Span::styled(
            format!("  ({over:.0}% ahead of pace)"),
            Style::new().fg(Color::Yellow),
        ));
    }
    Line::from(spans)
}

/// How far ahead of the clock this window has been spent, when far enough to
/// be worth saying.
fn ahead_of_pace(window: &Window, now: Timestamp) -> Option<f64> {
    // Only a budget can be overspent. A five-hour window is a rate: being
    // ahead of its clock is ordinary, and it corrects itself within the hour,
    // so saying so on every line would be noise rather than a warning.
    if window.kind() != crate::usage::WindowKind::Weekly {
        return None;
    }
    let resets_at = window.resets_at?;
    let remaining = (resets_at.as_second() - now.as_second()).max(0) as f64;
    if window.window_secs == 0 || remaining > window.window_secs as f64 {
        return None;
    }
    let elapsed = 100.0 * (1.0 - remaining / window.window_secs as f64);
    let over = window.used_at(now) - elapsed;
    (over > PACE_SLACK).then_some(over)
}

fn note(text: String, colour: Color, width: usize) -> Line<'static> {
    // Provider messages run long, and a line that overruns the window is cut
    // mid-word with no sign that anything is missing.
    Line::from(vec![
        Span::raw("    "),
        Span::styled(truncate(&text, width.saturating_sub(6)), Style::new().fg(colour)),
    ])
}

/// A meter: `███████░░░░░░░`.
fn meter(used: f64, width: usize) -> String {
    let filled = ((used / 100.0).clamp(0.0, 1.0) * width as f64).round() as usize;
    format!("{}{} ", "█".repeat(filled), "░".repeat(width - filled))
}

fn severity(used: f64) -> Color {
    match used {
        u if u >= 100.0 => Color::Red,
        u if u >= HIGH_PERCENT => Color::LightRed,
        u if u >= WARN_PERCENT => Color::Yellow,
        _ => Color::Green,
    }
}

fn truncate(text: &str, width: usize) -> String {
    match text.char_indices().nth(width) {
        Some((idx, _)) => format!("{}…", &text[..idx.saturating_sub(1)]),
        None => text.to_string(),
    }
}

fn draw_message(frame: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(busy) = &app.busy {
        Line::from(Span::styled(busy.clone(), Style::new().fg(Color::Cyan)))
    } else if let Some(message) = &app.message {
        let colour = if message.is_error {
            Color::Red
        } else {
            Color::Green
        };
        Line::from(Span::styled(message.text.clone(), Style::new().fg(colour)))
    } else {
        Line::raw("")
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// What can be done from here: always on screen, and always for this screen.
fn draw_keys(frame: &mut Frame, area: Rect, app: &App) {
    let keys: Vec<(&str, String)> = match &app.mode {
        Mode::Help => vec![("any key", "close".into())],
        Mode::ChooseProvider(_) => vec![("1 2", "choose an agent".into()), ("esc", "cancel".into())],
        Mode::ConfirmRemove { .. } => vec![("y", "remove".into()), ("any key", "cancel".into())],
        Mode::Browse => {
            // Named for the harness the selection is in, since that is what the
            // key will act on.
            let switching = match app.selected().map(|status| status.account.provider) {
                Some(kind) if app.switching_on(kind) => format!("{} manual", kind.display_name()),
                Some(kind) => format!("{} auto", kind.display_name()),
                None => "auto-switch".into(),
            };
            vec![
                ("enter", "use".into()),
                ("w", switching),
                ("p", "by agent".into()),
                ("r", "refresh".into()),
                ("a", "add".into()),
                ("i", "import".into()),
                ("d", "remove".into()),
                ("?", "keys".into()),
                ("q", "quit".into()),
            ]
        }
    };

    // Built until it fills the width and no further: a bar that runs off the
    // edge hides the keys at its end, which are still keys somebody needs.
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (key, what) in keys {
        let width = key.chars().count() + what.chars().count() + 5;
        if used + width > area.width as usize {
            break;
        }
        used += width;
        spans.push(Span::styled(
            format!(" {key} "),
            Style::new().bg(Color::DarkGray).fg(Color::White),
        ));
        spans.push(Span::styled(format!(" {what}  "), Style::new().fg(Color::Gray)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(frame: &mut Frame) {
    let lines = vec![
        Line::from("↑ ↓ / j k    select an account".to_string()),
        Line::from("enter, u     sign the agent CLI in to it".to_string()),
        Line::from("r            read usage now".to_string()),
        Line::from("p            show one agent at a time, or all".to_string()),
        Line::from("a            log in to a new account".to_string()),
        Line::from("i            import the account a CLI is signed in to".to_string()),
        Line::from("d            forget the selected account".to_string()),
        Line::from("w            switch automatically as limits approach".to_string()),
        Line::from("q, esc       quit".to_string()),
        Line::from(""),
        Line::from("With switching on, accounts are listed in the order they".to_string()),
        Line::from("will be taken: the one in use first, then the one next.".to_string()),
        Line::from(""),
        Line::from(Span::styled(
            "Press any key to close",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    let area = popup(frame.area(), 62, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(bordered("Keys")), area);
}

fn draw_provider_chooser(frame: &mut Frame, action: ProviderAction) {
    let mut lines = vec![Line::from(action.title()), Line::from("")];
    for (index, provider) in ProviderKind::ALL.iter().enumerate() {
        lines.push(Line::from(format!(
            "  {}  {}",
            index + 1,
            provider.display_name()
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Any other key cancels",
        Style::new().fg(Color::DarkGray),
    )));

    let area = popup(frame.area(), 46, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(bordered("Which agent?")), area);
}

fn draw_confirm_remove(frame: &mut Frame, id: &str, name: &str) {
    let lines = vec![
        Line::from(format!("Remove {id} ({name})?")),
        Line::from(""),
        Line::from("The account itself is untouched: agent-meter just"),
        Line::from("forgets it, and you can add it back by signing in."),
        Line::from(""),
        Line::from(Span::styled(
            "y to remove, any other key to cancel",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    let area = popup(frame.area(), 56, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(bordered("Remove account")), area);
}

fn bordered(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(format!(" {title} "))
}

/// Centres a box of the given size, shrinking it to fit a small terminal.
fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{FIVE_HOURS, ONE_WEEK, Usage};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Renders the interface and returns what the terminal would show.
    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn window(secs: u64, scope: Option<&str>, used: f64, resets_in: i64) -> Window {
        Window {
            window_secs: secs,
            scope: scope.map(Into::into),
            used_percent: used,
            resets_at: Some(Timestamp::now() + jiff::SignedDuration::from_secs(resets_in)),
        }
    }

    fn usage(windows: Vec<Window>) -> Usage {
        Usage {
            observed_at: Timestamp::now(),
            windows,
            limit_reached: false,
        }
    }

    /// Every limit of every account is on screen at once — the point of the
    /// layout, since the worst window alone does not say whether an account is
    /// spent for an hour or for days.
    #[test]
    fn every_window_of_every_account_is_shown_without_selecting_it() {
        let mut statuses = crate::tui::app::sample_statuses(2);
        statuses[0].usage = Some(usage(vec![
            window(FIVE_HOURS, None, 68.0, 3 * 3600),
            window(ONE_WEEK, None, 76.0, 4 * 86_400),
            window(ONE_WEEK, Some("Fable"), 19.0, 4 * 86_400),
        ]));
        statuses[1].usage = Some(usage(vec![window(FIVE_HOURS, None, 4.0, 3600)]));

        let mut app = App::for_tests(statuses);
        let screen = render(&mut app, 110, 30);

        // Both accounts, and every window of the first, without selecting it.
        assert!(
            screen.contains("claude-1") && screen.contains("claude-2"),
            "{screen}"
        );
        assert!(screen.contains("5h"), "{screen}");
        assert!(screen.contains("weekly"), "{screen}");
        assert!(screen.contains("weekly Fable"), "{screen}");
        assert!(
            screen.contains("68%") && screen.contains("76%") && screen.contains("19%"),
            "{screen}"
        );
        // Resets are on the same line as the figure they belong to.
        assert!(screen.contains("resets 3h"), "{screen}");
        // The account in use says so.
        assert!(screen.contains("IN USE"), "the account in use is unmistakable:
{screen}");
        // And the harness groups them.
        assert!(screen.contains("Claude Code"), "{screen}");
    }

    /// With switching on the list is the queue, so the account that would be
    /// taken next says so.
    #[test]
    fn the_next_account_is_named_only_while_switching_is_on() {
        let mut statuses = crate::tui::app::sample_statuses(2);
        statuses[0].usage = Some(usage(vec![window(FIVE_HOURS, None, 95.0, 3600)]));
        statuses[1].usage = Some(usage(vec![window(FIVE_HOURS, None, 5.0, 3600)]));

        let mut app = App::for_tests(statuses);
        assert!(
            !render(&mut app, 110, 30).contains("next"),
            "with switching off the order is just an order"
        );

        app.switching = ProviderKind::ALL.map(|kind| (kind, true)).to_vec();
        let screen = render(&mut app, 110, 30);
        assert!(screen.contains("next"), "{screen}");
        assert!(screen.contains("in the order they will be taken"), "{screen}");
    }

    /// Spending a window faster than it refills is the thing a percentage
    /// cannot say on its own.
    #[test]
    fn a_window_spent_ahead_of_its_clock_says_so() {
        // Nearly a full week spent with most of the week still to run.
        let early = window(ONE_WEEK, None, 90.0, 6 * 86_400);
        assert!(ahead_of_pace(&early, Timestamp::now()).is_some());

        // The same figure at the end of the week is simply a spent week.
        let late = window(ONE_WEEK, None, 90.0, 3600);
        assert_eq!(ahead_of_pace(&late, Timestamp::now()), None);

        // A window with no reset time states no pace to be ahead of.
        let undated = Window {
            resets_at: None,
            ..early.clone()
        };
        assert_eq!(ahead_of_pace(&undated, Timestamp::now()), None);
    }

    #[test]
    fn meters_scale_and_never_overflow() {
        assert_eq!(meter(0.0, 10), "░░░░░░░░░░ ");
        assert_eq!(meter(50.0, 10), "█████░░░░░ ");
        assert_eq!(meter(100.0, 10), "██████████ ");
        assert_eq!(meter(140.0, 10), "██████████ ");
    }

    #[test]
    fn severity_escalates_with_usage() {
        assert_eq!(severity(10.0), Color::Green);
        assert_eq!(severity(80.0), Color::Yellow);
        assert_eq!(severity(95.0), Color::LightRed);
        assert_eq!(severity(100.0), Color::Red);
    }

    #[test]
    fn renders_in_a_very_small_terminal_without_panicking() {
        let mut app = App::for_tests(crate::tui::app::sample_statuses(3));
        app.mode = Mode::Help;
        for (width, height) in [(20u16, 5u16), (40, 10), (200, 60)] {
            render(&mut app, width, height);
        }
    }

    #[test]
    fn draws_guidance_when_there_are_no_accounts() {
        let screen = render(&mut App::for_tests(Vec::new()), 80, 20);
        assert!(screen.contains("No accounts yet"), "{screen}");
    }

    #[test]
    fn modals_cover_the_list_and_state_the_choice() {
        let mut app = App::for_tests(crate::tui::app::sample_statuses(1));
        app.mode = Mode::ConfirmRemove {
            id: "claude-1".into(),
            name: "dev@example.com".into(),
        };
        let screen = render(&mut app, 80, 24);
        assert!(screen.contains("Remove claude-1 (dev@example.com)?"), "{screen}");

        app.mode = Mode::ChooseProvider(ProviderAction::Add);
        let screen = render(&mut app, 80, 24);
        assert!(
            screen.contains("1  Claude Code") && screen.contains("2  Codex"),
            "{screen}"
        );
    }
}
