//! A login that runs inside the interface rather than instead of it.
//!
//! The agent CLI's login is run with its input and output piped to us, so the
//! interface keeps the screen: what the CLI prints is shown in a panel, the
//! link it asks the user to open can be opened or copied from there, and a
//! code the sign-in page hands back can be typed or pasted into the panel and
//! passed on. Nothing the CLI writes can reach the terminal directly, so no
//! redraw can scribble over it and it cannot scribble over the interface.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::account::ProviderKind;
use crate::engine::{AddOutcome, Engine};
use crate::store::Store;

/// How often the login thread checks whether the CLI has finished or the user
/// has cancelled.
const WAIT_STEP: Duration = Duration::from_millis(50);

/// How much of the CLI's output is kept. A login prints a dozen lines; this
/// only bounds a CLI that goes wrong and prints forever.
const MAX_LINES: usize = 200;

/// Something the login thread reports.
enum Event {
    /// Text the CLI printed, in the order it printed it.
    Output(String),
    /// The login is over, one way or the other.
    Done(Result<AddOutcome, String>),
}

/// Where a login has got to.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Running,
    /// It stored the account `id`, as `note` says: `added` when it is new
    /// here, rather than one already stored signing in again. The panel
    /// closes on its own when it sees this.
    Succeeded {
        id: String,
        note: String,
        added: bool,
    },
    /// It failed, or was cancelled; the panel stays up so the reason can be
    /// read alongside what the CLI said.
    Failed(String),
}

/// A login in progress, and everything the panel draws from.
pub struct Login {
    pub provider: ProviderKind,
    /// The account being signed in to again, when it is one we already hold.
    pub account: Option<String>,
    pub state: State,
    /// What the user has typed for the CLI to read.
    pub input: String,
    pub started: Instant,
    transcript: Transcript,
    events: Receiver<Event>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    cancel: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Login {
    /// Starts logging in to `provider` on a thread of its own.
    ///
    /// `account` names the stored account this is expected to sign back in
    /// to; it is only shown, since whichever account the user actually signs
    /// in to is the one that gets stored. `label` names the account stored.
    pub fn start(
        store: Store,
        provider: ProviderKind,
        account: Option<String>,
        label: Option<String>,
    ) -> Result<Self> {
        let (tx, events) = channel();
        let stdin = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));

        let handle = {
            let stdin = stdin.clone();
            let cancel = cancel.clone();
            thread::Builder::new()
                .name("agent-meter-login".into())
                .spawn(move || {
                    let outcome = Engine::with_store(store)
                        .and_then(|engine| {
                            engine.login(provider, label, false, |command| {
                                run_piped(command, &tx, &stdin, &cancel)
                            })
                        })
                        .map_err(|error| {
                            if cancel.load(Ordering::SeqCst) {
                                "Login cancelled. Nothing was added.".to_string()
                            } else {
                                // A program may be passing this on.
                                crate::config::said_elsewhere(&error)
                            }
                        });
                    let _ = tx.send(Event::Done(outcome));
                })
                .context("starting the login")?
        };

