//! Interface state and the event loop.

use std::io;
use std::sync::Once;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{DisableLineWrap, EnableLineWrap};

use super::login::{self, Login};
use super::settings::{self, Setting, Step};
use super::worker::{Job, Worker};
use crate::account::ProviderKind;
use crate::config::Config;
use crate::engine::{Engine, Status};

/// How often to redraw while idle. Short enough that countdowns tick visibly.
const FRAME: Duration = Duration::from_millis(250);
/// How often to redraw while something is animating, such as a spinner.
const ANIMATION_FRAME: Duration = Duration::from_millis(80);

/// What the interface is waiting for the user to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Browsing the account list.
    Browse,
    /// Asking which provider an action applies to, with `index` the one the
    /// cursor is on.
    ChooseProvider { action: ProviderAction, index: usize },
    /// Asking whether to remove an account.
    ConfirmRemove { id: String, name: String },
    /// Showing the key bindings.
    Help,
    /// Following a login, which `App::login` holds.
    Login,
    /// Changing the settings, with `index` the one the cursor is on.
    /// `changed` once any of them has been, so that closing the panel can
    /// have the watcher act on them.
    Settings { index: usize, changed: bool },
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
    /// Jobs asked for while the worker was busy, to run in turn after it.
    queued: std::collections::VecDeque<Job>,
    /// Which harnesses switch automatically, as the configuration says.
    pub switching: Vec<(ProviderKind, bool)>,
    /// The settings as they stood when the store was last read.
    pub config: Config,
    /// The login under way, while the login panel is up.
    pub login: Option<Login>,
    /// When the interface started, which the spinners count from.
    pub epoch: Instant,
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
            queued: std::collections::VecDeque::new(),
            switching: Vec::new(),
            config: engine.config().clone(),
            login: None,
            epoch: Instant::now(),
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
            queued: std::collections::VecDeque::new(),
            switching: ProviderKind::ALL.map(|kind| (kind, false)).to_vec(),
            config: Config::default(),
            login: None,
            epoch: Instant::now(),
            should_quit: false,
            next_tick: Instant::now(),
            tick_interval: Duration::from_secs(60),
        };
        app.rebuild(None);
        app
    }

    /// Re-reads accounts and cached usage from the store, and takes up the
    /// settings `engine` was opened with.
    ///
    /// The settings are the engine's rather than a copy read here, because the
    /// engine is what judges the accounts: how full each is, and the order
    /// they are taken in. Read separately, the screen could show one set of
    /// settings and an order worked out from another. The event loop opens the
    /// engine again whenever they may have changed.
    pub fn reload(&mut self, engine: &Engine) -> Result<()> {
        self.statuses = engine.status()?;
        self.config = engine.config().clone();
        self.switching = ProviderKind::ALL
            .map(|kind| (kind, self.config.is_switching_on(kind)))
            .to_vec();
        self.tick_interval = Duration::from_secs(self.config.watch.poll_secs);
        self.rebuild(Some(engine));
        Ok(())
    }

    /// The settings the panel lists, in the order it lists them.
    pub fn settings(&self) -> Vec<Setting> {
        settings::list(&self.config, &self.statuses)
    }

    /// Whether this harness switches accounts on its own.
    pub fn switching_on(&self, kind: ProviderKind) -> bool {
        self.switching
            .iter()
            .find(|(provider, _)| *provider == kind)
            .is_some_and(|(_, on)| *on)
    }

    /// Whether something on screen is moving, and needs drawing more often.
    pub fn animating(&self) -> bool {
        self.busy.is_some() || self.login.as_ref().is_some_and(Login::is_running)
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
///
/// Bracketed paste is turned on too, so a code pasted into the login panel
/// arrives as one piece rather than as a burst of keys, any of which might be
/// a key the panel acts on.
fn claim_screen() -> Result<ratatui::DefaultTerminal> {
    let terminal = ratatui::try_init().context("starting the terminal interface")?;
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    if execute!(io::stdout(), DisableLineWrap).is_ok() {
        // Wrapping is a setting of the user's terminal, not ours, so it goes
        // back even if we leave by panicking.
        static RESTORE_WRAP: Once = Once::new();
        RESTORE_WRAP.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let _ = execute!(io::stdout(), EnableLineWrap, DisableBracketedPaste);
                previous(info);
            }));
        });
    }
    Ok(terminal)
}

