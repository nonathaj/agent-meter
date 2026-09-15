//! The command-line interface.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use comfy_table::{Cell, Color, ContentArrangement, LineStyle, Table, TableStyle};
use jiff::Timestamp;
use serde_json::json;

use crate::account::ProviderKind;
use crate::engine::{AddOutcome, Engine, Status, SwitchOutcome, TickOutcome};
use crate::policy::{Blocked, Decision, Reason, Stay};

/// Exit code used when a switch was wanted but every account is used up.
const EXIT_BLOCKED: u8 = 3;

/// A rule under the header and nothing else: account rows are one line each, so
/// boxing every row would cost more ink than it adds.
const TABLE_STYLE: TableStyle = TableStyle::new().header_separator(LineStyle::none().fill('─').junction('─'));

#[derive(Parser, Debug)]
#[command(
    name = "agent-meter",
    version,
    about = "Manage multiple AI coding-agent accounts and switch before you hit a limit",
    long_about = "agent-meter keeps the accounts of your coding agents (Claude Code, Codex) in one \
                  place: import the one you are signed in to, add more by logging in without \
                  disturbing a running agent, see how much of each subscription is left, and let \
                  `agent-meter watch` move you to a fresher account as limits approach.",
    max_term_width = 100
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show every account and how much of its limits is used
    #[command(visible_alias = "ls")]
    List(ListArgs),

    /// Log in to another account without disturbing a running agent
    ///
    /// The login runs against a throwaway configuration directory, so the agent
    /// CLI you have open keeps the credentials it is using.
    Add(AddArgs),

    /// Store the account an agent CLI is already signed in to
    Import(ImportArgs),

    /// Sign an agent CLI in to one of the stored accounts
    #[command(visible_alias = "switch")]
    Use(UseArgs),

    /// Forget a stored account
    #[command(visible_alias = "rm")]
    Remove(RemoveArgs),

    /// Watch usage and switch accounts as limits approach
    Watch(WatchArgs),

    /// Open the terminal UI
    Tui,

    /// Read or change settings
    Config(ConfigArgs),

    /// Show where agent-meter keeps its files
    Where,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Only show accounts of this provider
    #[arg(short, long, value_enum)]
    provider: Option<ProviderKind>,
    /// Poll usage now instead of showing the last reading
    #[arg(short, long)]
    refresh: bool,
    /// Print JSON instead of a table
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct AddArgs {
    /// Which agent CLI to log in to
    #[arg(value_enum)]
    provider: ProviderKind,
    /// A name for this account, shown instead of its email
    #[arg(short, long)]
    label: Option<String>,
    /// Use the device-code flow, for machines with no browser (Codex only)
    #[arg(long)]
    device_code: bool,
}

#[derive(Args, Debug)]
struct ImportArgs {
    /// Which agent CLI to read; omit to import from every one that is signed in
    #[arg(value_enum)]
    provider: Option<ProviderKind>,
    /// A name for this account, shown instead of its email
    #[arg(short, long)]
    label: Option<String>,
}

#[derive(Args, Debug)]
struct UseArgs {
    /// Account id, email or label
    account: String,
}

