//! The terminal UI.
//!
//! Every operation the CLI offers is reachable here. Work that can block —
//! polling usage, switching accounts — runs on a worker thread so the interface
//! keeps redrawing and stays interruptible.

mod app;
mod draw;
/// Shared with `--json-io`, which reports the same login as JSON lines.
pub(crate) mod login;
mod worker;

pub use app::run;

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::engine::Engine;

/// Renders the interface once, off-screen, and returns the text of the frame.
///
/// This drives the same code the interactive loop draws with, so layout can be
/// checked in tests and from `cargo run --example screenshot` without a
/// terminal to attach to.
pub fn screenshot(engine: &Engine, width: u16, height: u16, watching: bool) -> Result<String> {
    let mut app = app::App::preview(engine, watching)?;
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| draw::draw(frame, &mut app))?;
    Ok(draw::screen_text(terminal.backend().buffer()))
}