/// Gives the screen back, as we found it.
fn release_screen() -> Result<()> {
    let _ = execute!(io::stdout(), EnableLineWrap, DisableBracketedPaste);
    ratatui::try_restore().context("restoring the terminal")
}

/// Opens `engine` again, so that it judges by the settings as they now stand.
///
/// A settings file that no longer reads leaves it on the settings it last
/// read, as it leaves the watcher.
fn reopen(engine: &mut Engine) {
    if let Ok(fresh) = Engine::with_store(engine.store().clone()) {
        *engine = fresh;
    }
}

fn event_loop(engine: &Engine, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    // One of its own, opened again whenever the settings may have changed:
    // by a key here, or by `agent-meter config set` somewhere else.
    let mut engine = Engine::with_store(engine.store().clone())?;
    let engine = &mut engine;
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
            reopen(engine);
            app.reload(engine)?;
            if let Some(job) = app.queued.pop_front() {
                submit(&mut app, &worker, job);
            }
        }

        follow_login(&mut app, engine, &worker)?;

        if app.any_switching() && app.busy.is_none() && Instant::now() >= app.next_tick {
            app.next_tick = Instant::now() + app.tick_interval;
            submit(&mut app, &worker, Job::Tick);
        }

        if event::poll(if app.animating() { ANIMATION_FRAME } else { FRAME })? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(&mut app, engine, &worker, terminal, key)?;
                }
                Event::Paste(text) => paste(&mut app, &text),
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

/// Takes in what the login has done, and closes its panel once it has stored
/// an account.
///
/// A failed login keeps the panel up: the reason is only useful next to what
/// the CLI said before it gave up.
fn follow_login(app: &mut App, engine: &Engine, worker: &Worker) -> Result<()> {
    let Some(login) = app.login.as_mut() else {
        return Ok(());
    };
    login.update();
    if let login::State::Succeeded { id, note, .. } = login.state.clone() {
        app.login = None;
        app.mode = Mode::Browse;
        app.note(note);
        app.reload(engine)?;
        // Read now, whatever the interval says: a first reading is what makes
        // a new account eligible for switching, and an account signed back in
        // to is still showing the figures from before it was signed out.
        submit(app, worker, Job::Read(id));
    }
    Ok(())
}

/// Hands `job` to the worker, or queues it behind the one running.
///
/// Queued rather than dropped: a job dropped because a poll or a tick
/// happened to be running is one the user asked for and never got, with
/// nothing on screen to say so. The same job queued twice runs once.
fn submit(app: &mut App, worker: &Worker, job: Job) {
    if app.busy.is_some() {
        if !app.queued.contains(&job) {
            app.queued.push_back(job);
        }
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
    engine: &mut Engine,
    worker: &Worker,
    terminal: &mut ratatui::DefaultTerminal,
    key: KeyEvent,
) -> Result<()> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl-C quits from anywhere, as it does in every other terminal program.
    // A login under way is stopped on the way out, by dropping it.
    if control && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return Ok(());
    }
    // And Ctrl-L repaints, for when something outside this program has written
    // over the screen.
    if control && matches!(key.code, KeyCode::Char('l')) {
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
        Mode::ChooseProvider { action, index } => match choose_provider_key(index, key) {
            Choice::Move(index) => app.mode = Mode::ChooseProvider { action, index },
            Choice::Cancel => app.mode = Mode::Browse,
            Choice::Chosen(provider) => {
                app.mode = Mode::Browse;
                match action {
                    ProviderAction::Import => submit(app, worker, Job::Import(provider)),
                    ProviderAction::Add => start_login(app, engine, provider, None),
                }
            }
        },
        Mode::Login => login_key(app, key),
        Mode::Settings { index, changed } => settings_panel_key(app, engine, index, changed, key)?,
        Mode::Browse => browse_key(app, engine, worker, key),
    }
    Ok(())
}