        Ok(Self {
            provider,
            account,
            state: State::Running,
            input: String::new(),
            started: Instant::now(),
            transcript: Transcript::default(),
            events,
            stdin,
            cancel,
            handle: Some(handle),
        })
    }

    /// Takes in whatever the login thread has reported since the last frame.
    pub fn update(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(Event::Output(text)) => self.transcript.push(&text),
                Ok(Event::Done(Ok(outcome))) => {
                    self.state = match outcome {
                        AddOutcome::Added { id } => State::Succeeded {
                            note: format!("Added {id}"),
                            id,
                            added: true,
                        },
                        AddOutcome::Updated { id } => State::Succeeded {
                            note: format!("Signed in to {id} again"),
                            id,
                            added: false,
                        },
                    };
                }
                Ok(Event::Done(Err(error))) => self.state = State::Failed(error),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.state == State::Running {
                        self.state = State::Failed("the login stopped unexpectedly".into());
                    }
                    break;
                }
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.state == State::Running
    }

    /// Stops the CLI. The login thread then cleans up its throwaway home and
    /// reports the cancellation.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Sends what has been typed to the CLI, as a line.
    pub fn submit_input(&mut self) -> Result<()> {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return Ok(());
        }
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pipe) = stdin.as_mut() else {
            bail!("the login is not reading input yet");
        };
        pipe.write_all(format!("{line}\n").as_bytes())
            .and_then(|()| pipe.flush())
            .context("passing the code to the login")?;
        self.input.clear();
        self.transcript.push("\n");
        Ok(())
    }

    /// Lines the CLI printed, with the line it is still writing last.
    pub fn lines(&self) -> Vec<&str> {
        self.transcript.lines()
    }

    /// The page the CLI wants opened, once it has said which.
    pub fn link(&self) -> Option<&str> {
        self.transcript.link()
    }

    /// [`Self::link`], once the whole of it has arrived.
    pub fn settled_link(&self) -> Option<&str> {
        self.transcript.settled_link()
    }

    /// Whether the CLI has asked for something to be pasted back to it.
    ///
    /// Read from what it printed rather than assumed per agent, so the field
    /// appears exactly when the CLI is asking — Claude Code asks for a code
    /// when its browser callback cannot reach it, Codex never does.
    pub fn wants_input(&self) -> bool {
        self.transcript
            .lines()
            .iter()
            .any(|line| line.to_ascii_lowercase().contains("paste"))
    }
}

