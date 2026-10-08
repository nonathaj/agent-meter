//! A login another program drives, over JSON lines.
//!
//! `add --json-io` and `login --json-io` run the same piped login the terminal
//! UI shows in its panel, and report it as one JSON object per line on stdout
//! rather than drawing it:
//!
//! - `{"event":"url","url":…}` once the CLI has named its sign-in page;
//! - `{"event":"need_code"}` once it asks for a code to be pasted back;
//! - `{"event":"done","account":…,"added":bool}` when an account was stored;
//! - `{"event":"error","message":…}` when nothing was.
//!
//! Once the arguments have been parsed, `done` or `error` is always the last
//! line, and nothing else is ever written to stdout. Codes arrive on stdin as
//! `{"code":…}` lines. SIGTERM cancels: the CLI is stopped, and its throwaway
//! configuration directory, with whatever credential is in it, is removed. So
//! does stdin closing after a code has been asked for and before one came.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::account::ProviderKind;
use crate::engine::AddOutcome;
use crate::store::Store;
use crate::tui::login::{Login, State};

/// How often the login is looked at.
const STEP: Duration = Duration::from_millis(50);

/// One line of output.
#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event<'a> {
    Url { url: &'a str },
    NeedCode,
    Done { account: &'a str, added: bool },
    Error { message: &'a str },
}

impl<'a> Event<'a> {
    /// The last line of a login, from how it ended.
    pub fn finish(outcome: &'a Result<AddOutcome, String>) -> Self {
        match outcome {
            Ok(AddOutcome::Added { id }) => Event::Done {
                account: id,
                added: true,
            },
            Ok(AddOutcome::Updated { id }) => Event::Done {
                account: id,
                added: false,
            },
            Err(message) => Event::Error { message },
        }
    }
}

/// Writes `event` as one line and flushes it, so the program reading sees it
/// while the login waits on that program.
pub fn emit(output: &mut impl Write, event: &Event<'_>) -> std::io::Result<()> {
    let line = serde_json::to_string(event)?;
    writeln!(output, "{line}")?;
    output.flush()
}

/// Runs a login to its end, reporting its progress on `output` as it goes,
/// and returns what it stored or why it stored nothing.
///
/// The last line, `done` or `error`, is left to the caller, which may have
/// more to do before the login is over. `stop` cancels the login once set.
pub fn run(
    store: Store,
    provider: ProviderKind,
    account: Option<String>,
    label: Option<String>,
    input: impl BufRead + Send + 'static,
    output: &mut impl Write,
    stop: &AtomicBool,
) -> Result<AddOutcome, String> {
    let mut login = Login::start(store, provider, account, label).map_err(|error| format!("{error:#}"))?;
    let codes = read_codes(input);
    let mut waiting: VecDeque<String> = VecDeque::new();
    let (mut said_url, mut said_need_code, mut cancelled) = (false, false, false);
    let (mut stdin_closed, mut code_sent) = (false, false);
    // Set when this side gave the login up, to report instead of the
    // generic cancellation the login itself reports.
    let mut given_up: Option<&str> = None;

    loop {
        login.update();
        // Nobody left reading means nobody left to finish the login for.
        let mut unread = false;
        if !said_url && let Some(url) = login.settled_link() {
            unread |= emit(output, &Event::Url { url }).is_err();
            said_url = true;
        }
        if !said_need_code && login.wants_input() {
            unread |= emit(output, &Event::NeedCode).is_err();
            said_need_code = true;
        }
        if (unread || stop.load(Ordering::SeqCst)) && !cancelled {
            login.cancel();
            cancelled = true;
        }

        loop {
            match codes.try_recv() {
                Ok(code) => waiting.push_back(code),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stdin_closed = true;
                    break;
                }
            }
        }
        // A code is held only until the CLI has been started and its input
        // exists. From then on it goes straight into that input, where the
        // CLI finds it when it asks.
        if !cancelled
            && login.is_running()
            && let Some(code) = waiting.front()
        {
            login.input.clone_from(code);
            if login.submit_input().is_ok() {
                waiting.pop_front();
                code_sent = true;
            }
            login.input.clear();
        }

        // Asked for a code that can now never come, the CLI would wait for
        // its own timeout with a live credential in its throwaway directory.
        // Before it asks, a closed stdin means nothing: a login the browser's
        // callback finishes never needs one.
        if stdin_closed && said_need_code && !code_sent && waiting.is_empty() && !cancelled {
            login.cancel();
            cancelled = true;
            given_up = Some(STDIN_CLOSED);
        }

        match &login.state {
            State::Running => thread::sleep(STEP),
            State::Succeeded { id, added: true, .. } => return Ok(AddOutcome::Added { id: id.clone() }),
            State::Succeeded { id, .. } => return Ok(AddOutcome::Updated { id: id.clone() }),
            State::Failed(reason) => return Err(given_up.map_or_else(|| reason.clone(), String::from)),
        }
    }
}

/// Why a login was given up when stdin closed before the code it asked for.
const STDIN_CLOSED: &str = "stdin closed while a code was needed";

/// One line of input.
#[derive(Deserialize)]
struct Input {
    code: String,
}

/// Reads codes from `input` on a thread of its own, since reading blocks.
///
/// A line that is not a usable code is passed over with a note on stderr
/// that never repeats it: whatever it was, it may be a code.
fn read_codes(input: impl BufRead + Send + 'static) -> Receiver<String> {
    let (tx, rx) = channel();
    let _ = thread::Builder::new()
        .name("agent-meter-codes".into())
        .spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else {
                    break;
                };
                if line.trim().is_empty() {
                    continue;
                }
                match code_from(&line) {
                    Ok(code) => {
                        if tx.send(code).is_err() {
                            break;
                        }
                    }
                    Err(why) => eprintln!("agent-meter: {why}, so it was passed over"),
                }
            }
        });
    rx
}

