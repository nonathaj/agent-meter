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
    /// It stored an account. The panel closes on its own when it sees this.
    Succeeded(String),
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
    /// in to is the one that gets stored.
    pub fn start(store: Store, provider: ProviderKind, account: Option<String>) -> Result<Self> {
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
                            engine.login(provider, None, false, |command| {
                                run_piped(command, &tx, &stdin, &cancel)
                            })
                        })
                        .map_err(|error| {
                            if cancel.load(Ordering::SeqCst) {
                                "Login cancelled. Nothing was added.".to_string()
                            } else {
                                format!("{error:#}")
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
                    self.state = State::Succeeded(match outcome {
                        AddOutcome::Added { id } => format!("Added {id}"),
                        AddOutcome::Updated { id } => format!("Signed in to {id} again"),
                    });
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
}

impl Transcript {
    fn push(&mut self, text: &str) {
        for c in strip_escapes(text).chars() {
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
        self.lines.iter().chain([&self.partial]).find_map(|line| {
            let start = line.find("https://")?;
            let rest = &line[start..];
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            Some(rest[..end].trim_end_matches(['.', ',', ')']))
        })
    }
}

/// Removes terminal escape sequences: colours, cursor movement, and the
/// hyperlink wrapper Claude Code puts round its link, which would otherwise
/// print the address twice.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // Control sequence: parameters, then one final byte.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // Operating system command: runs to a bell or to ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Any other escape is two characters long.
            _ => {}
        }
    }
    out
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

/// Puts `text` on the clipboard of the terminal the interface is drawn in.
///
/// Done with the terminal's own clipboard sequence rather than the operating
/// system's, so it lands on the machine the user is sitting at even when this
/// one is at the other end of an SSH connection.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut stdout = std::io::stdout();
    write!(stdout, "\u{1b}]52;c;{encoded}\u{7}")
        .and_then(|()| stdout.flush())
        .context("copying to the clipboard")
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
