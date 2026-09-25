//! Rendering. Nothing here mutates anything but the frame.
//!
//! An account is a block rather than a row, because a row can only show the
//! worst of its limits and the worst limit is not the whole story: an account
//! at 5% of its five hours and 98% of its week is nearly spent, and one the
//! other way round is fine in an hour. Every window is on screen for every
//! account, so two accounts can be compared without selecting either.
//!
//! Colours are the terminal's own named ones, never fixed RGB values, so the
//! interface follows whatever theme the terminal is set to, light or dark.

use jiff::Timestamp;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};

use super::app::{App, Mode, ProviderAction, Row};
use super::login::{self, Login};
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

/// The one colour that means "this is where you are".
const ACCENT: Color = Color::Cyan;
/// Everything that supports the text rather than being it.
const DIM: Color = Color::DarkGray;

/// Frames of the spinner shown while something runs.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    // The keys keep a line of their own. Sharing it with whatever just
    // happened meant that every time something happened, the way to do the
    // next thing disappeared.
    let [header, rule, body, message, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    frame.render_widget(
        Paragraph::new("─".repeat(rule.width as usize)).style(Style::new().fg(DIM)),
        rule,
    );
    draw_body(frame, body, app);
    draw_message(frame, message, app);
    draw_keys(frame, keys, app);

    if app.mode != Mode::Browse {
        // The footer stays lit: it holds the keys for the dialog.
        dim_backdrop(frame, header.union(body));
    }
    match app.mode.clone() {
        Mode::Help => draw_help(frame),
        Mode::ChooseProvider { action, index } => draw_provider_chooser(frame, action, index),
        Mode::ConfirmRemove { id, name } => draw_confirm_remove(frame, &id, &name),
        Mode::Login => {
            if let Some(login) = &app.login {
                draw_login(frame, login, spinner(app));
            }
        }
        Mode::Browse => {}
    }
}

/// Greys out what is already drawn in `area`, so a dialog stands in front of
/// the list rather than among it.
fn dim_backdrop(frame: &mut Frame, area: Rect) {
    frame.buffer_mut().set_style(
        area,
        Style::new()
            .fg(DIM)
            .bg(Color::Reset)
            .remove_modifier(Modifier::BOLD),
    );
}

/// The name, and a tab for every agent: which one is showing is the question
/// the header answers.
fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(" ◆ ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled("agent-meter", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("   "),
    ];
    let tabs = std::iter::once((None, "All", app.statuses.len())).chain(ProviderKind::ALL.map(|kind| {
        let count = app
            .statuses
            .iter()
            .filter(|status| status.account.provider == kind)
            .count();
        (Some(kind), kind.display_name(), count)
    }));
    for (filter, name, count) in tabs {
        if filter == app.filter {
            spans.push(Span::styled(
                format!(" {name} {count} "),
                Style::new()
                    .bg(ACCENT)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw(format!(" {name} ")));
            spans.push(Span::styled(format!("{count} "), Style::new().fg(DIM)));
        }
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &mut App) {
    app.visible_height = area.height as usize;
    if app.rows.is_empty() {
        draw_empty(frame, area);
        return;
    }

    let now = Timestamp::now();
    let width = area.width as usize;
    let mut lines = Vec::new();
    for (index, row) in app.rows.iter().enumerate() {
        match row {
            Row::Provider(kind) => lines.push(provider_heading(*kind, app, width)),
            Row::Account { status, position } => {
                let selected = app.selected_row() == Some(index);
                lines.extend(account_block(status, *position, selected, app, now, width));
            }
        }
    }

    // Keep the selected block in view without a scrollbar: the list is short
    // enough that a moving window is less to read than a bar beside it.
    let first = app.scroll.min(lines.len().saturating_sub(1));
    let shown: Vec<Line> = lines.into_iter().skip(first).take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(shown), area);
}

fn draw_empty(frame: &mut Frame, area: Rect) {
    let key = |key: &'static str| Span::styled(key, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));
    let lines = vec![
        Line::from(Span::styled(
            "No accounts yet",
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(vec![
            key("i"),
            Span::styled(
                "  import the account an agent CLI is already signed in to",
                Style::new().fg(DIM),
            ),
        ]),
        Line::from(vec![
            key("a"),
            Span::styled("  log in to another one", Style::new().fg(DIM)),
        ]),
    ];
    let [middle] = Layout::vertical([Constraint::Length(lines.len() as u16)])
        .flex(Flex::Center)
        .areas(area);
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), middle);
}

