//! Interface state and the event loop.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;

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

/// Everything the interface draws from.
pub struct App {
    pub statuses: Vec<Status>,
    pub table: TableState,
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
            table: TableState::new().with_selected(Some(0)),
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
    pub(super) fn preview(engine: &Engine) -> Result<Self> {
        Self::new(engine)
    }

    /// An interface populated with fixed data, for rendering tests.
    #[cfg(test)]
    pub(super) fn for_tests(statuses: Vec<Status>) -> Self {
        Self {
            statuses,
            table: TableState::new().with_selected(Some(0)),
            mode: Mode::Browse,
            message: None,
            busy: None,
            watching: false,
            threshold: 90.0,
            should_quit: false,
            next_tick: Instant::now(),
            tick_interval: Duration::from_secs(300),
        }
    }

    /// Re-reads accounts and cached usage from the store.
    pub fn reload(&mut self, engine: &Engine) -> Result<()> {
        self.statuses = engine.status()?;
        let selected = self.table.selected().unwrap_or(0);
        self.table.select(if self.statuses.is_empty() {
            None
        } else {
            Some(selected.min(self.statuses.len() - 1))
        });
        Ok(())
    }

    pub fn selected(&self) -> Option<&Status> {
        self.statuses.get(self.table.selected()?)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.statuses.is_empty() {
            return;
        }
        let last = self.statuses.len() - 1;
        let current = self.table.selected().unwrap_or(0) as isize;
        self.table
            .select(Some((current + delta).clamp(0, last as isize) as usize));
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
        KeyCode::Home => app.table.select(Some(0)),
        KeyCode::End => app.move_selection(app.statuses.len() as isize),
        KeyCode::Char('r') => submit(app, worker, Job::Poll { force: true }),
        KeyCode::Char('?') | KeyCode::F(1) => app.mode = Mode::Help,
        KeyCode::Char('a') => app.mode = Mode::ChooseProvider(ProviderAction::Add),
        KeyCode::Char('i') => app.mode = Mode::ChooseProvider(ProviderAction::Import),
        KeyCode::Char('w') => {
            app.watching = !app.watching;
            if app.watching {
                app.next_tick = Instant::now();
                app.note(format!("Watching: will switch at {:.0}%", app.threshold));
            } else {
                app.note("Watching off");
            }
        }
        KeyCode::Enter | KeyCode::Char('u') => match app.selected() {
            Some(status) if status.active => app.note(format!("{} is already active", status.account.id)),
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
    let _ = (engine, terminal);
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
pub(super) fn sample_statuses(count: usize) -> Vec<Status> {
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
    use crate::store::Store;

    fn app_with(count: usize) -> App {
        App::for_tests(sample_statuses(count))
    }

    #[test]
    fn selection_stays_inside_the_list() {
        let mut app = app_with(3);
        app.move_selection(-1);
        assert_eq!(app.table.selected(), Some(0));
        app.move_selection(10);
        assert_eq!(app.table.selected(), Some(2));
        assert_eq!(app.selected().unwrap().account.id, "claude-3");

        let mut empty = app_with(0);
        empty.move_selection(1);
        assert!(empty.selected().is_none());
    }

    #[test]
    fn reload_keeps_the_selection_in_range() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::with_store(Store::open(dir.path().join("data")).unwrap()).unwrap();
        let mut app = app_with(3);
        app.table.select(Some(2));
        // The store is empty, so every account disappears.
        app.reload(&engine).unwrap();
        assert_eq!(app.table.selected(), None);
    }
}
