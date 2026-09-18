//! Renders the terminal UI to stdout with the real store's data.
//!
//! Useful for checking layout without an interactive terminal:
//! `cargo run --example screenshot -- [width] [height] [watch]`.

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let width: u16 = args.next().map_or(Ok(110), |a| a.parse())?;
    let height: u16 = args.next().map_or(Ok(24), |a| a.parse())?;

    let watching = args.next().is_some_and(|arg| arg == "watch");

    let engine = agent_meter::engine::Engine::open()?;
    println!(
        "{}",
        agent_meter::tui::screenshot(&engine, width, height, watching)?
    );
    Ok(())
}
