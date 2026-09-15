//! The terminal UI.
//!
//! Every operation the CLI offers is reachable here. Work that can block —
//! polling usage, switching accounts — runs on a worker thread so the interface
//! keeps redrawing and stays interruptible.

mod app;
mod draw;
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
pub fn screenshot(engine: &Engine, width: u16, height: u16) -> Result<String> {
    let mut app = app::App::preview(engine)?;
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| draw::draw(frame, &mut app))?;

    let buffer = terminal.backend().buffer().clone();
    Ok((0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n"))
}