/// A harness: which account it is on, and whether it picks its own.
fn provider_heading(kind: ProviderKind, app: &App, width: usize) -> Line<'static> {
    let count = app
        .rows
        .iter()
        .filter(|row| matches!(row, Row::Account { status, .. } if status.account.provider == kind))
        .count();
    let mut left = vec![
        Span::raw(" "),
        Span::styled(
            kind.display_name().to_string(),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {count}"), Style::new().fg(DIM)),
        Span::styled("  ·  ", Style::new().fg(DIM)),
    ];

    // Naming it here answers "which account is this agent on" without reading
    // down the list hunting for a marker.
    match app.active_of(kind) {
        Some(status) => {
            left.push(Span::styled("using ", Style::new().fg(DIM)));
            left.push(Span::styled(
                status.account.display_name().to_string(),
                Style::new().fg(Color::Green),
            ));
        }
        None => left.push(Span::styled(
            "not signed in to a stored account",
            Style::new().fg(Color::Yellow),
        )),
    }

    // Per harness, because they are switched on separately and one word for
    // both would be a lie about whichever is off.
    let status = if app.switching_on(kind) {
        Span::styled(
            format!(" ● auto at {:.0}% ", app.threshold),
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(" ○ manual ", Style::new().fg(DIM))
    };
    // The order is explained only where there is room: the status is the part
    // that must not be pushed off the edge.
    let mut right = Vec::new();
    let hint = "in the order they will be taken  ";
    if app.switching_on(kind) && spans_width(&left) + hint.len() + status.width() + 3 <= width {
        right.push(Span::styled(
            hint,
            Style::new().fg(DIM).add_modifier(Modifier::ITALIC),
        ));
    }
    right.push(status);
    spread(left, right, width)
}

/// One account: who it is, then every limit it has.
///
/// The selected one carries a bar down its whole height, so it reads as one
/// thing rather than a name with some lines after it.
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
    let gutter = || {
        if selected {
            Span::styled("┃", Style::new().fg(ACCENT))
        } else {
            Span::raw(" ")
        }
    };

    let name = if selected {
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };
    let mut head = vec![
        gutter(),
        Span::styled(format!(" {position}  "), Style::new().fg(DIM)),
        Span::styled(account.display_name().to_string(), name),
    ];
    let details: Vec<String> = [
        identity.workspace_label().map(|org| org.into_owned()),
        identity.plan_label(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !details.is_empty() {
        head.push(Span::styled(
            format!("  {}", details.join(" · ")),
            Style::new().fg(DIM),
        ));
    }

    let mut lines = vec![spread(head, standing(status, position, app, now), width)];
    for line in body(status, now, width) {
        let mut spans = vec![gutter()];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));
    lines
}

/// `left` at the start of a line and `right` a column short of its end, or
/// simply one after the other when the line is too narrow for both.
fn spread(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let gap = width
        .saturating_sub(spans_width(&left) + spans_width(&right) + 1)
        .max(2);
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    Line::from(left)
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// What is drawn under an account's name: every limit it has, or why it has
/// none to show.
fn body(status: &Status, now: Timestamp, width: usize) -> Vec<Line<'static>> {
    match (&status.account.needs_login, &status.error, &status.usage) {
        (Some(reason), _, _) => vec![note(
            "✗ signed out — press l to sign in again",
            reason,
            Color::Red,
            width,
        )],
        (None, Some(error), None) => vec![note("! no reading", error, Color::Yellow, width)],
        (None, error, Some(usage)) => {
            let mut lines: Vec<Line<'static>> = usage
                .windows
                .iter()
                .map(|window| window_line(window, now, meter_width(width)))
                .collect();
            if let Some(error) = error {
                lines.push(note("! not refreshed", error, Color::Yellow, width));
            }
            lines
        }
        (None, None, None) => vec![note("no reading yet — press r", "", DIM, width)],
    }
}

/// How many lines `body` draws.
///
/// Scrolling needs the height of a block before its lines exist, so it is
/// stated here rather than guessed at from the number of limits: an account
/// that needs a login shows one line whatever its last reading held, and an
/// account with both a reading and an error shows one more than it has limits.
/// Guessing got both wrong, which scrolled to a line other than the one the
/// selection was on. A test holds this and `body` together.
pub(super) fn body_height(status: &Status) -> usize {
    match (&status.account.needs_login, &status.error, &status.usage) {
        (Some(_), _, _) => 1,
        (None, Some(_), None) => 1,
        (None, error, Some(usage)) => usage.windows.len() + usize::from(error.is_some()),
        (None, None, None) => 1,
    }
}

/// What this account is, in a badge: in use, next, or spent.
fn standing(status: &Status, position: usize, app: &App, now: Timestamp) -> Vec<Span<'static>> {
    let badge = |text: &str, colour: Color| {
        vec![Span::styled(
            format!(" {text} "),
            Style::new()
                .bg(colour)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )]
    };
    if status.active {
        // Filled, rather than a word among words: this is the one fact
        // somebody opens the interface to find.
        return badge("IN USE", Color::Green);
    }
    if status.account.needs_login.is_some() {
        return vec![Span::styled(" login needed ", Style::new().fg(Color::Red))];
    }
    if status
        .usage
        .as_ref()
        .is_some_and(|usage| usage.is_exhausted_at(now))
    {
        return vec![Span::styled(" spent ", Style::new().fg(Color::Red))];
    }
    // Only meaningful when something is actually choosing: with switching off
    // the order is just an order.
    if app.switching_on(status.account.provider) && position == 2 {
        return vec![Span::styled(
            " next up ",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
    }
    Vec::new()
}

/// How wide the meters are in a window `width` columns across.
///
/// Full size where there is room, and narrower rather than cut off where
/// there is not: everything after the meter on a line — when the window
/// resets, and whether it is being spent too fast — is worth more than the
/// meter's resolution.
fn meter_width(width: usize) -> usize {
    // Everything else on the longest line a window draws.
    const REST: usize = 66;
    width.saturating_sub(REST).clamp(10, BAR)
}

/// One limit: its name, a meter, the figure, and when it turns over.
fn window_line(window: &Window, now: Timestamp, bar: usize) -> Line<'static> {
    let used = window.used_at(now);
    let colour = severity(used);
    let (filled, track) = meter(used, bar);
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(
            format!("{:<14}", truncate(&window.label(), 14)),
            Style::new().fg(Color::Gray),
        ),
        Span::styled(filled, Style::new().fg(colour)),
        Span::styled(track, Style::new().fg(DIM)),
        Span::styled(
            format!("{used:>5.0}%"),
            Style::new().fg(colour).add_modifier(Modifier::BOLD),
        ),
    ];

    match window.resets_at {
        Some(at) if at > now => spans.push(Span::styled(
            format!("   resets in {}", timefmt::until(now, at)),
            Style::new().fg(DIM),
        )),
        Some(_) => spans.push(Span::styled("   resetting", Style::new().fg(DIM))),
        None => {}
    }
    // Spending faster than the window refills is the thing a percentage alone
    // cannot say: 60% of a week is fine on day five and a warning on day two.
    if let Some(over) = ahead_of_pace(window, now) {
        spans.push(Span::styled(
            format!("   ▲ {over:.0}% ahead of pace"),
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

/// Why an account has no figures: what it means in `colour`, then the
/// provider's own words, dimmed, since they explain rather than instruct.
fn note(headline: &str, detail: &str, colour: Color, width: usize) -> Line<'static> {
    // Provider messages run long, and a line that overruns the window is cut
    // mid-word with no sign that anything is missing.
    let room = width.saturating_sub(6);
    let headline = truncate(headline, room);
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(headline.clone(), Style::new().fg(colour)),
    ];
    let left = room.saturating_sub(headline.chars().count() + 3);
    if !detail.is_empty() && left > 0 {
        spans.push(Span::styled(
            format!(" · {}", truncate(detail, left)),
            Style::new().fg(DIM),
        ));
    }
    Line::from(spans)
}

/// A meter, as the part used and the part left: `━━━━━━╸` and `━━━━━━━━`.
///
/// Drawn to half a cell, so two figures a few points apart do not draw the
/// same bar. The two parts are coloured separately by the caller.
fn meter(used: f64, width: usize) -> (String, String) {
    let halves = ((used / 100.0).clamp(0.0, 1.0) * (width * 2) as f64).round() as usize;
    let (full, half) = (halves / 2, halves % 2);
    let mut filled = "━".repeat(full);
    if half == 1 {
        filled.push('╸');
    }
    (filled, "━".repeat(width - full - half))
}

fn severity(used: f64) -> Color {
    match used {
        u if u >= 100.0 => Color::Red,
        u if u >= HIGH_PERCENT => Color::LightRed,
        u if u >= WARN_PERCENT => Color::Yellow,
        _ => Color::Green,
    }
}

/// `text`, cut to at most `width` characters, with an ellipsis where it was
/// cut.
///
/// Counted in characters throughout. Cutting at a byte offset one back from a
/// character boundary lands inside whatever multi-byte character precedes it —
/// an em dash, or a name that is not written in ASCII — and panics, which
/// takes the whole interface down mid-draw.
fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(width - 1).collect();
    cut.push('…');
    cut
}

/// `text` broken into lines of at most `width` characters, at spaces where
/// there are any.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        // A word longer than a whole line is split wherever it has to be.
        while word.chars().count() > width {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let rest = word.chars().skip(width).collect();
            lines.push(word.chars().take(width).collect());
            word = rest;
        }
        let needed = line.chars().count() + usize::from(!line.is_empty()) + word.chars().count();
        if needed > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The spinner's current frame, counted from when the interface started so
/// it turns at the same speed however often the screen is drawn.
fn spinner(app: &App) -> &'static str {
    SPINNER[(app.epoch.elapsed().as_millis() / 80) as usize % SPINNER.len()]
}

