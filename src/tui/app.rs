//! Interface state and the event loop.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::worker::{Job, Worker};
use crate::account::ProviderKind;
use crate::engine::{Engine, Status};

/// How often to redraw while idle. Short enough that countdowns tick visibly.
const FRAME: Duration = Duration::from_millis(250);

/// What the interface is waiting for the user to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Browsing the account list.
    Browse,
    /// Asking which provider an action applies to.
    ChooseProvider(ProviderAction),
    /// Asking whether to remove an account.
    ConfirmRemove { id: String, name: String },
    /// Showing the key bindings.
    Help,
}

/// An action that needs a provider before it can run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProviderAction {
    /// Log in to a new account.
    Add,
    /// Store the account a CLI is already signed in to.
    Import,
}

impl ProviderAction {
    pub fn title(self) -> &'static str {
        match self {
            ProviderAction::Add => "Log in to a new account",
            ProviderAction::Import => "Import the account already signed in",
        }
    }
}

/// One entry in the list: a harness heading, or an account under it.
///
/// The account is boxed because it is far larger than a heading, and the list
/// holds many more headings than it looks: every entry would otherwise be as
/// wide as the largest.
#[derive(Debug, Clone)]
pub enum Row {
    Provider(ProviderKind),
    Account {
        status: Box<Status>,
        /// Its place in the order, counted from one within its harness.
        position: usize,
    },
}

/// Everything the interface draws from.
pub struct App {
    pub statuses: Vec<Status>,
    /// The list as drawn: headings and accounts, in the order they are taken.
    pub rows: Vec<Row>,
    /// Which entry is selected, as an index into `rows`.
    selected: usize,
    /// First drawn line on screen, for scrolling a list taller than the window.
    pub scroll: usize,
    pub visible_height: usize,
    /// Show one harness at a time, or all of them.
    pub filter: Option<ProviderKind>,
    pub mode: Mode,
    /// The last thing that happened, shown in the footer.
    pub message: Option<Message>,
    /// Set while the worker is busy.
    pub busy: Option<String>,
    /// Whether automatic switching runs while the interface is open.
    pub watching: bool,
    pub threshold: f64,
    pub should_quit: bool,
    next_tick: Instant,
    tick_interval: Duration,
}

/// A line shown in the footer.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub text: String,
    pub is_error: bool,
}

impl App {
    fn new(engine: &Engine) -> Result<Self> {
        let interval = Duration::from_secs(engine.config().watch.poll_secs);
        let mut app = Self {
            statuses: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            visible_height: 24,
            filter: None,
            mode: Mode::Browse,
            message: None,
            busy: None,
            watching: false,
            threshold: engine.config().watch.threshold,
            should_quit: false,
            next_tick: Instant::now() + interval,
            tick_interval: interval,
        };
        app.reload(engine)?;
        Ok(app)
    }

    /// An interface built from the store but never run, for rendering a frame
    /// off-screen.
    pub(super) fn preview(engine: &Engine, watching: bool) -> Result<Self> {
        let mut app = Self::new(engine)?;
        app.watching = watching;
        app.rebuild(Some(engine));
        Ok(app)
    }

    /// An interface populated with fixed data, for rendering tests.
    #[cfg(test)]
    pub(crate) fn for_tests(statuses: Vec<Status>) -> Self {
        let mut app = Self {
            statuses,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            visible_height: 24,
            filter: None,
            mode: Mode::Browse,
            message: None,
            busy: None,
            watching: false,
            threshold: 90.0,
            should_quit: false,
            next_tick: Instant::now(),
            tick_interval: Duration::from_secs(60),
        };
        app.rebuild(None);
        app
    }

    /// Re-reads accounts and cached usage from the store.
    pub fn reload(&mut self, engine: &Engine) -> Result<()> {
        self.statuses = engine.status()?;
        self.rebuild(Some(engine));
        Ok(())
    }