impl Drop for Login {
    /// Leaving the interface mid-login stops the CLI and waits for the thread,
    /// so the throwaway home and the credential in it are always removed.
    fn drop(&mut self) {
        self.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
impl Login {
    /// A login still waiting for the user, that has printed `output`, for
    /// drawing without a CLI behind it.
    pub fn printed(provider: ProviderKind, output: &str) -> Self {
        let (_, events) = channel();
        let mut transcript = Transcript::default();
        transcript.push(output);
        Self {
            provider,
            account: None,
            state: State::Running,
            input: String::new(),
            started: Instant::now(),
            transcript,
            events,
            stdin: Arc::new(Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

/// Runs the login CLI with every stream piped to us, until it exits or the
/// user cancels.
fn run_piped(
    command: &mut Command,
    events: &Sender<Event>,
    stdin: &Mutex<Option<ChildStdin>>,
    cancel: &AtomicBool,
) -> Result<ExitStatus> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a cancel reaches whatever it started too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);

    let mut child = command
        .spawn()
        .context("starting the login. Is the CLI installed and on PATH?")?;
    *stdin.lock().unwrap_or_else(|e| e.into_inner()) = child.stdin.take();
    if let Some(stdout) = child.stdout.take() {
        forward(stdout, events.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward(stderr, events.clone());
    }

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.load(Ordering::SeqCst) {
            kill_tree(&mut child);
            let _ = child.wait();
            bail!("cancelled");
        }
        thread::sleep(WAIT_STEP);
    };
    // Closing our end of its input, now that nothing will read it.
    *stdin.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(status)
}

/// Passes on what a stream prints as it prints it, not a line at a time: a
/// prompt does not end its line, and it has to be on screen while the CLI
/// waits for the answer.
fn forward(mut stream: impl Read + Send + 'static, events: Sender<Event>) {
    let _ = thread::Builder::new()
        .name("agent-meter-login-output".into())
        .spawn(move || {
            let mut buffer = [0u8; 4096];
            // A multi-byte character can straddle two reads, so bytes are held
            // until they decode.
            let mut pending = Vec::new();
            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                pending.extend_from_slice(&buffer[..read]);
                let complete = match std::str::from_utf8(&pending) {
                    Ok(_) => pending.len(),
                    Err(error) if error.error_len().is_none() => error.valid_up_to(),
                    Err(_) => pending.len(),
                };
                let text = String::from_utf8_lossy(&pending[..complete]).into_owned();
                pending.drain(..complete);
                if events.send(Event::Output(text)).is_err() {
                    break;
                }
            }
        });
}

/// Stops the CLI and everything it started.
///
/// On Windows the CLI is often a batch shim that starts Node that starts the
/// real binary; killing the shim alone leaves the binary holding the port its
/// browser callback listens on, and the next login cannot start.
fn kill_tree(child: &mut Child) {
    let pid = child.id().to_string();
    let killed = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    } else {
        Command::new("kill")
            .args(["-TERM", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    if !killed.is_ok_and(|status| status.success()) {
        let _ = child.kill();
    }
}

/// What the CLI has printed, as plain text.
#[derive(Debug, Default)]
struct Transcript {
    lines: Vec<String>,
    /// The line still being written, which a prompt leaves unfinished.
    partial: String,
    /// A carriage return that has not yet been followed by anything. Held,
    /// because it means two different things: before a newline it is just how
    /// Windows ends a line, and before anything else it rewrites the line, as
    /// a spinner does. The two halves of `\r\n` can arrive in separate reads.
    carriage_return: bool,
    /// Where the escape sequence being read has got to. A sequence can arrive
    /// split across reads too, and the hyperlink wrapper round Claude Code's
    /// link holds the whole address: were its second half taken for text, the
    /// address would be printed where it should not be, or doubled.
    escape: Escape,
}

impl Transcript {
    fn push(&mut self, text: &str) {
        for c in text.chars().filter(|&c| self.escape.is_text(c)) {
            if std::mem::take(&mut self.carriage_return) && c != '\n' {
                self.partial.clear();
            }
            match c {
                '\n' => {
                    let line = std::mem::take(&mut self.partial);
                    self.lines.push(line.trim_end().to_string());
                }
                '\r' => self.carriage_return = true,
                c if c.is_control() && c != '\t' => {}
                c => self.partial.push(c),
            }
        }
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..self.lines.len() - MAX_LINES);
        }
    }

    fn lines(&self) -> Vec<&str> {
        let mut lines: Vec<&str> = self.lines.iter().map(String::as_str).collect();
        if !self.partial.trim().is_empty() {
            lines.push(self.partial.trim_end());
        }
        lines
    }

    /// The first web page the CLI named. Codex names its own local callback
    /// server before the sign-in page, which is plain http and not for people.
    fn link(&self) -> Option<&str> {
        self.lines
            .iter()
            .chain([&self.partial])
            .find_map(|line| link_in(line).map(|(link, _)| link))
    }

    /// [`Self::link`], once nothing more can be added to it: in a line that
    /// has ended, or followed by a space. Output is taken as it arrives, so
    /// an address can be cut off at the end of one read, and a program that
    /// is told the link once must be told all of it.
    fn settled_link(&self) -> Option<&str> {
        self.lines
            .iter()
            .find_map(|line| link_in(line).map(|(link, _)| link))
            .or_else(|| {
                link_in(&self.partial)
                    .filter(|(_, ended)| *ended)
                    .map(|(link, _)| link)
            })
    }
}

/// The first web page `line` names, and whether anything follows it.
fn link_in(line: &str) -> Option<(&str, bool)> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace);
    let link = rest[..end.unwrap_or(rest.len())].trim_end_matches(['.', ',', ')']);
    Some((link, end.is_some()))
}

/// How far into a terminal escape sequence the text read so far is.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum Escape {
    /// Not in one: this is text.
    #[default]
    None,
    /// Just after the escape character.
    Started,
    /// A control sequence: parameters, then one final byte.
    Control,
    /// An operating system command, which runs to a bell or to ESC \.
    Command,
    /// An escape character inside a command, which may be ending it.
    CommandEnding,
}

impl Escape {
    /// Takes the next character, and says whether it is text rather than
    /// part of a sequence.
    fn is_text(&mut self, c: char) -> bool {
        const ESC: char = '\u{1b}';
        const BEL: char = '\u{7}';
        *self = match (*self, c) {
            (Self::None, ESC) => Self::Started,
            (Self::None, _) => return true,
            (Self::Started, '[') => Self::Control,
            (Self::Started, ']') => Self::Command,
            // Any other escape is two characters long.
            (Self::Started, _) => Self::None,
            (Self::Control, '\u{40}'..='\u{7e}') => Self::None,
            (Self::Control, _) => Self::Control,
            (Self::Command | Self::CommandEnding, BEL) => Self::None,
            (Self::Command | Self::CommandEnding, ESC) => Self::CommandEnding,
            (Self::CommandEnding, '\\') => Self::None,
            (Self::Command | Self::CommandEnding, _) => Self::Command,
        };
        false
    }
}