fn draw_message(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(4) as usize;
    let line = if let Some(busy) = &app.busy {
        Line::from(vec![
            Span::styled(format!(" {} ", spinner(app)), Style::new().fg(ACCENT)),
            Span::styled(truncate(busy, width), Style::new().fg(ACCENT)),
        ])
    } else if let Some(message) = &app.message {
        let (mark, colour) = if message.is_error {
            ("✗", Color::Red)
        } else {
            ("✓", Color::Green)
        };
        Line::from(vec![
            Span::styled(
                format!(" {mark} "),
                Style::new().fg(colour).add_modifier(Modifier::BOLD),
            ),
            Span::styled(truncate(&message.text, width), Style::new().fg(colour)),
        ])
    } else {
        Line::raw("")
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// What can be done from here: always on screen, and always for this screen.
fn draw_keys(frame: &mut Frame, area: Rect, app: &App) {
    let keys: Vec<(&str, String)> = match &app.mode {
        Mode::Help => vec![("any key", "close".into())],
        Mode::ChooseProvider { .. } => vec![
            ("↑↓", "choose".into()),
            ("enter", "confirm".into()),
            ("esc", "cancel".into()),
        ],
        Mode::ConfirmRemove { .. } => vec![("y", "remove".into()), ("any key", "cancel".into())],
        Mode::Login => match &app.login {
            Some(login) if !login.is_running() => vec![("esc", "close".into())],
            Some(login) if login.wants_input() => vec![
                ("enter", "submit code".into()),
                ("ctrl+o", "open link".into()),
                ("ctrl+y", "copy link".into()),
                ("esc", "cancel".into()),
            ],
            _ => vec![
                ("o", "open link".into()),
                ("y", "copy link".into()),
                ("esc", "cancel".into()),
            ],
        },
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
                ("l", "sign in".into()),
                ("w", switching),
                ("tab", "agents".into()),
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
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1usize;
    for (key, what) in keys {
        let width = key.chars().count() + what.chars().count() + 4;
        if used + width > area.width as usize {
            break;
        }
        used += width;
        spans.push(Span::styled(
            key.to_string(),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {what}   "), Style::new().fg(DIM)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(frame: &mut Frame) {
    let row = |key: &'static str, what: &'static str| {
        Line::from(vec![
            Span::styled(
                format!("{key:<12}"),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(what),
        ])
    };
    let lines = vec![
        row("↑ ↓  j k", "select an account"),
        row("enter  u", "sign the agent CLI in to it"),
        row("l", "sign in to the selected account again"),
        row("a", "log in to a new account"),
        row("i", "import the account a CLI is signed in to"),
        row("d", "forget the selected account"),
        row("w", "switch automatically as limits approach"),
        row("tab  p", "show one agent at a time, or all"),
        row("r", "read usage now"),
        row("q  esc", "quit"),
        Line::raw(""),
        Line::from(Span::styled(
            "With switching on, accounts are listed in the order they",
            Style::new().fg(DIM),
        )),
        Line::from(Span::styled(
            "will be taken: the one in use first, then the one next.",
            Style::new().fg(DIM),
        )),
    ];
    let area = popup(frame.area(), 64, lines.len() as u16 + 4);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(dialog("Keys")), area);
}

fn draw_provider_chooser(frame: &mut Frame, action: ProviderAction, index: usize) {
    let mut lines = vec![
        Line::from(Span::styled(action.title(), Style::new().fg(DIM))),
        Line::raw(""),
    ];
    for (at, provider) in ProviderKind::ALL.iter().enumerate() {
        let (marker, style) = if at == index {
            ("▸", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))
        } else {
            (" ", Style::new())
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), Style::new().fg(ACCENT)),
            Span::styled(format!("{}  ", at + 1), Style::new().fg(DIM)),
            Span::styled(provider.display_name(), style),
        ]));
    }

    let area = popup(frame.area(), 46, lines.len() as u16 + 4);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(dialog("Which agent?")), area);
}