    /// Rebuilds the list: grouped by harness, and within each one in the order
    /// the accounts would actually be taken.
    ///
    /// The selected account is kept selected across a rebuild, because the
    /// order moves underneath it — a poll can lift an account up the queue
    /// while somebody is still deciding what to do with the one they are
    /// looking at, and the selection should follow the account, not the place.
    fn rebuild(&mut self, engine: Option<&Engine>) {
        let keep = self.selected().map(|status| status.account.id.clone());
        self.rows.clear();

        for kind in ProviderKind::ALL {
            if self.filter.is_some_and(|only| only != kind) {
                continue;
            }
            let mut mine: Vec<Status> = self
                .statuses
                .iter()
                .filter(|status| status.account.provider == kind)
                .cloned()
                .collect();
            if mine.is_empty() {
                continue;
            }
            if let Some(engine) = engine {
                let order = engine.switch_order(&self.statuses, kind);
                mine.sort_by_key(|status| {
                    order
                        .iter()
                        .position(|id| *id == status.account.id)
                        .unwrap_or(usize::MAX)
                });
            }
            self.rows.push(Row::Provider(kind));
            for (index, status) in mine.into_iter().enumerate() {
                self.rows.push(Row::Account {
                    status: Box::new(status),
                    position: index + 1,
                });
            }
        }

        self.selected = keep
            .and_then(|id| self.row_of(&id))
            .or_else(|| self.first_account())
            .unwrap_or(0);
        self.follow_selection();
    }

    fn first_account(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Account { .. }))
    }

    fn row_of(&self, id: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Account { status, .. } if status.account.id == id))
    }

    /// The selected entry, when the selection is on an account.
    pub fn selected_row(&self) -> Option<usize> {
        matches!(self.rows.get(self.selected)?, Row::Account { .. }).then_some(self.selected)
    }

    pub fn selected(&self) -> Option<&Status> {
        match self.rows.get(self.selected)? {
            Row::Account { status, .. } => Some(status),
            Row::Provider(_) => None,
        }
    }

    /// Moves to the next or previous account, stepping over the headings.
    fn move_selection(&mut self, delta: isize) {
        let accounts: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, Row::Account { .. }))
            .map(|(index, _)| index)
            .collect();
        if accounts.is_empty() {
            return;
        }
        let at = accounts
            .iter()
            .position(|index| *index >= self.selected)
            .unwrap_or(accounts.len() - 1) as isize;
        let next = (at + delta).clamp(0, accounts.len() as isize - 1) as usize;
        self.selected = accounts[next];
        self.follow_selection();
    }

    /// Scrolls just enough to keep the selected account on screen.
    fn follow_selection(&mut self) {
        let height = self.visible_height.max(1);
        let drawn = self.drawn_offset(self.selected);
        if drawn < self.scroll {
            self.scroll = drawn;
        } else if drawn + 2 >= self.scroll + height {
            self.scroll = (drawn + 3).saturating_sub(height);
        }
    }

    /// How many lines are drawn before `row`.
    ///
    /// An account is as tall as the number of limits it has, so this is counted
    /// rather than assumed.
    fn drawn_offset(&self, row: usize) -> usize {
        self.rows
            .iter()
            .take(row)
            .map(|row| match row {
                Row::Provider(_) => 1,
                Row::Account { status, .. } => {
                    let windows = status
                        .usage
                        .as_ref()
                        .map_or(1, |usage| usage.windows.len().max(1));
                    // Its name, each limit, and the blank line after it.
                    2 + windows
                }
            })
            .sum()
    }

    /// What the header says is being shown.
    pub fn filter_label(&self) -> String {
        match self.filter {
            Some(kind) => format!("{} only", kind.display_name()),
            None => format!("{} accounts", self.statuses.len()),
        }
    }

    /// Cycles through showing every harness and each one alone.
    fn cycle_filter(&mut self, engine: Option<&Engine>) {
        let order: Vec<Option<ProviderKind>> =
            std::iter::once(None).chain(ProviderKind::ALL.map(Some)).collect();
        let at = order
            .iter()
            .position(|filter| *filter == self.filter)
            .unwrap_or(0);
        self.filter = order[(at + 1) % order.len()];
        self.rebuild(engine);
        let shown = self.filter_label();
        self.note(format!("Showing {shown}"));
    }

    fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.is_empty() {
            self.message = Some(Message {
                text,
                is_error: false,
            });
        }
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            text: text.into(),
            is_error: true,
        });
    }
}

/// Runs the interface until the user quits.
pub fn run(engine: &Engine) -> Result<()> {
    let mut terminal = ratatui::try_init().context("starting the terminal interface")?;
    let result = event_loop(engine, &mut terminal);
    ratatui::try_restore().context("restoring the terminal")?;
    result
}

fn event_loop(engine: &Engine, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let mut app = App::new(engine)?;
    let worker = Worker::spawn(engine.store().clone())?;
    // Start with fresh numbers, but respect the poll interval so opening the
    // interface repeatedly does not burn through the provider's budget.
    submit(&mut app, &worker, Job::Poll { force: false });

    while !app.should_quit {
        terminal.draw(|frame| super::draw::draw(frame, &mut app))?;

        if let Some(outcome) = worker.poll() {
            app.busy = None;
            match outcome {
                Ok(note) => app.note(note),
                Err(error) => app.fail(error),
            }
            app.reload(engine)?;
        }

        if app.watching && app.busy.is_none() && Instant::now() >= app.next_tick {
            app.next_tick = Instant::now() + app.tick_interval;
            submit(&mut app, &worker, Job::Tick);
        }

        if event::poll(FRAME)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            handle_key(&mut app, engine, &worker, terminal, key)?;
        }
    }
    Ok(())
}