/// Keys while the settings panel is up, its cursor on the setting at `index`.
fn settings_panel_key(
    app: &mut App,
    engine: &mut Engine,
    index: usize,
    changed: bool,
    key: KeyEvent,
) -> Result<()> {
    match settings_key(key) {
        SettingsKey::Move(by) => {
            let last = app.settings().len().saturating_sub(1) as isize;
            let index = (index as isize).saturating_add(by).clamp(0, last) as usize;
            app.mode = Mode::Settings { index, changed };
        }
        SettingsKey::Change(step) => {
            let Some(setting) = app.settings().get(index).cloned() else {
                return Ok(());
            };
            match change_setting(engine, &setting, step) {
                Ok(false) => {}
                Ok(true) => {
                    reopen(engine);
                    app.reload(engine)?;
                    // The list is drawn from the settings and the readings,
                    // and the setting changed keeps its place in it whatever
                    // else moved.
                    let index = app
                        .settings()
                        .iter()
                        .position(|listed| *listed == setting)
                        .unwrap_or(index);
                    app.mode = Mode::Settings { index, changed: true };
                    app.message = None;
                }
                Err(error) => app.fail(format!("{error:#}")),
            }
        }
        SettingsKey::Close => {
            app.mode = Mode::Browse;
            if changed {
                // Whoever changed them is waiting to see what they do, and
                // would otherwise wait out the rest of the interval. Not after
                // every step, or a threshold on its way from 90 to 60 would
                // be acted on at every mark in between.
                app.next_tick = Instant::now();
                app.note("Settings saved");
            }
        }
        SettingsKey::Nothing => {}
    }
    Ok(())
}

/// What a key does in the settings panel.
#[derive(Debug, PartialEq)]
enum SettingsKey {
    Move(isize),
    Change(Step),
    Close,
    Nothing,
}

fn settings_key(key: KeyEvent) -> SettingsKey {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => SettingsKey::Move(-1),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => SettingsKey::Move(1),
        KeyCode::Home | KeyCode::Char('g') => SettingsKey::Move(isize::MIN),
        KeyCode::End | KeyCode::Char('G') => SettingsKey::Move(isize::MAX),
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => SettingsKey::Change(Step::Down),
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') | KeyCode::Char('=') => {
            SettingsKey::Change(Step::Up)
        }
        KeyCode::Char(' ') | KeyCode::Enter => SettingsKey::Change(Step::Toggle),
        KeyCode::Backspace | KeyCode::Delete => SettingsKey::Change(Step::Clear),
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => SettingsKey::Close,
        _ => SettingsKey::Nothing,
    }
}

/// Applies `step` to `setting` in the settings file, and says whether that
/// changed it.
///
/// The file is read again for every change rather than written from the copy
/// on screen, so that a setting changed somewhere else while the panel was
/// open is not put back by a key pressed here.
fn change_setting(engine: &Engine, setting: &Setting, step: Step) -> Result<bool> {
    let store = engine.store();
    let mut config = store.config()?;
    if !setting.change(&mut config, step) {
        return Ok(false);
    }
    config.save(store.dir())?;
    Ok(true)
}

/// What a key does in the provider chooser.
#[derive(Debug, PartialEq)]
enum Choice {
    Move(usize),
    Chosen(ProviderKind),
    Cancel,
}

fn choose_provider_key(index: usize, key: KeyEvent) -> Choice {
    let count = ProviderKind::ALL.len();
    match key.code {
        KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => Choice::Move((index + count - 1) % count),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => Choice::Move((index + 1) % count),
        KeyCode::Enter => ProviderKind::ALL
            .get(index)
            .copied()
            .map_or(Choice::Cancel, Choice::Chosen),
        // The number each agent is listed under.
        KeyCode::Char(c) => c
            .to_digit(10)
            .and_then(|n| (n as usize).checked_sub(1))
            .and_then(|n| ProviderKind::ALL.get(n).copied())
            .map_or(Choice::Cancel, Choice::Chosen),
        _ => Choice::Cancel,
    }
}

fn browse_key(app: &mut App, engine: &Engine, worker: &Worker, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Home | KeyCode::Char('g') => app.move_selection(-(app.rows.len() as isize)),
        KeyCode::End | KeyCode::Char('G') => app.move_selection(app.rows.len() as isize),
        KeyCode::Char('r') => submit(app, worker, Job::Poll { force: true }),
        KeyCode::Char('p') | KeyCode::Tab => app.cycle_filter(Some(engine)),
        KeyCode::Char('?') | KeyCode::F(1) => app.mode = Mode::Help,
        KeyCode::Char('s') => {
            app.mode = Mode::Settings {
                index: 0,
                changed: false,
            }
        }
        KeyCode::Char('a') => {
            app.mode = Mode::ChooseProvider {
                action: ProviderAction::Add,
                index: 0,
            }
        }
        KeyCode::Char('i') => {
            app.mode = Mode::ChooseProvider {
                action: ProviderAction::Import,
                index: 0,
            }
        }
        KeyCode::Char('l') => match app.selected() {
            Some(status) => {
                let provider = status.account.provider;
                let name = status.account.display_name().to_string();
                start_login(app, engine, provider, Some(name));
            }
            None => app.fail("Select an account to sign in to it again"),
        },
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
}

