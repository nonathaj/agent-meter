//! Renders the terminal UI to stdout with the real store's data.
//!
//! Useful for checking layout without an interactive terminal:
//! `cargo run --example screenshot -- [width] [height] [watch|settings [row]]`.

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let width: u16 = args.next().map_or(Ok(110), |a| a.parse())?;
    let height: u16 = args.next().map_or(Ok(24), |a| a.parse())?;

    let view = args.next();
    let engine = agent_meter::engine::Engine::open()?;
    let screen = match view.as_deref() {
        // The settings panel, with its cursor on the row named.
        Some("settings") => {
            let row: usize = args.next().map_or(Ok(0), |a| a.parse())?;
            agent_meter::tui::settings_screenshot(&engine, width, height, row)?
        }
        view => agent_meter::tui::screenshot(&engine, width, height, view == Some("watch"))?,
    };
    println!("{screen}");
    Ok(())
}