fn submit(app: &mut App, worker: &Worker, job: Job) {
    if app.busy.is_some() {
        return;
    }
    if worker.submit(job.clone()) {
        app.busy = Some(job.describe());
    } else {
        app.fail("the background worker is not running");
    }
}

fn handle_key(
    app: &mut App,
    engine: &Engine,
    worker: &Worker,
    terminal: &mut ratatui::DefaultTerminal,
    key: KeyEvent,
) -> Result<()> {
    // Ctrl-C quits from anywhere, as it does in every other terminal program.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return Ok(());
    }

    match app.mode.clone() {
        Mode::Help => app.mode = Mode::Browse,
        Mode::ConfirmRemove { id, .. } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.mode = Mode::Browse;
                submit(app, worker, Job::Remove(id));
            }
            _ => {
                app.mode = Mode::Browse;
                app.note("Nothing was removed");
            }
        },
        Mode::ChooseProvider(action) => {
            let chosen = match key.code {
                KeyCode::Char('1') => Some(ProviderKind::Claude),
                KeyCode::Char('2') => Some(ProviderKind::Codex),
                _ => None,
            };
            app.mode = Mode::Browse;
            match (chosen, action) {
                (Some(provider), ProviderAction::Import) => submit(app, worker, Job::Import(provider)),
                (Some(provider), ProviderAction::Add) => login(app, engine, terminal, provider)?,
                (None, _) => {}
            }
        }
        Mode::Browse => browse_key(app, engine, worker, terminal, key)?,
    }
    Ok(())
}

fn browse_key(
    app: &mut App,
    engine: &Engine,
    worker: &Worker,
    terminal: &mut ratatui::DefaultTerminal,
    key: KeyEvent,
) -> Result<()> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Home => app.move_selection(-(app.rows.len() as isize)),
        KeyCode::End => app.move_selection(app.rows.len() as isize),
        KeyCode::Char('r') => submit(app, worker, Job::Poll { force: true }),
        KeyCode::Char('p') => app.cycle_filter(Some(engine)),
        KeyCode::Char('?') | KeyCode::F(1) => app.mode = Mode::Help,
        KeyCode::Char('a') => app.mode = Mode::ChooseProvider(ProviderAction::Add),
        KeyCode::Char('i') => app.mode = Mode::ChooseProvider(ProviderAction::Import),
        KeyCode::Char('w') => {
            app.watching = !app.watching;
            if app.watching {
                app.next_tick = Instant::now();
                app.note(format!("Switching on: will move at {:.0}%", app.threshold));
            } else {
                app.note("Switching off");
            }
            // The order means something different now, so rebuild it.
            app.rebuild(Some(engine));
        }
        KeyCode::Enter | KeyCode::Char('u') => match app.selected() {
            Some(status) if status.active => {
                let id = status.account.id.clone();
                app.note(format!("{id} is already the one in use"));
            }
            Some(status) => {
                let id = status.account.id.clone();
                submit(app, worker, Job::Switch(id));
            }
            None => app.fail("There are no accounts yet — press a to add one"),
        },
        KeyCode::Char('d') | KeyCode::Delete => match app.selected() {
            Some(status) => {
                app.mode = Mode::ConfirmRemove {
                    id: status.account.id.clone(),
                    name: status.account.display_name().to_string(),
                }
            }
            None => app.fail("There are no accounts to remove"),
        },
        _ => {}
    }
    let _ = terminal;
    Ok(())
}

/// Runs an interactive login, which needs the terminal the interface is using.
///
/// The interface stands down for the duration: an OAuth flow prints a URL and
/// may ask questions, and no redraw may scribble over it.
fn login(
    app: &mut App,
    engine: &Engine,
    terminal: &mut ratatui::DefaultTerminal,
    provider: ProviderKind,
) -> Result<()> {
    ratatui::try_restore().context("handing the terminal to the login")?;

    let result = engine.login(provider, None, false, |command| {
        println!(
            "Starting the {} login. Any running session keeps its own account.\n",
            provider.display_name()
        );
        command
            .status()
            .context("starting the login. Is the CLI installed and on PATH?")
    });

    *terminal = ratatui::try_init().context("taking the terminal back after the login")?;
    terminal.clear()?;

    match result {
        Ok(outcome) => {
            app.note(format!("Added {}", outcome.id()));
            // A first reading makes the new account eligible for switching.
            app.reload(engine)?;
        }
        Err(error) => app.fail(format!("{error:#}")),
    }
    Ok(())
}