/// Starts a login and puts its panel up. The interface keeps running
/// throughout: the CLI's output is shown in the panel, not handed the
/// terminal.
fn start_login(app: &mut App, engine: &Engine, provider: ProviderKind, account: Option<String>) {
    match Login::start(engine.store().clone(), provider, account, None) {
        Ok(login) => {
            app.login = Some(login);
            app.mode = Mode::Login;
        }
        Err(error) => app.fail(format!("{error:#}")),
    }
}

/// What a key does in the login panel, while the login is still running.
#[derive(Debug, PartialEq)]
enum LoginKey {
    Cancel,
    Open,
    Copy,
    Submit,
    Erase,
    Type(char),
    Nothing,
}

/// While the CLI waits for a code every printable key is typing, so the link
/// actions are on Ctrl; with no code wanted, the plain letters work too.
/// `c` copies as it does in Claude Code's own login: with nothing typed yet,
/// even while a code is wanted, since a code is pasted rather than typed and a
/// paste does not arrive as keys.
fn login_key_action(key: KeyEvent, typing: bool, typed_nothing: bool) -> LoginKey {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => LoginKey::Cancel,
        KeyCode::Char('o') if control || !typing => LoginKey::Open,
        KeyCode::Char('c') if !control && (!typing || typed_nothing) => LoginKey::Copy,
        KeyCode::Char('y') if control || !typing => LoginKey::Copy,
        KeyCode::Enter if typing => LoginKey::Submit,
        KeyCode::Backspace if typing => LoginKey::Erase,
        KeyCode::Char(c) if typing && !control => LoginKey::Type(c),
        _ => LoginKey::Nothing,
    }
}

/// Keys while the login panel is up.
fn login_key(app: &mut App, key: KeyEvent) {
    let Some(login) = app.login.as_mut() else {
        app.mode = Mode::Browse;
        return;
    };

    // Over and failed: the reason moves to the status line as it closes.
    if !login.is_running() && matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
        if let login::State::Failed(error) = login.state.clone() {
            app.fail(error);
        }
        app.login = None;
        app.mode = Mode::Browse;
        return;
    }

    let typing = login.is_running() && login.wants_input();
    let result: Result<Option<&str>> = match login_key_action(key, typing, login.input.is_empty()) {
        LoginKey::Cancel => {
            login.cancel();
            Ok(None)
        }
        LoginKey::Open => match login.link() {
            Some(link) => {
                login::open_in_browser(link).map(|()| Some("Opened the sign-in page in your browser"))
            }
            None => Ok(None),
        },
        LoginKey::Copy => match login.link() {
            Some(link) => login::copy_to_clipboard(link)
                .map(|copied| {
                    Some(match copied {
                        login::Copied::System => "Copied the sign-in link",
                        login::Copied::Terminal => {
                            "Asked your terminal to copy the link; if it can't, select it above"
                        }
                    })
                })
                .context("Couldn't copy the sign-in link; select it above instead"),
            None => Ok(Some("No sign-in link to copy yet")),
        },
        LoginKey::Submit => login.submit_input().map(|()| None),
        LoginKey::Erase => {
            login.input.pop();
            Ok(None)
        }
        LoginKey::Type(c) => {
            login.input.push(c);
            Ok(None)
        }
        LoginKey::Nothing => Ok(None),
    };
    match result {
        Ok(Some(note)) => app.note(note),
        Ok(None) => {}
        Err(error) => app.fail(format!("{error:#}")),
    }
}