/// Removes terminal escape sequences: colours, cursor movement, and the
/// hyperlink wrapper Claude Code puts round its link, which would otherwise
/// print the address twice.
pub(super) fn strip_escapes(text: &str) -> String {
    let mut escape = Escape::None;
    text.chars().filter(|&c| escape.is_text(c)).collect()
}

/// Opens `url` in the user's browser.
pub fn open_in_browser(url: &str) -> Result<()> {
    let mut command = if cfg!(windows) {
        // `start` would need the URL quoted for cmd, and its `&`s escaped;
        // this takes it as one argument, untouched.
        let mut command = Command::new("rundll32");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(url);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("opening the browser")?;
    Ok(())
}

/// Where a copy went.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Copied {
    /// The operating system's clipboard took it, and said so.
    System,
    /// Only handed to the terminal, to put on the clipboard of the machine it
    /// runs on. Terminals that do not allow it ignore it without a word, so
    /// whether it arrived cannot be known.
    Terminal,
}

/// How long a clipboard program may take before it is given up on. The
/// interface waits for it, so this is short.
const CLIPBOARD_WAIT: Duration = Duration::from_secs(2);

/// Puts `text` on the clipboard of the machine the user is sitting at.
///
/// The terminal is always asked, with its own clipboard sequence (OSC 52):
/// that is the one way to reach the user's clipboard when this machine is at
/// the other end of an SSH connection. On this machine's own screen its
/// clipboard program is run as well, since that one says whether it worked.
pub fn copy_to_clipboard(text: &str) -> Result<Copied> {
    let mut stdout = std::io::stdout();
    let terminal = write!(stdout, "{}", clipboard_sequence(text))
        .and_then(|()| stdout.flush())
        .context("copying to the clipboard");
    let programs = clipboard_programs(std::env::consts::OS, |name| std::env::var_os(name));
    if programs
        .into_iter()
        .any(|(program, args)| pipe_to(program, args, text).is_ok())
    {
        return Ok(Copied::System);
    }
    terminal.map(|()| Copied::Terminal)
}

/// The sequence that asks a terminal to put `text` on its clipboard.
fn clipboard_sequence(text: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\u{1b}]52;c;{encoded}\u{7}")
}