fn draw_confirm_remove(frame: &mut Frame, id: &str, name: &str) {
    let lines = vec![
        Line::from(vec![
            Span::raw("Remove "),
            Span::styled(id.to_string(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!(" ({name})?")),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "The account itself is untouched: agent-meter just",
            Style::new().fg(DIM),
        )),
        Line::from(Span::styled(
            "forgets it, and you can add it back by signing in.",
            Style::new().fg(DIM),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("y", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled(" remove   ", Style::new().fg(DIM)),
            Span::styled("any key", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", Style::new().fg(DIM)),
        ]),
    ];
    let area = popup(frame.area(), 58, lines.len() as u16 + 4);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(dialog("Remove account")), area);
}

/// The login panel: where the sign-in page is, a field for the code it may
/// hand back, and what the CLI has said.
fn draw_login(frame: &mut Frame, login: &Login, spinner: &str) {
    let screen = frame.area();
    // The tallest it may be; it is drawn only as tall as what it holds.
    let limit = popup(screen, 86, screen.height.saturating_sub(4).clamp(12, 22));
    let inner = limit.width.saturating_sub(4) as usize;
    let label = |text: &'static str| Line::from(Span::styled(text, Style::new().fg(DIM)));

    let mut lines = Vec::new();
    match &login.state {
        login::State::Failed(error) => {
            for (at, line) in wrap(error, inner.saturating_sub(2)).into_iter().enumerate() {
                let mark = if at == 0 { "✗ " } else { "  " };
                lines.push(Line::from(Span::styled(
                    format!("{mark}{line}"),
                    Style::new().fg(Color::Red),
                )));
            }
        }
        _ => {
            let elapsed = login.started.elapsed().as_secs();
            lines.push(Line::from(vec![
                Span::styled(format!("{spinner} "), Style::new().fg(ACCENT)),
                Span::raw("Waiting for you to sign in in your browser…"),
                Span::styled(
                    format!("  {}:{:02}", elapsed / 60, elapsed % 60),
                    Style::new().fg(DIM),
                ),
            ]));
        }
    }
    lines.push(Line::raw(""));

    lines.push(label("Sign-in page"));
    match login.link() {
        Some(link) => lines.push(Line::from(Span::styled(
            truncate(link, inner),
            Style::new().fg(ACCENT).add_modifier(Modifier::UNDERLINED),
        ))),
        None => lines.push(Line::from(Span::styled(
            format!("Starting {}…", login.provider.display_name()),
            Style::new().fg(DIM),
        ))),
    }

    if login.is_running() && login.wants_input() {
        lines.push(Line::raw(""));
        lines.push(label("If the page shows a code, paste it here and press enter"));
        // The end of what was typed, when it is longer than the field.
        let room = inner.saturating_sub(3);
        let typed: String = {
            let count = login.input.chars().count();
            login.input.chars().skip(count.saturating_sub(room)).collect()
        };
        lines.push(Line::from(vec![
            Span::styled("› ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw(typed),
            Span::styled("█", Style::new().fg(ACCENT)),
        ]));
    }

    // What the CLI said, newest last, in whatever room is left.
    let output = login.lines();
    // Borders, the padding above, and a blank line below.
    let frame_lines = 4;
    let room = (limit.height as usize)
        .saturating_sub(frame_lines)
        .saturating_sub(lines.len() + 2);
    if room > 0 && !output.is_empty() {
        lines.push(Line::raw(""));
        lines.push(label("Output"));
        for line in output.iter().skip(output.len().saturating_sub(room)) {
            lines.push(Line::from(Span::styled(
                truncate(line, inner),
                Style::new().fg(Color::Gray),
            )));
        }
    }

    let title = match &login.account {
        Some(account) => format!("Sign in to {account} again"),
        None => format!("Log in to a new {} account", login.provider.display_name()),
    };
    let area = popup(screen, limit.width, (lines.len() + frame_lines) as u16);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(dialog(&title)), area);
}

/// A dialog's frame: rounded, in the accent colour, with room inside.
fn dialog(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 1, 0))
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

    /// Every state an account can be drawn in, for the tests that have to
    /// cover all of them.
    fn every_state() -> Vec<Status> {
        let mut statuses = crate::tui::app::sample_statuses(5);
        let limits = || {
            usage(vec![
                window(FIVE_HOURS, None, 68.0, 3 * 3600),
                window(ONE_WEEK, None, 76.0, 4 * 86_400),
                window(ONE_WEEK, Some("Fable"), 19.0, 4 * 86_400),
            ])
        };
        let long = "the provider rejected its credential: invalid_grant (Refresh token not found \
                    or invalid)";
        // A reading, and nothing wrong.
        statuses[0].usage = Some(limits());
        // Signed out, but still holding the reading from before it was.
        statuses[1].usage = Some(limits());
        statuses[1].account.needs_login = Some(long.into());
        // A reading that could not be brought up to date.
        statuses[2].usage = Some(limits());
        statuses[2].error = Some(long.into());
        // An error and no reading at all.
        statuses[3].error = Some(long.into());
        // Never read.
        statuses
    }

    /// Scrolling needs a block's height before the block exists, so the height
    /// is stated in one place and the lines drawn in another. When they
    /// disagree the interface scrolls to a line other than the one the
    /// selection is on, and moving through the list walks the screen off the
    /// account it says is selected.
    #[test]
    fn a_blocks_stated_height_is_the_number_of_lines_it_draws() {
        for status in every_state() {
            let drawn = body(&status, Timestamp::now(), 110).len();
            assert_eq!(
                body_height(&status),
                drawn,
                "{}: said {} lines, drew {drawn}",
                status.account.id,
                body_height(&status),
            );
        }
    }

    /// What the user does: hold a key and watch the list go by. The account
    /// the interface says is selected has to be one that is on the screen.
    #[test]
    fn moving_through_the_list_keeps_the_selected_account_in_view() {
        let statuses = every_state();
        let names: Vec<String> = statuses.iter().map(|status| status.account.id.clone()).collect();
        let mut app = App::for_tests(statuses);

        // A window too short for the list, so the view has to follow.
        render(&mut app, 110, 14);
        for direction in [1, -1] {
            for _ in 0..names.len() + 1 {
                app.move_selection(direction);
                let screen = render(&mut app, 110, 14);
                let selected = app.selected().expect("a selected account").account.id.clone();
                assert!(
                    screen.contains(&selected),
                    "selected {selected} is off screen:\n{screen}"
                );
            }
        }
    }

    /// Provider messages are long and arrive in whatever alphabet the account
    /// is named in. Cutting one at a byte offset lands inside a character and
    /// panics, which takes the interface down in the middle of a frame.
    #[test]
    fn a_line_is_cut_by_characters_and_never_through_one() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");

        // The characters this interface actually prints, and ones it may be
        // handed: an em dash before the cut, and a name outside ASCII.
        for text in [
            "sign in again — the provider rejected its credential",
            "просроченный токен обновления",
            "アカウントの認証が必要です",
            "✅ fine ▌ also fine █░…",
        ] {
            for width in 0..text.chars().count() + 2 {
                let cut = truncate(text, width);
                assert!(
                    cut.chars().count() <= width,
                    "{text:?} cut to {width} gave {cut:?}"
                );
            }
        }
    }

    #[test]
    fn wrapping_breaks_at_spaces_and_splits_only_what_cannot_fit() {
        assert_eq!(
            wrap("the login exited with 1", 10),
            ["the login", "exited", "with 1"]
        );
        assert_eq!(wrap("abcdefghijkl", 5), ["abcde", "fghij", "kl"]);
        for line in wrap(
            "просроченный токен обновления — https://example.com/very/long/path",
            7,
        ) {
            assert!(line.chars().count() <= 7, "{line:?}");
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
        assert!(screen.contains("resets in 3h"), "{screen}");
        // The account in use says so.
        assert!(
            screen.contains("IN USE"),
            "the account in use is unmistakable:
{screen}"
        );
        // And the harness groups them.
        assert!(screen.contains("Claude Code"), "{screen}");
    }

    /// A badge is pushed against the right edge, but never past it: at any
    /// width the status of an account is on screen.
    #[test]
    fn the_badge_stays_on_screen_at_every_width() {
        let mut app = App::for_tests(crate::tui::app::sample_statuses(1));
        for width in [60u16, 80, 110, 160] {
            let screen = render(&mut app, width, 20);
            assert!(screen.contains("IN USE"), "at {width}:\n{screen}");
            let line = screen.lines().find(|line| line.contains("IN USE")).unwrap();
            assert!(line.chars().count() <= width as usize);
        }
    }

    /// With switching on the list is the queue, so the account that would be
    /// taken next says so.
    #[test]
    fn the_next_account_is_named_only_while_switching_is_on() {
        let mut statuses = crate::tui::app::sample_statuses(2);
        statuses[0].usage = Some(usage(vec![window(FIVE_HOURS, None, 95.0, 3600)]));
        statuses[1].usage = Some(usage(vec![window(FIVE_HOURS, None, 5.0, 3600)]));

        let mut app = App::for_tests(statuses);
        let screen = render(&mut app, 110, 30);
        assert!(
            !screen.contains("next up"),
            "with switching off the order is just an order"
        );
        assert!(screen.contains("○ manual"), "{screen}");

        app.switching = ProviderKind::ALL.map(|kind| (kind, true)).to_vec();
        let screen = render(&mut app, 110, 30);
        assert!(screen.contains("next up"), "{screen}");
        assert!(screen.contains("in the order they will be taken"), "{screen}");
        assert!(screen.contains("● auto at 90%"), "{screen}");
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
    fn meters_scale_to_half_a_cell_and_never_overflow() {
        let drawn = |used| {
            let (filled, track) = meter(used, 10);
            format!("{filled}|{track}")
        };
        assert_eq!(drawn(0.0), "|━━━━━━━━━━");
        assert_eq!(drawn(50.0), "━━━━━|━━━━━");
        assert_eq!(drawn(55.0), "━━━━━╸|━━━━");
        assert_eq!(drawn(100.0), "━━━━━━━━━━|");
        assert_eq!(drawn(140.0), "━━━━━━━━━━|");
        for used in 0..=100 {
            let (filled, track) = meter(used as f64, BAR);
            assert_eq!(filled.chars().count() + track.chars().count(), BAR, "{used}%");
        }
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
        for mode in [
            Mode::Help,
            Mode::ChooseProvider {
                action: ProviderAction::Add,
                index: 1,
            },
            Mode::ConfirmRemove {
                id: "claude-1".into(),
                name: "a very long name that will not fit anywhere".into(),
            },
            Mode::Browse,
        ] {
            app.mode = mode;
            for (width, height) in [(1u16, 1u16), (20, 5), (40, 10), (200, 60)] {
                render(&mut app, width, height);
            }
        }
    }

    #[test]
    fn draws_guidance_when_there_are_no_accounts() {
        let screen = render(&mut App::for_tests(Vec::new()), 80, 20);
        assert!(screen.contains("No accounts yet"), "{screen}");
    }

    /// The header says which agents there are and which one is showing.
    #[test]
    fn the_header_has_a_tab_for_every_agent_with_its_count() {
        let mut statuses = crate::tui::app::sample_statuses(3);
        statuses[2].account.id = "codex-1".into();
        statuses[2].account.provider = ProviderKind::Codex;
        let screen = render(&mut App::for_tests(statuses), 110, 20);
        let header = screen.lines().next().unwrap();
        assert!(header.contains("All 3"), "{header}");
        assert!(header.contains("Claude Code 2"), "{header}");
        assert!(header.contains("Codex 1"), "{header}");
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

        app.mode = Mode::ChooseProvider {
            action: ProviderAction::Add,
            index: 1,
        };
        let screen = render(&mut app, 80, 24);
        assert!(
            screen.contains("1  Claude Code") && screen.contains("▸ 2  Codex"),
            "the cursor is on the second agent:\n{screen}"
        );
    }
}
