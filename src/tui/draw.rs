//! Rendering. Nothing here mutates anything but the frame.

use jiff::Timestamp;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, Wrap};

use super::app::{App, Mode, ProviderAction};
use crate::account::ProviderKind;
use crate::engine::Status;
use crate::timefmt;

/// Usage at which a window is drawn as nearly spent.
const WARN_PERCENT: f64 = 75.0;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, list, detail, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(7),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    draw_list(frame, list, app);
    draw_detail(frame, detail, app);
    draw_footer(frame, footer, app);

    match app.mode.clone() {
        Mode::Help => draw_help(frame),
        Mode::ChooseProvider(action) => draw_provider_chooser(frame, action),
        Mode::ConfirmRemove { id, name } => draw_confirm_remove(frame, &id, &name),
        Mode::Browse => {}
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let watching = if app.watching {
        Span::styled(
            format!(" watching, switches at {:.0}% ", app.threshold),
            Style::new().bg(Color::Green).fg(Color::Black),
        )
    } else {
        Span::styled(" watching off ", Style::new().fg(Color::DarkGray))
    };
    let line = Line::from(vec![
        Span::styled(
            " agent-meter ",
            Style::new().bg(Color::Blue).fg(Color::White).bold(),
        ),
        Span::raw(" "),
        watching,
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let now = Timestamp::now();
    if app.statuses.is_empty() {
        let text = Paragraph::new(vec![
            Line::raw(""),
            Line::raw("No accounts yet."),
            Line::raw(""),
            Line::raw("Press i to import the account an agent CLI is already signed in to,"),
            Line::raw("or a to log in to another one."),
        ])
        .alignment(Alignment::Center)
        .block(bordered("Accounts"));
        frame.render_widget(text, area);
        return;
    }

    let rows: Vec<Row> = app
        .statuses
        .iter()
        .map(|status| {
            let used = status.used(now);
            let identity = &status.account.identity;
            let note = match (&status.account.needs_login, &status.error) {
                (Some(_), _) => Span::styled("login needed", Style::new().fg(Color::Red)),
                (None, Some(_)) => Span::styled("usage unavailable", Style::new().fg(Color::Yellow)),
                (None, None) => Span::raw(""),
            };
            Row::new(vec![
                Cell::from(if status.active { "▶" } else { " " }).style(Style::new().fg(Color::Green)),
                Cell::from(status.account.id.clone()),
                Cell::from(status.account.provider.display_name()),
                Cell::from(status.account.display_name().to_string()),
                // The organisation is part of which account this is: the same
                // address in two of them is two accounts, with separate limits.
                Cell::from(identity.workspace_name.clone().unwrap_or_else(|| "-".into())),
                Cell::from(identity.plan_label().unwrap_or_else(|| "-".into())),
                Cell::from(bar(used, 14)).style(used_style(used, status)),
                Cell::from(Line::from(note)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Min(16),
            Constraint::Length(14),
            Constraint::Length(9),
            Constraint::Length(22),
            Constraint::Length(18),
        ],
    )
    .header(
        Row::new([
            "",
            "ID",
            "PROVIDER",
            "ACCOUNT",
            "ORGANIZATION",
            "PLAN",
            "USED",
            "",
        ])
        .style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
    )
    .row_highlight_style(Style::new().bg(Color::DarkGray))
    .block(bordered("Accounts"));

    frame.render_stateful_widget(table, area, &mut app.table);
}

/// A textual meter: `███████░░░░░░  56%`.
fn bar(used: Option<f64>, width: usize) -> String {
    let Some(used) = used else {
        return format!("{:width$}   ?", "", width = width);
    };
    let filled = ((used / 100.0).clamp(0.0, 1.0) * width as f64).round() as usize;
    format!("{}{}{used:4.0}%", "█".repeat(filled), "░".repeat(width - filled))
}

fn used_style(used: Option<f64>, status: &Status) -> Style {
    let now = Timestamp::now();
    if status.usage.as_ref().is_some_and(|u| u.is_exhausted_at(now)) {
        return Style::new().fg(Color::Red);
    }
    match used {
        None => Style::new().fg(Color::DarkGray),
        Some(used) if used >= 90.0 => Style::new().fg(Color::Yellow),
        Some(used) if used >= WARN_PERCENT => Style::new().fg(Color::LightYellow),
        Some(_) => Style::new().fg(Color::Green),
    }
}

/// The selected account's individual limit windows.
fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let now = Timestamp::now();
    let Some(status) = app.selected() else {
        frame.render_widget(bordered("Limits"), area);
        return;
    };
    let title = format!("Limits — {}", status.account.display_name());
    let block = bordered(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(reason) = &status.account.needs_login {
        let text = format!("This account needs a fresh login: {reason}\nPress a to sign in again.");
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).fg(Color::Red),
            inner,
        );
        return;
    }
    let Some(usage) = &status.usage else {
        let text = match &status.error {
            Some(error) => format!("Usage could not be read: {error}"),
            None => "No usage reading yet. Press r to fetch one.".to_string(),
        };
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).fg(Color::DarkGray),
            inner,
        );
        return;
    };

    let rows = Layout::vertical(vec![Constraint::Length(1); usage.windows.len().max(1)]).split(inner);
    for (window, row) in usage.windows.iter().zip(rows.iter()) {
        let used = window.used_at(now);
        let [label, meter, resets] = Layout::horizontal([
            Constraint::Length(20),
            Constraint::Min(12),
            Constraint::Length(12),
        ])
        .areas(*row);
        frame.render_widget(Paragraph::new(window.label()), label);
        // A filled/hollow bar rather than a coloured line, so the reading is
        // legible in a monochrome terminal too.
        frame.render_widget(
            Paragraph::new(bar(Some(used), meter.width.saturating_sub(6) as usize)).fg(gauge_color(used)),
            meter,
        );
        let resets_in = window
            .resets_at
            .map(|at| format!("in {}", timefmt::until(now, at)))
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(resets_in)
                .alignment(Alignment::Right)
                .fg(Color::DarkGray),
            resets,
        );
    }
}