/// The programs that may put text on this machine's clipboard, most likely
/// first, on operating system `os` with environment `env`.
///
/// None over SSH: this machine's clipboard is not the one the user can paste
/// from.
fn clipboard_programs(
    os: &str,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<(&'static str, &'static [&'static str])> {
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    if ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"].into_iter().any(set) {
        return Vec::new();
    }
    match os {
        "windows" => vec![("clip", &[])],
        "macos" => vec![("pbcopy", &[])],
        _ => {
            let mut programs: Vec<(&'static str, &'static [&'static str])> = Vec::new();
            if set("WAYLAND_DISPLAY") {
                programs.push(("wl-copy", &[]));
            }
            if set("DISPLAY") {
                programs.push(("xclip", &["-selection", "clipboard"]));
                programs.push(("xsel", &["--clipboard", "--input"]));
            }
            // Under WSL, the Windows clipboard.
            programs.push(("clip.exe", &[]));
            programs
        }
    }
}

/// Runs `program` with `text` as its input, and says whether it succeeded.
fn pipe_to(program: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Waited for even when it would not take the text, so it is not left
    // behind; its input is closed first, so it knows the text has ended.
    let written = child
        .stdin
        .take()
        .map_or(Ok(()), |mut stdin| stdin.write_all(text.as_bytes()));
    let deadline = Instant::now() + CLIPBOARD_WAIT;
    loop {
        if let Some(status) = child.try_wait()? {
            written?;
            if !status.success() {
                bail!("{program} exited with {status}");
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} did not finish");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Claude Code printed to a pipe, escapes and all.
    const CLAUDE: &str = "Opening browser to sign in…\nIf the browser didn't open, visit: \
                          \u{1b}]8;;https://claude.com/cai/oauth/authorize?code=true&state=x\u{7}\
                          https://claude.com/cai/oauth/authorize?code=true&state=x\u{1b}]8;;\u{7}\n\
                          Paste code here if prompted > ";

    #[test]
    fn a_hyperlinked_address_is_printed_once() {
        let mut transcript = Transcript::default();
        transcript.push(CLAUDE);
        assert_eq!(
            transcript.lines(),
            [
                "Opening browser to sign in…",
                "If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&state=x",
                "Paste code here if prompted >",
            ]
        );
        assert_eq!(
            transcript.link(),
            Some("https://claude.com/cai/oauth/authorize?code=true&state=x")
        );
    }

    /// Wherever a read ends, the settled link is all of the address or
    /// nothing yet — never the part of it that has arrived.
    #[test]
    fn a_settled_link_is_never_cut_short() {
        let full = "https://claude.com/cai/oauth/authorize?code=true&state=x";
        let chars: Vec<char> = CLAUDE.chars().collect();
        for at in 0..=chars.len() {
            let mut transcript = Transcript::default();
            transcript.push(&chars[..at].iter().collect::<String>());
            let settled = transcript.settled_link();
            assert!(
                settled.is_none() || settled == Some(full),
                "split at {at}: {settled:?}"
            );
            transcript.push(&chars[at..].iter().collect::<String>());
            assert_eq!(transcript.settled_link(), Some(full), "split at {at}");
        }

        // An address on an unfinished line is whole once something follows it.
        let mut transcript = Transcript::default();
        transcript.push("Open https://example.com/sign-in");
        assert_eq!(transcript.settled_link(), None);
        transcript.push(" and paste the code > ");
        assert_eq!(transcript.settled_link(), Some("https://example.com/sign-in"));
    }

    /// Codex names its local callback server first; that is not the page to
    /// open.
    #[test]
    fn the_link_is_the_sign_in_page_not_the_local_callback() {
        let mut transcript = Transcript::default();
        transcript.push(
            "Starting local login server on http://localhost:1455.\n\
             If your browser did not open, navigate to this URL to authenticate:\n\n\
             https://auth.openai.com/oauth/authorize?response_type=code&state=y\n",
        );
        assert_eq!(
            transcript.link(),
            Some("https://auth.openai.com/oauth/authorize?response_type=code&state=y")
        );
    }

    /// A read can end anywhere, including inside the hyperlink wrapper that
    /// carries the whole address; wherever it ends, the text and the link
    /// come out the same.
    #[test]
    fn output_split_anywhere_reads_the_same() {
        let mut whole = Transcript::default();
        whole.push(CLAUDE);
        let chars: Vec<char> = CLAUDE.chars().collect();
        for at in 0..=chars.len() {
            let mut split = Transcript::default();
            split.push(&chars[..at].iter().collect::<String>());
            split.push(&chars[at..].iter().collect::<String>());
            assert_eq!(split.lines(), whole.lines(), "split at {at}");
            assert_eq!(split.link(), whole.link(), "split at {at}");
        }
    }

    #[test]
    fn output_split_across_reads_joins_up() {
        let mut transcript = Transcript::default();
        transcript.push("\u{1b}[32mWaiting");
        transcript.push(" for you\u{1b}[0m\r\nDone\r");
        // The other half of a Windows line ending, in the next read.
        transcript.push("\n");
        assert_eq!(transcript.lines(), ["Waiting for you", "Done"]);

        // A carriage return on its own rewrites the line, as a spinner does.
        transcript.push("⠋ working\r⠙ working\rfinished\n");
        assert_eq!(transcript.lines().last(), Some(&"finished"));
    }

    #[test]
    fn the_terminal_is_asked_to_copy_the_text_whole() {
        assert_eq!(
            clipboard_sequence("https://example.com/?a=1&b=2"),
            "\u{1b}]52;c;aHR0cHM6Ly9leGFtcGxlLmNvbS8/YT0xJmI9Mg==\u{7}"
        );
    }

    /// On this machine's own screen its clipboard program is tried; over SSH
    /// that clipboard is on the wrong machine, so only the terminal is asked.
    #[test]
    fn the_clipboard_program_suits_the_machine() {
        let env = |vars: &'static [&'static str]| move |name: &str| vars.contains(&name).then(|| "1".into());
        let names = |programs: Vec<(&'static str, &'static [&'static str])>| -> Vec<&'static str> {
            programs.into_iter().map(|(program, _)| program).collect()
        };

        assert_eq!(names(clipboard_programs("macos", env(&[]))), ["pbcopy"]);
        assert_eq!(names(clipboard_programs("windows", env(&[]))), ["clip"]);
        assert_eq!(
            names(clipboard_programs("linux", env(&["WAYLAND_DISPLAY", "DISPLAY"]))),
            ["wl-copy", "xclip", "xsel", "clip.exe"]
        );
        // No display at all: only the Windows clipboard, should this be WSL.
        assert_eq!(names(clipboard_programs("linux", env(&[]))), ["clip.exe"]);

        for os in ["linux", "macos", "windows"] {
            for ssh in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
                let over_ssh = move |name: &str| [ssh, "DISPLAY"].contains(&name).then(|| "1".into());
                assert!(clipboard_programs(os, over_ssh).is_empty(), "{os} {ssh}");
            }
        }
    }

    #[test]
    fn escapes_are_removed_and_text_is_kept() {
        assert_eq!(strip_escapes("\u{1b}[1;31mred\u{1b}[0m"), "red");
        assert_eq!(strip_escapes("a\u{1b}]0;title\u{1b}\\b"), "ab");
        assert_eq!(strip_escapes("plain — text"), "plain — text");
    }

    /// The whole path the interface takes: a CLI that prints a prompt, reads
    /// a line, and exits. It stands in for the agent CLI, which cannot be run
    /// in a test.
    #[test]
    fn a_piped_command_shows_its_prompt_and_reads_the_answer() {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("powershell");
            command.args([
                "-NoProfile",
                "-Command",
                "[Console]::Out.Write('Paste code here > '); $code = [Console]::In.ReadLine(); \
                 [Console]::Out.WriteLine(''); [Console]::Out.WriteLine('got ' + $code)",
            ]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args([
                "-c",
                "printf 'Paste code here > '; read code; echo; echo \"got $code\"",
            ]);
            command
        };

        let (tx, rx) = channel();
        let stdin = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let runner = {
            let stdin = stdin.clone();
            let cancel = cancel.clone();
            thread::spawn(move || run_piped(&mut command, &tx, &stdin, &cancel))
        };

        let mut transcript = Transcript::default();
        let wait_for = |transcript: &mut Transcript, what: &str| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !transcript.lines().iter().any(|line| line.contains(what)) {
                assert!(
                    Instant::now() < deadline,
                    "never saw {what:?}: {:?}",
                    transcript.lines()
                );
                if let Ok(Event::Output(text)) = rx.recv_timeout(Duration::from_millis(50)) {
                    transcript.push(&text);
                }
            }
        };

        wait_for(&mut transcript, "Paste code here >");
        // The prompt is on screen before its line ends, which is when it
        // matters.
        let deadline = Instant::now() + Duration::from_secs(30);
        while stdin.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        stdin
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .write_all(b"abc123\n")
            .unwrap();

        wait_for(&mut transcript, "got abc123");
        assert!(runner.join().unwrap().unwrap().success());
    }

    #[test]
    fn a_cancelled_command_is_stopped() {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("powershell");
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
            command
        } else {
            let mut command = Command::new("sleep");
            command.arg("60");
            command
        };
        let (tx, _rx) = channel();
        let stdin = Mutex::new(None);
        let cancel = AtomicBool::new(true);

        let started = Instant::now();
        let error = run_piped(&mut command, &tx, &stdin, &cancel).unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        assert!(started.elapsed() < Duration::from_secs(30));
    }
}