/// Accounts with no usage reading, for tests in this module and in `draw`.
#[cfg(test)]
pub(crate) fn sample_statuses(count: usize) -> Vec<Status> {
    use crate::account::{Account, Credential, Identity, SCHEMA_VERSION};

    (1..=count)
        .map(|n| Status {
            account: Account {
                schema_version: SCHEMA_VERSION,
                id: format!("claude-{n}"),
                provider: ProviderKind::Claude,
                label: None,
                identity: Identity::default(),
                credential: Credential {
                    access_token: "a".into(),
                    refresh_token: "r".into(),
                    id_token: None,
                    expires_at: None,
                    refresh_expires_at: None,
                },
                provider_data: Default::default(),
                added_at: jiff::Timestamp::from_second(0).unwrap(),
                entitlement_checked_at: None,
                needs_login: None,
            },
            active: n == 1,
            usage: None,
            error: None,
            error_at: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::ProviderKind;
    use crate::store::Store;

    fn app_with(count: usize) -> App {
        App::for_tests(sample_statuses(count))
    }

    #[test]
    fn selection_moves_between_accounts_and_stays_inside_the_list() {
        let mut app = app_with(3);
        assert_eq!(app.selected().unwrap().account.id, "claude-1");

        app.move_selection(-1);
        assert_eq!(app.selected().unwrap().account.id, "claude-1");
        app.move_selection(10);
        assert_eq!(app.selected().unwrap().account.id, "claude-3");

        let mut empty = app_with(0);
        empty.move_selection(1);
        assert!(empty.selected().is_none());
    }

    /// A heading is not something to land on: moving goes account to account.
    #[test]
    fn headings_are_stepped_over() {
        let mut statuses = sample_statuses(2);
        statuses[1].account.id = "codex-1".into();
        statuses[1].account.provider = ProviderKind::Codex;
        let mut app = App::for_tests(statuses);

        assert!(matches!(app.rows[0], Row::Provider(ProviderKind::Claude)));
        assert!(matches!(app.rows[2], Row::Provider(ProviderKind::Codex)));

        app.move_selection(1);
        assert_eq!(
            app.selected().unwrap().account.id,
            "codex-1",
            "one press should cross the heading, not land on it"
        );
    }

    /// The order moves underneath the selection as readings change, so the
    /// selection follows the account rather than the place in the list.
    #[test]
    fn the_selected_account_stays_selected_when_the_order_changes() {
        let mut app = app_with(3);
        app.move_selection(2);
        assert_eq!(app.selected().unwrap().account.id, "claude-3");

        // The list is rebuilt with that account somewhere else entirely.
        app.statuses.reverse();
        app.rebuild(None);
        assert_eq!(app.selected().unwrap().account.id, "claude-3");
    }

    #[test]
    fn one_harness_can_be_shown_at_a_time() {
        let mut statuses = sample_statuses(2);
        statuses[1].account.id = "codex-1".into();
        statuses[1].account.provider = ProviderKind::Codex;
        let mut app = App::for_tests(statuses);

        let accounts = |app: &App| {
            app.rows
                .iter()
                .filter(|row| matches!(row, Row::Account { .. }))
                .count()
        };
        assert_eq!(accounts(&app), 2);
        assert_eq!(app.filter_label(), "2 accounts");

        app.cycle_filter(None);
        assert_eq!(app.filter, Some(ProviderKind::Claude));
        assert_eq!(accounts(&app), 1);
        assert_eq!(app.filter_label(), "Claude Code only");

        app.cycle_filter(None);
        assert_eq!(app.filter, Some(ProviderKind::Codex));
        assert_eq!(accounts(&app), 1);

        // And back to everything.
        app.cycle_filter(None);
        assert_eq!(app.filter, None);
        assert_eq!(accounts(&app), 2);
    }

    #[test]
    fn reload_survives_every_account_disappearing() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::with_store(Store::open(dir.path().join("data")).unwrap()).unwrap();
        let mut app = app_with(3);
        app.move_selection(2);
        // The store is empty, so every account disappears.
        app.reload(&engine).unwrap();
        assert!(app.selected().is_none());
        assert!(app.rows.is_empty());
    }
}