#[derive(Args, Debug)]
struct RemoveArgs {
    /// Account id, email or label
    account: String,
    /// Do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args, Debug)]
struct WatchArgs {
    /// Check once and exit instead of running until interrupted
    #[arg(long)]
    once: bool,
    /// Switch away from an account once it reaches this percent
    #[arg(short, long)]
    threshold: Option<f64>,
    /// Seconds between checks
    #[arg(short, long)]
    interval: Option<u64>,
    /// Report what would happen without changing anything
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Print the current settings
    Show,
    /// Print the path of the configuration file
    Path,
    /// Change a setting, e.g. `watch.threshold 85`
    Set { key: String, value: String },
}

/// Entry point. Errors are printed with their causes and turned into exit codes.
pub fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            let mut stderr = io::stderr();
            let _ = writeln!(stderr, "agent-meter: {error}");
            for cause in error.chain().skip(1) {
                let _ = writeln!(stderr, "  caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let engine = Engine::open()?;

    match cli.command.unwrap_or(Command::List(ListArgs {
        provider: None,
        refresh: false,
        json: false,
    })) {
        Command::List(args) => list(&engine, &args),
        Command::Add(args) => add(&engine, &args),
        Command::Import(args) => import(&engine, &args),
        Command::Use(args) => switch(&engine, &args),
        Command::Remove(args) => remove(&engine, &args),
        Command::Watch(args) => watch(&engine, &args),
        Command::Tui => crate::tui::run(&engine).map(|()| ExitCode::SUCCESS),
        Command::Config(args) => config(&engine, &args),
        Command::Where => {
            println!("{}", engine.store().dir().display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn list(engine: &Engine, args: &ListArgs) -> Result<ExitCode> {
    if args.refresh {
        engine.poll(&[], true)?;
    }
    let mut statuses = engine.status()?;
    if let Some(provider) = args.provider {
        statuses.retain(|s| s.account.provider == provider);
    }

    if args.json {
        let now = Timestamp::now();
        let rows: Vec<_> = statuses.iter().map(|s| status_json(s, now)).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(ExitCode::SUCCESS);
    }

    if statuses.is_empty() {
        println!("No accounts yet. Add one with:");
        println!("  agent-meter import        # store the account you are signed in to");
        println!("  agent-meter add claude    # log in to another one");
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", render_table(&statuses, Timestamp::now()));
    Ok(ExitCode::SUCCESS)
}

fn status_json(status: &Status, now: Timestamp) -> serde_json::Value {
    json!({
        "id": status.account.id,
        "provider": status.account.provider,
        "label": status.account.label,
        "email": status.account.identity.email,
        "plan": status.account.identity.plan,
        "organization": status.account.identity.workspace_name,
        "active": status.active,
        "needsLogin": status.account.needs_login,
        "usage": status.usage.as_ref().map(|usage| json!({
            "observedAt": usage.observed_at,
            "usedPercent": usage.used_at(now),
            "exhausted": usage.is_exhausted_at(now),
            "windows": usage.windows.iter().map(|w| json!({
                "label": w.label(),
                "windowSeconds": w.window_secs,
                "usedPercent": w.used_at(now),
                "resetsAt": w.resets_at,
            })).collect::<Vec<_>>(),
        })),
        "error": status.error,
    })
}

fn render_table(statuses: &[Status], now: Timestamp) -> String {
    let mut table = Table::new();
    table
        .load_style(TABLE_STYLE)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([
            "",
            "ID",
            "PROVIDER",
            "ACCOUNT",
            "PLAN",
            "USED",
            "WINDOWS",
            "RESETS IN",
            "",
        ]);

    let mut notes = Vec::new();
    for status in statuses {
        let account = &status.account;
        let used = status.used(now);
        let windows = status
            .usage
            .as_ref()
            .map(|usage| {
                usage
                    .windows
                    .iter()
                    .map(|w| format!("{} {:.0}%", w.label(), w.used_at(now)))
                    .collect::<Vec<_>>()
                    .join("  ")
            })
            .unwrap_or_default();
        let resets = status
            .usage
            .as_ref()
            .and_then(|usage| usage.binding_window(now))
            .and_then(|w| w.resets_at)
            .map(|at| crate::timefmt::until(now, at))
            .unwrap_or_else(|| "-".into());

        // Keep the numbers in their columns: a flag here, the explanation
        // under the table, where a long provider message cannot stretch it.
        let ago = status
            .error_at
            .map(|at| format!(" {} ago", crate::timefmt::since(now, at)))
            .unwrap_or_default();
        let (flag, detail) = match (&account.needs_login, &status.error) {
            (Some(reason), _) => ("login needed", Some(format!("{}: {reason}", account.id))),
            (None, Some(error)) if status.usage.is_some() => (
                "reading is stale",
                Some(format!("{}: poll failed{ago}: {error}", account.id)),
            ),
            (None, Some(error)) => (
                "no usage yet",
                Some(format!("{}: poll failed{ago}: {error}", account.id)),
            ),
            (None, None) => ("", None),
        };
        notes.extend(detail);

        table.add_row([
            Cell::new(if status.active { "*" } else { "" }).fg(Color::Green),
            Cell::new(&account.id),
            Cell::new(account.provider.display_name()),
            Cell::new(account.display_name()),
            Cell::new(account.identity.plan.as_deref().unwrap_or("-")),
            used_cell(
                used,
                status.usage.as_ref().is_some_and(|u| u.is_exhausted_at(now)),
            ),
            Cell::new(windows),
            Cell::new(resets),
            Cell::new(flag).fg(if account.needs_login.is_some() {
                Color::Red
            } else {
                Color::Yellow
            }),
        ]);
    }

    let mut rendered = format!("{table}\n");
    for note in notes {
        rendered.push_str(&format!("  ! {note}\n"));
    }
    rendered
}

fn used_cell(used: Option<f64>, exhausted: bool) -> Cell {
    match used {
        None => Cell::new("?"),
        Some(used) => {
            let cell = Cell::new(format!("{used:.0}%"));
            if exhausted || used >= 100.0 {
                cell.fg(Color::Red)
            } else if used >= 90.0 {
                cell.fg(Color::Yellow)
            } else {
                cell.fg(Color::Green)
            }
        }
    }
}

fn add(engine: &Engine, args: &AddArgs) -> Result<ExitCode> {
    println!(
        "Logging in to a new {} account in a temporary configuration directory.",
        args.provider.display_name()
    );
    println!(
        "Any {} you already have running keeps its own login.\n",
        args.provider.display_name()
    );

    let outcome = engine.login(args.provider, args.label.clone(), args.device_code, |command| {
        command
            .status()
            .context("starting the login. Is the CLI installed and on PATH?")
    })?;
    report_added(engine, &outcome)
}

fn import(engine: &Engine, args: &ImportArgs) -> Result<ExitCode> {
    let providers: Vec<_> = args
        .provider
        .map_or_else(|| ProviderKind::ALL.to_vec(), |p| vec![p]);
    let mut imported = 0;
    let mut last = None;

    for provider in providers {
        match engine.import(provider, args.label.clone()) {
            Ok(outcome) => {
                imported += 1;
                println!(
                    "{} {} from {}",
                    match outcome {
                        AddOutcome::Added { .. } => "Imported",
                        AddOutcome::Updated { .. } => "Updated",
                    },
                    outcome.id(),
                    provider.display_name()
                );
                last = Some(outcome);
            }
            // With no provider named, this is a survey: a CLI that is not
            // signed in here is expected, not an error.
            Err(error) if args.provider.is_none() => {
                eprintln!("Skipped {}: {error}", provider.display_name());
            }
            Err(error) => return Err(error),
        }
    }

    match last {
        Some(outcome) if imported == 1 => report_added(engine, &outcome),
        Some(_) => Ok(ExitCode::SUCCESS),
        None => bail!("no agent CLI on this machine is signed in to an account agent-meter can read"),
    }
}

fn report_added(engine: &Engine, outcome: &AddOutcome) -> Result<ExitCode> {
    let account = engine.resolve(outcome.id())?;
    match outcome {
        AddOutcome::Added { .. } => println!("\nAdded {} ({})", account.id, account.display_name()),
        AddOutcome::Updated { .. } => {
            println!(
                "\nUpdated {} ({}) — it was already stored",
                account.id,
                account.display_name()
            );
        }
    }
    // A first reading makes the account eligible for automatic switching.
    if let Some((_, Err(error))) = engine.poll(std::slice::from_ref(&account.id), true)?.first() {
        eprintln!("Could not read its usage yet: {error}");
    }
    print!("{}", render_table(&engine.status()?, Timestamp::now()));
    Ok(ExitCode::SUCCESS)
}

fn switch(engine: &Engine, args: &UseArgs) -> Result<ExitCode> {
    let account = engine.resolve(&args.account)?;
    let outcome = engine.switch_to(&account.id)?;
    print_switch(&outcome, account.provider.display_name());
    Ok(ExitCode::SUCCESS)
}

fn print_switch(outcome: &SwitchOutcome, provider: &str) {
    match &outcome.from {
        Some(from) if from == &outcome.to => println!("{} is already signed in to {}", provider, outcome.to),
        Some(from) => println!("{provider}: {from} -> {}", outcome.to),
        None => println!("{provider}: signed in to {}", outcome.to),
    }
    if outcome.restart_required {
        println!("Restart any running {provider} session to pick up the new account.");
    }
}

fn remove(engine: &Engine, args: &RemoveArgs) -> Result<ExitCode> {
    let account = engine.resolve(&args.account)?;
    if !args.yes {
        let prompt = format!(
            "Remove {} ({}) from agent-meter? The account itself is not touched, \
             and you can add it back by signing in again. [y/N] ",
            account.id,
            account.display_name()
        );
        if !confirm(&prompt)? {
            println!("Nothing was removed.");
            return Ok(ExitCode::SUCCESS);
        }
    }
    engine.remove(&account.id)?;
    println!("Removed {}", account.id);
    Ok(ExitCode::SUCCESS)
}

fn confirm(prompt: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("cannot ask for confirmation without a terminal; pass --yes");
    }
    print!("{prompt}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn watch(engine: &Engine, args: &WatchArgs) -> Result<ExitCode> {
    let mut engine = Engine::with_store(engine.store().clone())?;
    engine.override_watch(args.threshold, args.interval)?;
    let interval = Duration::from_secs(engine.config().watch.poll_secs);

    if args.once {
        let outcomes = run_tick(&engine, args.dry_run)?;
        return Ok(exit_code_for(&outcomes));
    }

    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    ctrlc::set_handler(move || flag.store(false, Ordering::SeqCst))
        .context("installing the interrupt handler")?;

    println!(
        "Watching every {} at a {:.0}% threshold. Press Ctrl-C to stop.",
        crate::timefmt::duration(interval.as_secs()),
        engine.config().watch.threshold
    );
    while running.load(Ordering::SeqCst) {
        if let Err(error) = run_tick(&engine, args.dry_run) {
            // A failed check should not end the watch: the provider may be
            // briefly unreachable, and the next tick will try again.
            eprintln!("{} check failed: {error}", Timestamp::now());
        }
        // Wake often enough to notice Ctrl-C promptly.
        let mut waited = Duration::ZERO;
        while running.load(Ordering::SeqCst) && waited < interval {
            std::thread::sleep(Duration::from_millis(200));
            waited += Duration::from_millis(200);
        }
    }
    println!("Stopped.");
    Ok(ExitCode::SUCCESS)
}

fn run_tick(engine: &Engine, dry_run: bool) -> Result<Vec<TickOutcome>> {
    let outcomes = if dry_run {
        engine.dry_tick()?
    } else {
        engine.tick()?
    };
    for outcome in &outcomes {
        let provider = outcome.provider.display_name();
        if let Some(switched) = &outcome.switched {
            print_switch(switched, provider);
            continue;
        }
        if let Some(held) = &outcome.held {
            println!("{provider}: {held}");
            continue;
        }
        println!("{provider}: {}", describe(&outcome.decision, dry_run));
    }
    Ok(outcomes)
}

fn describe(decision: &Decision, dry_run: bool) -> String {
    match decision {
        Decision::Stay(Stay::BelowThreshold { used }) => format!("{used:.0}% used, staying put"),
        Decision::Stay(Stay::NoActiveAccount) => {
            "no stored account is signed in; run `agent-meter use <account>`".into()
        }
        Decision::Stay(Stay::UsageUnknown) => "usage unknown, staying put".into(),
        Decision::Stay(Stay::NoBetterAccount { used }) => {
            format!("{used:.0}% used, but no other account has meaningfully more left")
        }
        Decision::Switch { to, reason } => {
            let why = match reason {
                Reason::ThresholdCrossed { used, target_used } => {
                    format!("{used:.0}% used; {to} is at {target_used:.0}%")
                }
                Reason::BestOfExhausted { used, target_used } => {
                    format!("every account is busy; {to} is the roomiest at {target_used:.0}% vs {used:.0}%")
                }
                Reason::ActiveExhausted { target_used } => {
                    format!("out of quota; {to} is at {target_used:.0}%")
                }
            };
            if dry_run {
                format!("would switch to {to} ({why})")
            } else {
                format!("switching to {to} ({why})")
            }
        }
        Decision::Blocked(Blocked::AllExhausted {
            relief_at,
            relief_account,
        }) => match (relief_at, relief_account) {
            (Some(at), Some(account)) => format!(
                "every account is out of quota; {account} frees up in {}",
                crate::timefmt::until(Timestamp::now(), *at)
            ),
            _ => "every account is out of quota".into(),
        },
        Decision::Blocked(Blocked::NoAlternative) => {
            "over the threshold, but there is no other usable account to switch to".into()
        }
    }
}

fn exit_code_for(outcomes: &[TickOutcome]) -> ExitCode {
    if outcomes
        .iter()
        .any(|o| matches!(o.decision, Decision::Blocked(_)))
    {
        ExitCode::from(EXIT_BLOCKED)
    } else {
        ExitCode::SUCCESS
    }
}

fn config(engine: &Engine, args: &ConfigArgs) -> Result<ExitCode> {
    match &args.command {
        ConfigCommand::Path => println!("{}", crate::config::Config::path(engine.store().dir()).display()),
        ConfigCommand::Show => print!("{}", toml::to_string_pretty(engine.config())?),
        ConfigCommand::Set { key, value } => {
            let mut config = engine.config().clone();
            config.set(key, value)?;
            config.save(engine.store().dir())?;
            println!("{key} = {value}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_the_main_verbs() {
        assert!(Cli::parse_from(["agent-meter"]).command.is_none());
        assert!(matches!(
            Cli::parse_from(["agent-meter", "ls"]).command,
            Some(Command::List(_))
        ));
        let Some(Command::Add(args)) =
            Cli::parse_from(["agent-meter", "add", "codex", "--device-code"]).command
        else {
            panic!("expected add");
        };
        assert_eq!(args.provider, ProviderKind::Codex);
        assert!(args.device_code);

        let Some(Command::Watch(args)) = Cli::parse_from(["agent-meter", "watch", "-t", "80"]).command else {
            panic!("expected watch");
        };
        assert_eq!(args.threshold, Some(80.0));
    }
}
