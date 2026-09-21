//! Interface state and the event loop.

use std::io;
use std::sync::Once;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{DisableLineWrap, EnableLineWrap};

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
    /// Which harnesses switch automatically, as the configuration says.
    pub switching: Vec<(ProviderKind, bool)>,
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
            switching: Vec::new(),
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
    pub(super) fn preview(engine: &Engine, switching: bool) -> Result<Self> {
        let mut app = Self::new(engine)?;
        if switching {
            app.switching = ProviderKind::ALL.map(|kind| (kind, true)).to_vec();
        }
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
            switching: ProviderKind::ALL.map(|kind| (kind, false)).to_vec(),
            threshold: 90.0,
            should_quit: false,
            next_tick: Instant::now(),
            tick_interval: Duration::from_secs(60),
        };
        app.rebuild(None);
        app
    }

    /// Re-reads accounts, cached usage and the settings from the store.
    ///
    /// The settings are re-read rather than remembered, because `w` writes them
    /// and the watcher reads them: holding a copy here would let the screen and
    /// the thing doing the switching disagree.
    pub fn reload(&mut self, engine: &Engine) -> Result<()> {
        self.statuses = engine.status()?;
        if let Ok(config) = engine.store().config() {
            self.threshold = config.watch.threshold;
            self.switching = ProviderKind::ALL
                .map(|kind| (kind, config.is_switching_on(kind)))
                .to_vec();
        }
        self.rebuild(Some(engine));
        Ok(())
    }

    /// Whether this harness switches accounts on its own.
    pub fn switching_on(&self, kind: ProviderKind) -> bool {
        self.switching
            .iter()
            .find(|(provider, _)| *provider == kind)
            .is_some_and(|(_, on)| *on)
    }

    /// Whether anything is switching, which is when a tick is worth running.
    pub fn any_switching(&self) -> bool {
        self.switching.iter().any(|(_, on)| *on)
    }

    /// Which account each harness is signed in to, for the headings.
    pub fn active_of(&self, kind: ProviderKind) -> Option<&Status> {
        self.statuses
            .iter()
            .find(|status| status.account.provider == kind && status.active)
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
    pub(super) fn move_selection(&mut self, delta: isize) {
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
                // Its name, what is drawn under it, and the blank line after.
                Row::Account { status, .. } => 2 + super::draw::body_height(status),
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
    let mut terminal = claim_screen()?;
    let result = event_loop(engine, &mut terminal);
    release_screen()?;
    result
}

/// Takes the screen, and stops the terminal wrapping long lines while we have
/// it.
///
/// Every line is drawn to fit the width the terminal reports. When that width
/// is larger than the window actually is — which a Windows console does after
/// the alternate screen is entered — a line that reaches the last column wraps,
/// and the wrapped half lands on top of the row below it. One long line then
/// destroys the rest of the screen, and nothing redraws it, because as far as
/// the interface is concerned it drew the right thing.
///
/// With wrapping off, a line the terminal has no room for is cut at the edge
/// instead. That is the same thing the interface already does on purpose
/// everywhere it knows a line is too long, and it cannot corrupt anything.
fn claim_screen() -> Result<ratatui::DefaultTerminal> {
    let terminal = ratatui::try_init().context("starting the terminal interface")?;
    if execute!(io::stdout(), DisableLineWrap).is_ok() {
        // Wrapping is a setting of the user's terminal, not ours, so it goes
        // back even if we leave by panicking.
        static RESTORE_WRAP: Once = Once::new();
        RESTORE_WRAP.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let _ = execute!(io::stdout(), EnableLineWrap);
                previous(info);
            }));
        });
    }
    Ok(terminal)
}

/// Gives the screen back, as we found it.
fn release_screen() -> Result<()> {
    let _ = execute!(io::stdout(), EnableLineWrap);
    ratatui::try_restore().context("restoring the terminal")
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

        if app.any_switching() && app.busy.is_none() && Instant::now() >= app.next_tick {
            app.next_tick = Instant::now() + app.tick_interval;
            submit(&mut app, &worker, Job::Tick);
        }

        if event::poll(FRAME)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(&mut app, engine, &worker, terminal, key)?;
                }
                // The window changed shape, so everything on it was drawn for
                // a window that no longer exists. Paint all of it again rather
                // than the difference against a screen we can no longer
                // describe.
                Event::Resize(..) => terminal.clear()?,
                _ => {}
            }
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
    // And Ctrl-L repaints, for when something outside this program has written
    // over the screen.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('l')) {
        terminal.clear()?;
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
        KeyCode::Char('w') => match app.selected().map(|status| status.account.provider) {
            Some(provider) => {
                let on = !app.switching_on(provider);
                if on {
                    app.next_tick = Instant::now();
                }
                submit(app, worker, Job::SetSwitching { provider, on });
            }
            None => app.fail("Select an account to switch its agent automatically"),
        },
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
    release_screen().context("handing the terminal to the login")?;

    let result = engine.login(provider, None, false, |command| {
        println!(
            "Starting the {} login. Any running session keeps its own account.\n",
            provider.display_name()
        );
        command
            .status()
            .context("starting the login. Is the CLI installed and on PATH?")
    });

    *terminal = claim_screen().context("taking the terminal back after the login")?;
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
            fleet_home: None,
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