/// A paste lands in the login panel's code field, and nowhere else: pasted
/// text anywhere in the list would be read as a string of commands.
fn paste(app: &mut App, text: &str) {
    if let Some(login) = app.login.as_mut()
        && login.is_running()
        && login.wants_input()
    {
        login.input.extend(text.chars().filter(|c| !c.is_whitespace()));
    }
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

    /// The chooser takes arrows and enter, or the number an agent is listed
    /// under; anything else backs out without doing anything.
    #[test]
    fn an_agent_is_chosen_by_arrows_or_by_number() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let last = ProviderKind::ALL.len() - 1;

        assert_eq!(choose_provider_key(0, key(KeyCode::Down)), Choice::Move(1));
        // Round from either end.
        assert_eq!(choose_provider_key(0, key(KeyCode::Up)), Choice::Move(last));
        assert_eq!(choose_provider_key(last, key(KeyCode::Down)), Choice::Move(0));

        assert_eq!(
            choose_provider_key(1, key(KeyCode::Enter)),
            Choice::Chosen(ProviderKind::ALL[1])
        );
        assert_eq!(
            choose_provider_key(1, key(KeyCode::Char('1'))),
            Choice::Chosen(ProviderKind::ALL[0])
        );

        assert_eq!(choose_provider_key(0, key(KeyCode::Char('9'))), Choice::Cancel);
        assert_eq!(choose_provider_key(0, key(KeyCode::Char('0'))), Choice::Cancel);
        assert_eq!(choose_provider_key(0, key(KeyCode::Esc)), Choice::Cancel);
    }

    /// `c` copies the link, as in Claude Code's own login, without taking a
    /// letter from a code being typed; Ctrl keeps the link actions within
    /// reach once one is.
    #[test]
    fn c_copies_the_link_unless_a_code_is_being_typed() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
        let c = KeyCode::Char('c');

        // No code wanted, or wanted and nothing typed yet.
        assert_eq!(login_key_action(key(c), false, true), LoginKey::Copy);
        assert_eq!(login_key_action(key(c), true, true), LoginKey::Copy);
        // Part-way through typing one, `c` is part of it.
        assert_eq!(login_key_action(key(c), true, false), LoginKey::Type('c'));

        // The other ways to copy, as before.
        assert_eq!(
            login_key_action(key(KeyCode::Char('y')), false, true),
            LoginKey::Copy
        );
        assert_eq!(
            login_key_action(ctrl(KeyCode::Char('y')), true, false),
            LoginKey::Copy
        );
        assert_eq!(
            login_key_action(key(KeyCode::Char('y')), true, true),
            LoginKey::Type('y')
        );

        // The keys `c` sits beside are unchanged.
        assert_eq!(
            login_key_action(key(KeyCode::Char('o')), false, true),
            LoginKey::Open
        );
        assert_eq!(
            login_key_action(key(KeyCode::Char('o')), true, true),
            LoginKey::Type('o')
        );
        assert_eq!(login_key_action(key(KeyCode::Esc), true, false), LoginKey::Cancel);
        assert_eq!(
            login_key_action(key(KeyCode::Enter), true, false),
            LoginKey::Submit
        );
    }

    /// A job asked for while another runs is one the user wanted: after a
    /// login, the read of the account signed in to was being thrown away
    /// whenever a poll or a tick happened to be running.
    #[test]
    fn a_job_asked_for_while_busy_waits_its_turn_once() {
        let dir = tempfile::tempdir().unwrap();
        let worker = Worker::spawn(Store::open(dir.path().join("data")).unwrap()).unwrap();
        let mut app = app_with(1);
        app.busy = Some("Checking whether to switch…".into());

        submit(&mut app, &worker, Job::Read("claude-1".into()));
        submit(&mut app, &worker, Job::Read("claude-1".into()));
        submit(&mut app, &worker, Job::Poll { force: true });
        assert_eq!(
            Vec::from(app.queued.clone()),
            [Job::Read("claude-1".into()), Job::Poll { force: true }]
        );
    }

    #[test]
    fn the_settings_panel_has_a_key_for_every_way_to_change_a_setting() {
        let key = |code| settings_key(KeyEvent::new(code, KeyModifiers::NONE));
        for (code, does) in [
            (KeyCode::Up, SettingsKey::Move(-1)),
            (KeyCode::Char('j'), SettingsKey::Move(1)),
            (KeyCode::Left, SettingsKey::Change(Step::Down)),
            (KeyCode::Char('+'), SettingsKey::Change(Step::Up)),
            (KeyCode::Char(' '), SettingsKey::Change(Step::Toggle)),
            (KeyCode::Enter, SettingsKey::Change(Step::Toggle)),
            (KeyCode::Delete, SettingsKey::Change(Step::Clear)),
            (KeyCode::Esc, SettingsKey::Close),
            // The key that opened it closes it.
            (KeyCode::Char('s'), SettingsKey::Close),
            (KeyCode::Char('x'), SettingsKey::Nothing),
        ] {
            assert_eq!(key(code), does, "{code:?}");
        }
    }

    /// The panel as somebody uses it: `s` opens it, the arrows move through
    /// it and change what the cursor is on, and closing it says they are
    /// saved and has them acted on.
    #[test]
    fn the_settings_panel_is_opened_changed_and_closed_from_the_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        let mut engine = Engine::with_store(store.clone()).unwrap();
        let worker = Worker::spawn(store.clone()).unwrap();
        let mut app = app_with(0);
        app.reload(&engine).unwrap();
        app.next_tick = Instant::now() + Duration::from_secs(3600);
        let press = |app: &mut App, engine: &mut Engine, code| {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            match app.mode.clone() {
                Mode::Settings { index, changed } => {
                    settings_panel_key(app, engine, index, changed, key).unwrap();
                }
                _ => browse_key(app, engine, &worker, key),
            }
        };

        press(&mut app, &mut engine, KeyCode::Char('s'));
        assert_eq!(
            app.mode,
            Mode::Settings {
                index: 0,
                changed: false
            }
        );
        // Looking is not changing: closing it again says nothing.
        press(&mut app, &mut engine, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.message, None);
        assert!(app.next_tick > Instant::now());

        press(&mut app, &mut engine, KeyCode::Char('s'));
        press(&mut app, &mut engine, KeyCode::Left);
        press(&mut app, &mut engine, KeyCode::Left);
        assert_eq!(store.config().unwrap().watch.threshold, 88.0);
        assert_eq!(app.config.watch.threshold, 88.0, "and it is what is on screen");
        // The cursor stops at either end of the list.
        press(&mut app, &mut engine, KeyCode::Up);
        press(&mut app, &mut engine, KeyCode::End);
        press(&mut app, &mut engine, KeyCode::Down);
        assert_eq!(
            app.mode,
            Mode::Settings {
                index: 4,
                changed: true
            }
        );
        // A key that means nothing to the setting it is on changes nothing.
        press(&mut app, &mut engine, KeyCode::Delete);
        press(&mut app, &mut engine, KeyCode::Char(' '));
        let written = store.config().unwrap();
        assert!(!written.watch.wait_for_reset);
        assert_eq!(written.watch.threshold, 88.0);

        press(&mut app, &mut engine, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.message.as_ref().unwrap().text, "Settings saved");
        assert!(app.next_tick <= Instant::now(), "a check is due at once");
    }

    /// A key in the panel changes the settings file, and the interface then
    /// judges by what it says: the watcher reads the same file, so the two
    /// cannot disagree.
    #[test]
    fn a_setting_changed_in_the_panel_is_written_and_taken_up() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = Engine::with_store(Store::open(dir.path().join("data")).unwrap()).unwrap();
        let mut app = app_with(1);
        app.reload(&engine).unwrap();
        let claude = ProviderKind::Claude;
        let fable = Setting::Model(claude, "Fable".into());

        assert!(change_setting(&engine, &fable, Step::Toggle).unwrap());
        assert!(change_setting(&engine, &Setting::Threshold, Step::Down).unwrap());
        assert!(change_setting(&engine, &Setting::Poll, Step::Up).unwrap());
        let written = engine.store().config().unwrap();
        assert_eq!(written.models_for(claude), ["Fable"]);
        assert_eq!(written.watch.threshold, 89.0);

        // Nothing here has taken it up until the engine is opened again.
        assert_eq!(app.config.watch.threshold, 90.0);
        reopen(&mut engine);
        app.reload(&engine).unwrap();
        assert_eq!(app.config, written);
        assert_eq!(engine.rules_for(claude).threshold, 89.0);
        assert_eq!(app.tick_interval, Duration::from_secs(120));

        // A step that changes nothing writes nothing.
        assert!(!change_setting(&engine, &Setting::Poll, Step::Clear).unwrap());

        // A setting changed elsewhere while the panel is open is kept: each
        // key changes the file as it stands, not a copy from when it opened.
        let mut elsewhere = engine.store().config().unwrap();
        elsewhere.set("watch.margin", "12").unwrap();
        elsewhere.save(engine.store().dir()).unwrap();
        assert!(change_setting(&engine, &Setting::Threshold, Step::Down).unwrap());
        let written = engine.store().config().unwrap();
        assert_eq!((written.watch.margin, written.watch.threshold), (12.0, 88.0));

        // A file that no longer reads is not written over, and the engine
        // stays on the settings it last read.
        std::fs::write(
            Config::path(engine.store().dir()),
            "[watch]\nthreshold = \"high\"\n",
        )
        .unwrap();
        assert!(change_setting(&engine, &Setting::Threshold, Step::Down).is_err());
        reopen(&mut engine);
        assert_eq!(engine.rules_for(claude).threshold, 89.0);
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