/// The code one line of input carries.
fn code_from(line: &str) -> Result<String, &'static str> {
    let input: Input = serde_json::from_str(line).map_err(|_| "a line on stdin was not {\"code\": \"…\"}")?;
    let code = input.code.trim();
    if code.is_empty() {
        return Err("a code on stdin was empty");
    }
    // The CLI reads its code as one line, and a line break inside one would
    // hand it a second line that nobody meant to send.
    if code.chars().any(char::is_control) {
        return Err("a code on stdin held a line break or another control character");
    }
    Ok(code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(event: &Event<'_>) -> String {
        let mut output = Vec::new();
        emit(&mut output, event).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn each_event_is_one_line_of_json() {
        assert_eq!(
            line(&Event::Url {
                url: "https://claude.ai/oauth?a=1&b=2"
            }),
            "{\"event\":\"url\",\"url\":\"https://claude.ai/oauth?a=1&b=2\"}\n"
        );
        assert_eq!(line(&Event::NeedCode), "{\"event\":\"need_code\"}\n");
        let added = Ok(AddOutcome::Added {
            id: "claude-3".into(),
        });
        assert_eq!(
            line(&Event::finish(&added)),
            "{\"event\":\"done\",\"account\":\"claude-3\",\"added\":true}\n"
        );
        let updated = Ok(AddOutcome::Updated {
            id: "claude-1".into(),
        });
        assert_eq!(
            line(&Event::finish(&updated)),
            "{\"event\":\"done\",\"account\":\"claude-1\",\"added\":false}\n"
        );
        // A message spanning lines still makes one line.
        let failed = Err("the login exited\nwith 1".to_string());
        assert_eq!(
            line(&Event::finish(&failed)),
            "{\"event\":\"error\",\"message\":\"the login exited\\nwith 1\"}\n"
        );
    }

    #[test]
    fn a_code_is_one_clean_line_or_nothing() {
        assert_eq!(code_from(r#"{"code": "abc#def"}"#), Ok("abc#def".into()));
        assert_eq!(code_from(r#"{"code": "  abc  "}"#), Ok("abc".into()));
        assert_eq!(
            code_from(r#"{"code": "abc", "requestId": 7}"#),
            Ok("abc".into()),
            "what else a program sends along is its own business"
        );
        for refused in [
            "abc",
            "{}",
            r#"{"code": 7}"#,
            r#"{"code": "   "}"#,
            r#"{"code": "abc\ndef"}"#,
            r#"{"code": "abc\u001b[2J"}"#,
        ] {
            assert!(code_from(refused).is_err(), "{refused}");
        }
        // Whatever was wrong, the reason never repeats the line.
        assert!(
            !code_from(r#"{"code": "secret\nmore"}"#)
                .unwrap_err()
                .contains("secret")
        );
    }

    #[test]
    fn codes_are_read_in_order_and_the_rest_passed_over() {
        let input = "\n{\"code\": \"one\"}\nnot json\n{\"code\": \"two\"}\n";
        let codes = read_codes(std::io::Cursor::new(input.as_bytes().to_vec()));
        let read: Vec<String> = codes.iter().collect();
        assert_eq!(read, ["one", "two"]);
    }
}