fn gauge_color(used: f64) -> Color {
    match used {
        u if u >= 100.0 => Color::Red,
        u if u >= 90.0 => Color::Yellow,
        u if u >= WARN_PERCENT => Color::LightYellow,
        _ => Color::Green,
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(busy) = &app.busy {
        Line::from(Span::styled(busy.clone(), Style::new().fg(Color::Cyan)))
    } else if let Some(message) = &app.message {
        let color = if message.is_error {
            Color::Red
        } else {
            Color::Green
        };
        Line::from(Span::styled(message.text.clone(), Style::new().fg(color)))
    } else {
        Line::from(Span::styled(
            "↑↓ select   enter use   r refresh   a add   i import   d remove   w watch   ? help   q quit",
            Style::new().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_help(frame: &mut Frame) {
    let lines = vec![
        Line::from("↑ ↓ / j k    select an account".to_string()),
        Line::from("enter, u     sign the agent CLI in to it".to_string()),
        Line::from("r            read usage now".to_string()),
        Line::from("a            log in to a new account".to_string()),
        Line::from("i            import the account a CLI is signed in to".to_string()),
        Line::from("d            forget the selected account".to_string()),
        Line::from("w            switch automatically as limits approach".to_string()),
        Line::from("q, esc       quit".to_string()),
        Line::from(""),
        Line::from(Span::styled(
            "Press any key to close",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    let area = popup(frame.area(), 60, lines.len() as u16 + 2);
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
    use crate::usage::{FIVE_HOURS, ONE_WEEK, Usage, Window};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Renders the interface and returns what the terminal would show.
    fn render(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
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

    fn usage(five_hour: f64, weekly: f64) -> Usage {
        let now = Timestamp::now();
        Usage {
            observed_at: now,
            windows: vec![
                Window {
                    window_secs: FIVE_HOURS,
                    scope: None,
                    used_percent: five_hour,
                    resets_at: Some(now + jiff::SignedDuration::from_secs(3600)),
                },
                Window {
                    window_secs: ONE_WEEK,
                    scope: None,
                    used_percent: weekly,
                    resets_at: Some(now + jiff::SignedDuration::from_hours(50)),
                },
            ],
            limit_reached: false,
        }
    }

    #[test]
    fn draws_accounts_their_meters_and_the_selected_account_limits() {
        let mut statuses = crate::tui::app::sample_statuses(2);
        statuses[0].usage = Some(usage(94.0, 60.0));
        statuses[1].usage = Some(usage(10.0, 5.0));
        let mut app = App::for_tests(statuses);
        let screen = render(&mut app);

        // The account list shows both accounts, with the live one marked.
        assert!(screen.contains("claude-1"), "{screen}");
        assert!(screen.contains("claude-2"), "{screen}");
        assert!(
            screen.contains('▶'),
            "the active account should be marked:\n{screen}"
        );
        assert!(
            screen.contains("94%"),
            "the worst window drives the meter:\n{screen}"
        );

        // The detail pane breaks the selected account down by window.
        assert!(screen.contains("Limits"), "{screen}");
        assert!(screen.contains("5h"), "{screen}");
        assert!(screen.contains("weekly"), "{screen}");
        assert!(
            screen.contains("in 1h"),
            "reset countdowns should show:\n{screen}"
        );

        // The footer advertises the keys.
        assert!(screen.contains("q quit"), "{screen}");
    }

    #[test]
    fn draws_guidance_when_there_are_no_accounts() {
        let screen = render(&mut App::for_tests(Vec::new()));
        assert!(screen.contains("No accounts yet"), "{screen}");
        assert!(screen.contains("press") || screen.contains("Press"), "{screen}");
    }

    #[test]
    fn modals_cover_the_list_and_state_the_choice() {
        let mut app = App::for_tests(crate::tui::app::sample_statuses(1));
        app.mode = Mode::ConfirmRemove {
            id: "claude-1".into(),
            name: "dev@example.com".into(),
        };
        let screen = render(&mut app);
        assert!(screen.contains("Remove claude-1 (dev@example.com)?"), "{screen}");
        assert!(screen.contains("y to remove"), "{screen}");

        app.mode = Mode::ChooseProvider(ProviderAction::Add);
        let screen = render(&mut app);
        assert!(screen.contains("1  Claude Code"), "{screen}");
        assert!(screen.contains("2  Codex"), "{screen}");

        app.mode = Mode::Help;
        assert!(render(&mut app).contains("sign the agent CLI in to it"));
    }

    #[test]
    fn renders_in_a_very_small_terminal_without_panicking() {
        let mut app = App::for_tests(crate::tui::app::sample_statuses(3));
        app.mode = Mode::Help;
        for (width, height) in [(20u16, 5u16), (40, 10), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        }
    }

    #[test]
    fn bars_scale_and_handle_unknown_usage() {
        assert_eq!(bar(Some(0.0), 10), "░░░░░░░░░░   0%");
        assert_eq!(bar(Some(50.0), 10), "█████░░░░░  50%");
        assert_eq!(bar(Some(100.0), 10), "██████████ 100%");
        // Over 100% must not overflow the bar.
        assert_eq!(bar(Some(150.0), 10), "██████████ 150%");
        assert_eq!(bar(None, 4), "       ?");
    }

    #[test]
    fn popups_fit_inside_small_terminals() {
        let small = Rect::new(0, 0, 20, 5);
        let area = popup(small, 60, 12);
        assert!(area.width <= small.width && area.height <= small.height);
    }

    #[test]
    fn gauge_colors_escalate_with_usage() {
        assert_eq!(gauge_color(10.0), Color::Green);
        assert_eq!(gauge_color(80.0), Color::LightYellow);
        assert_eq!(gauge_color(95.0), Color::Yellow);
        assert_eq!(gauge_color(100.0), Color::Red);
    }
}
