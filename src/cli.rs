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

use std::path::PathBuf;

use crate::account::ProviderKind;
use crate::engine::{AddOutcome, Engine, Status, SwitchOutcome, TickOutcome};
use crate::foreign::Source;
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
    /// Register account homes and inspect persistent session assignments
    Fleet(FleetArgs),
    /// Launch or resume a CLI on its pinned subscription account
    Run(RunArgs),
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

    /// Write these accounts into another tool's store
    Export(ExportArgs),

    /// Keep another machine's accounts in step with this one
    Sync(SyncArgs),

    /// The other machines agent-meter knows how to reach
    Remote(RemoteArgs),

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
struct FleetArgs {
    #[command(subcommand)]
    command: FleetCommand,
}

#[derive(Subcommand, Debug)]
enum FleetCommand {
    /// Register an existing independently authenticated CLI home (never copies tokens)
    Register {
        account: String,
        #[arg(long)]
        home: PathBuf,
    },
    /// Print account homes and conversation bindings as credential-free JSON
    List,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[arg(long, value_enum)]
    provider: ProviderKind,
    /// Account id or label; required on first launch, optional on resume
    #[arg(long, env = "AGENT_METER_ACCOUNT")]
    account: Option<String>,
    /// Stable city namespace; Gas City sets GT_ROOT
    #[arg(long, env = "GT_ROOT")]
    scope: String,
    /// Durable conversation identity; Gas City sets GC_SESSION_ID
    #[arg(long, env = "GC_SESSION_ID")]
    session: String,
    /// Require the selected home to match runtime metadata supplied by an orchestrator
    #[arg(long)]
    expect_home: Option<PathBuf>,
    /// Print launch metadata without launching or recording a binding
    #[arg(long)]
    dry_run: bool,
    /// Arguments forwarded verbatim to the native CLI
    #[arg(last = true)]
    args: Vec<std::ffi::OsString>,
}

fn fleet(engine: &Engine, args: &FleetArgs) -> Result<ExitCode> {
    match &args.command {
        FleetCommand::Register { account, home } => {
            let account = engine.resolve(account)?;
            let home = crate::fleet::Fleet::register(engine.store(), &account.id, home)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"account": account.id, "home": home}))?
            );
        }
        FleetCommand::List => println!(
            "{}",
            serde_json::to_string_pretty(&crate::fleet::Fleet::load(engine.store())?)?
        ),
    }
    Ok(ExitCode::SUCCESS)
}

fn run_pinned(engine: &Engine, args: &RunArgs) -> Result<ExitCode> {
    let account = args.account.as_deref().map(|id| engine.resolve(id)).transpose()?;
    if let Some(expected) = &args.expect_home {
        let plan = crate::fleet::Fleet::launch(
            engine.store(),
            args.provider,
            &args.scope,
            &args.session,
            account.as_ref().map(|a| a.id.as_str()),
            false,
        )?;
        anyhow::ensure!(
            expected.is_absolute()
                && expected.canonicalize().context("opening expected fleet home")? == plan.home,
            "selected fleet home differs from the orchestrator's --expect-home; fix runtime metadata before launching"
        );
    }
    let launch = crate::fleet::Fleet::launch(
        engine.store(),
        args.provider,
        &args.scope,
        &args.session,
        account.as_ref().map(|a| a.id.as_str()),
        !args.dry_run,
    )?;
    if args.dry_run {
        println!("{}", serde_json::to_string_pretty(&launch)?);
        Ok(ExitCode::SUCCESS)
    } else {
        launch.execute(&args.args)
    }
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Only show accounts of this provider
    #[arg(short, long, value_enum)]
    provider: Option<ProviderKind>,
    /// Poll usage now instead of showing the last reading
    #[arg(short, long)]
    refresh: bool,
    /// Poll only readings due under provider intervals and backoff
    #[arg(long, conflicts_with = "refresh")]
    poll: bool,
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
    /// Which agent CLI to read; omit to import every one that applies
    #[arg(value_enum)]
    provider: Option<ProviderKind>,
    /// A name for this account, shown instead of its email
    ///
    /// Only applies when importing a single signed-in account; accounts taken
    /// from another tool keep the names that tool gave them.
    #[arg(short, long)]
    label: Option<String>,
    /// Where to import from: the signed-in CLIs, or another tool's store
    #[arg(long, value_enum, default_value = "live")]
    from: Source,
    /// Where that tool keeps its files, if not in its usual place
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct ExportArgs {
    /// Which tool to write into
    #[arg(long, value_enum)]
    to: Source,
    /// Only export accounts of this provider
    #[arg(value_enum)]
    provider: Option<ProviderKind>,
    /// Where that tool keeps its files, if not in its usual place
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,
    /// Do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args, Debug)]
struct SyncArgs {
    /// Which machines; omit for every one configured
    ///
    /// A name that is not configured is taken as an ssh destination, so a
    /// machine can be synced with once without being written down first.
    #[arg(value_name = "MACHINE")]
    names: Vec<String>,
    /// Only send; take nothing back
    #[arg(long)]
    push: bool,
    /// Only take; send nothing
    #[arg(long, conflicts_with = "push")]
    pull: bool,
    /// Only accounts of this provider
    #[arg(short, long, value_enum)]
    provider: Option<ProviderKind>,
    /// Say what would happen and write nothing, here or there
    #[arg(short = 'n', long)]
    dry_run: bool,
    /// Do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
    /// Answer one exchange arriving on standard input
    ///
    /// This is the other end of `agent-meter sync`, started over ssh. It reads
    /// one request and writes one reply, and prints nothing else.
    #[arg(long, hide = true, conflicts_with_all = ["names", "push", "pull", "dry_run"])]
    serve: bool,
}

#[derive(Args, Debug)]
struct RemoteArgs {
    #[command(subcommand)]
    command: RemoteCommand,
}

#[derive(Subcommand, Debug)]
enum RemoteCommand {
    /// Remember a machine to sync with
    Add {
        /// What to call it here
        name: String,
        /// Where to reach it: `user@host`, or a host from your ssh config
        destination: String,
        /// What agent-meter is called there, if it is not on the PATH
        #[arg(long)]
        command: Option<String>,
        /// Do not send to this machine from `agent-meter watch`
        #[arg(long)]
        no_auto: bool,
    },
    /// Show the machines that are configured
    #[command(visible_alias = "ls")]
    List,
    /// Forget a machine
    #[command(visible_alias = "rm")]
    Remove { name: String },
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
        poll: false,
        json: false,
    })) {
        Command::Fleet(args) => fleet(&engine, &args),
        Command::Run(args) => run_pinned(&engine, &args),
        Command::List(args) => list(&engine, &args),
        Command::Add(args) => add(&engine, &args),
        Command::Import(args) => import(&engine, &args),
        Command::Export(args) => export(&engine, &args),
        Command::Sync(args) => sync(&engine, &args),
        Command::Remote(args) => remote(&engine, &args),
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
    if args.refresh || args.poll {
        if let Some(provider) = args.provider {
            let ids: Vec<_> = engine
                .store()
                .accounts()?
                .into_iter()
                .filter(|a| a.provider == provider)
                .map(|a| a.id)
                .collect();
            if !ids.is_empty() {
                engine.poll(&ids, args.refresh)?;
            }
        } else {
            engine.poll(&[], args.refresh)?;
        }
    }
    let mut statuses = engine.status()?;
    if let Some(provider) = args.provider {
        statuses.retain(|s| s.account.provider == provider);
    }

    if args.json {
        let now = Timestamp::now();
        let rows: Vec<_> = statuses
            .iter()
            .map(|s| status_json(s, now, engine.poll_interval(s.account.provider).as_secs()))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(ExitCode::SUCCESS);
    }

    if statuses.is_empty() {
        println!("No accounts yet. Add one with:");
        println!("  agent-meter import        # store the account you are signed in to");
        println!("  agent-meter add claude    # log in to another one");
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", render_table(engine, &statuses, Timestamp::now()));
    Ok(ExitCode::SUCCESS)
}

fn status_json(status: &Status, now: Timestamp, poll_interval: u64) -> serde_json::Value {
    json!({
        "id": status.account.id,
        "provider": status.account.provider,
        "label": status.account.label,
        "email": status.account.identity.email,
        "plan": status.account.identity.plan,
        "capacityMultiplier": status.account.identity.capacity,
        "organization": status.account.identity.workspace_name,
        "active": status.active,
        "fleetHome": status.fleet_home,
        "needsLogin": status.account.needs_login,
        "pollIntervalSeconds": poll_interval,
        "usage": status.usage.as_ref().map(|usage| json!({
            "observedAt": usage.observed_at,
            "usedPercent": usage.used_at(now),
            "exhausted": usage.is_exhausted_at(now),
            "windows": usage.windows.iter().map(|w| json!({
                "label": w.label(),
                "kind": match w.kind() {
                    crate::usage::WindowKind::FiveHour => "five_hour",
                    crate::usage::WindowKind::Weekly => "weekly",
                    crate::usage::WindowKind::Other => "other",
                },
                "scope": w.scope,
                "windowSeconds": w.window_secs,
                "usedPercent": w.used_at(now),
                "resetsAt": w.resets_at,
            })).collect::<Vec<_>>(),
        })),
        "error": status.error,
        "errorAt": status.error_at,
    })
}

fn render_table(engine: &Engine, statuses: &[Status], now: Timestamp) -> String {
    let mut table = Table::new();
    table
        .load_style(TABLE_STYLE)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([
            "",
            "ID",
            "PROVIDER",
            "ACCOUNT",
            "ORGANIZATION",
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
        // An age is worth saying only when it is a surprise: a reading older
        // than the interval that should already have replaced it. Saying "just
        // now" on every row spends a column on the ordinary case, and where a
        // poll failed the error already explains why the figures are not
        // fresher — the age is the redundant half of that pair.
        let overdue = status.usage.as_ref().filter(|usage| {
            let interval = engine.poll_interval(account.provider).as_secs() as i64;
            usage.age_secs(now) > interval + (interval / 2).max(60)
        });
        let (flag, detail) = match (&account.needs_login, &status.error, overdue) {
            (Some(reason), _, _) => ("login needed", Some(format!("{}: {reason}", account.id))),
            (None, Some(error), _) if status.usage.is_some() => {
                ("reading is stale", Some(format!("{}: {error}", account.id)))
            }
            (None, Some(error), _) => ("no usage yet", Some(format!("{}: {error}", account.id))),
            // Nothing failed, and yet nothing has read this account lately —
            // which usually means no watcher is running.
            (None, None, Some(usage)) => (
                "",
                Some(format!(
                    "{}: read {} ago; `agent-meter list --refresh` or `agent-meter watch` updates it",
                    account.id,
                    crate::timefmt::since(now, usage.observed_at)
                )),
            ),
            (None, None, None) if status.fleet_home.is_some() => ("fleet home", None),
            (None, None, None) => ("", None),
        };
        notes.extend(detail);

        table.add_row([
            Cell::new(if status.active { "*" } else { "" }).fg(Color::Green),
            Cell::new(&account.id),
            Cell::new(account.provider.display_name()),
            Cell::new(account.display_name()),
            // The organisation is part of which account this is: the same
            // address in two of them is two accounts, with separate limits.
            Cell::new(account.identity.workspace_label().unwrap_or("-".into())),
            Cell::new(account.identity.plan_label().unwrap_or_else(|| "-".into())),
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
    report_added(engine, &[outcome])
}

fn import(engine: &Engine, args: &ImportArgs) -> Result<ExitCode> {
    if args.from != Source::Live {
        let imported = engine.import_from(args.from, args.dir.as_deref(), args.provider)?;
        let outcomes: Vec<_> = imported
            .into_iter()
            .map(|(origin, outcome)| {
                println!(
                    "{} {} from {origin}",
                    match outcome {
                        AddOutcome::Added { .. } => "Imported",
                        AddOutcome::Updated { .. } => "Updated",
                    },
                    outcome.id()
                );
                outcome
            })
            .collect();
        return report_added(engine, &outcomes);
    }

    let providers: Vec<_> = args
        .provider
        .map_or_else(|| ProviderKind::ALL.to_vec(), |p| vec![p]);
    let mut outcomes = Vec::new();
    for provider in providers {
        match engine.import(provider, args.label.clone()) {
            Ok(outcome) => {
                println!(
                    "{} {} from {}",
                    match outcome {
                        AddOutcome::Added { .. } => "Imported",
                        AddOutcome::Updated { .. } => "Updated",
                    },
                    outcome.id(),
                    provider.display_name()
                );
                outcomes.push(outcome);
            }
            // With no provider named, this is a survey of the machine: a CLI
            // that is not signed in here is expected, not an error.
            Err(error) if args.provider.is_none() => {
                eprintln!("Skipped {}: {error}", provider.display_name());
            }
            Err(error) => return Err(error),
        }
    }
    if outcomes.is_empty() {
        bail!("no agent CLI on this machine is signed in to an account agent-meter can read");
    }
    report_added(engine, &outcomes)
}

/// Names what was added and shows where it leaves the user.
fn report_added(engine: &Engine, outcomes: &[AddOutcome]) -> Result<ExitCode> {
    if let [outcome] = outcomes {
        let account = engine.resolve(outcome.id())?;
        match outcome {
            AddOutcome::Added { .. } => println!("\nAdded {} ({})", account.id, account.display_name()),
            AddOutcome::Updated { .. } => println!(
                "\nUpdated {} ({}) — it was already stored",
                account.id,
                account.display_name()
            ),
        }
    }

    // A first reading is what makes an account eligible for switching, so take
    // one now rather than leaving the table full of question marks.
    let ids: Vec<String> = outcomes.iter().map(|o| o.id().to_string()).collect();
    for (id, result) in engine.poll(&ids, true)? {
        if let Err(error) = result {
            eprintln!("Could not read usage for {id} yet: {error}");
        }
    }
    println!();
    print!("{}", render_table(engine, &engine.status()?, Timestamp::now()));
    Ok(ExitCode::SUCCESS)
}

/// Writes agent-meter's accounts into another tool's store.
fn export(engine: &Engine, args: &ExportArgs) -> Result<ExitCode> {
    let fleet = crate::fleet::Fleet::load(engine.store())?;
    let mut accounts = engine.store().accounts()?;
    accounts.retain(|account| {
        let managed = fleet.homes.contains_key(&account.id);
        if managed {
            eprintln!("skip {}: fleet credentials stay in their native home", account.id);
        }
        !managed
    });
    if let Some(provider) = args.provider {
        accounts.retain(|account| account.provider == provider);
    }
    if accounts.is_empty() {
        bail!("there are no accounts to export");
    }

    // Worked out first and shown before anything is written: this is somebody
    // else's store, and it may be one they are still using.
    let plan = crate::foreign::export(args.to, args.dir.as_deref(), &accounts, false)?;
    let tool = args.to.display_name();
    for line in &plan.added {
        println!("  add     {line}");
    }
    for line in &plan.updated {
        println!("  update  {line}");
    }
    for line in &plan.skipped {
        println!("  skip    {line}");
    }
    if plan.untouched > 0 {
        println!(
            "  keep    {} account(s) {tool} holds that agent-meter does not",
            plan.untouched
        );
    }
    if plan.writes() == 0 {
        println!("Nothing to write to {tool}.");
        return Ok(ExitCode::SUCCESS);
    }

    if !args.yes {
        let prompt = format!(
            "\nWrite {} account(s) into {tool}? Its own accounts are kept. [y/N] ",
            plan.writes()
        );
        if !confirm(&prompt)? {
            println!("Nothing was written.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    // A fleet registration may have happened while confirmation was pending.
    let _lock = engine.store().lock()?;
    let fleet = crate::fleet::Fleet::load(engine.store())?;
    anyhow::ensure!(
        !accounts.iter().any(|a| fleet.homes.contains_key(&a.id)),
        "an account was registered for fleet use during export; rerun to review the new export set"
    );
    let done = crate::foreign::export(args.to, args.dir.as_deref(), &accounts, true)?;
    println!(
        "Wrote {} account(s) to {tool}: {} added, {} updated.",
        done.writes(),
        done.added.len(),
        done.updated.len()
    );
    Ok(ExitCode::SUCCESS)
}

/// Keeps other machines' accounts in step with this one.
fn sync(engine: &Engine, args: &SyncArgs) -> Result<ExitCode> {
    if args.serve {
        // The other end of the conversation. Nothing else may be printed on
        // standard output while this runs: the reply is the whole of it.
        return crate::remote::serve(engine, io::stdin().lock(), io::stdout().lock())
            .map(|()| ExitCode::SUCCESS);
    }

    let machines = crate::remote::resolve(&engine.config().remote, &args.names)?;
    let options = crate::remote::Options {
        push: !args.pull,
        pull: !args.push,
        apply: false,
        provider: args.provider,
        unattended: false,
    };

    let mut failures = 0;
    for (name, machine) in &machines {
        // Worked out and shown before anything is written, here or there:
        // these are credentials, and the machine at the other end may be one
        // somebody is working on right now.
        let planned = match crate::remote::exchange(engine, machine, options) {
            Ok(planned) => planned,
            Err(error) => {
                eprintln!("{name}: {error:#}");
                failures += 1;
                continue;
            }
        };
        report_exchange(name, &planned);

        let writes = planned.sent.writes() + planned.received.writes();
        if args.dry_run || writes == 0 {
            continue;
        }
        if !args.yes && !confirm(&format!("\nApply to {name} and here? [y/N] "))? {
            println!("Nothing was written.");
            continue;
        }
        match crate::remote::exchange(
            engine,
            machine,
            crate::remote::Options {
                apply: true,
                ..options
            },
        ) {
            Ok(done) => println!(
                "{name}: {} sent, {} taken.",
                done.sent.writes(),
                done.received.writes()
            ),
            Err(error) => {
                eprintln!("{name}: {error:#}");
                failures += 1;
            }
        }
    }

    if failures > 0 && failures == machines.len() {
        bail!("no machine could be reached");
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints what an exchange did, in both directions.
fn report_exchange(name: &str, exchange: &crate::remote::Exchange) {
    println!("{name} — agent-meter {}", exchange.version);
    let side = |heading: String, report: &crate::remote::Report, there: &str| {
        println!("  {heading}");
        for id in &report.added {
            println!("    add     {id} {there}");
        }
        for id in &report.updated {
            println!("    update  {id} {there}");
        }
        for note in &report.skipped {
            println!("    skip    {note}");
        }
        if report.unchanged > 0 {
            println!("    {} already current {there}", report.unchanged);
        }
    };
    side(
        format!("sent {} account(s)", exchange.offered),
        &exchange.sent,
        "there",
    );
    side("taken back".to_string(), &exchange.received, "here");
}

/// Sends changed credentials to every machine that is kept in step
/// automatically. Used by `watch`, where nobody is at the keyboard.
fn push_to_remotes(engine: &Engine) {
    let machines: Vec<_> = engine
        .config()
        .remote
        .iter()
        .filter(|(_, machine)| machine.is_automatic())
        .map(|(name, machine)| (name.clone(), machine.clone()))
        .collect();

    for (name, machine) in machines {
        let options = crate::remote::Options {
            apply: true,
            unattended: true,
            ..crate::remote::Options::default()
        };
        match crate::remote::exchange(engine, &machine, options) {
            Ok(done) if done.sent.writes() + done.received.writes() > 0 => println!(
                "  synced with {name}: {} sent, {} taken",
                done.sent.writes(),
                done.received.writes()
            ),
            Ok(_) => {}
            // A machine that is asleep is not a reason to stop watching this
            // one, and the credentials it is missing will go next time.
            Err(error) => eprintln!("  {name} was not reached: {error:#}"),
        }
    }
}

fn remote(engine: &Engine, args: &RemoteArgs) -> Result<ExitCode> {
    let dir = engine.store().dir();
    match &args.command {
        RemoteCommand::Add {
            name,
            destination,
            command,
            no_auto,
        } => {
            let mut config = engine.config().clone();
            config.set(&format!("remote.{name}.ssh"), destination)?;
            if let Some(command) = command {
                config.set(&format!("remote.{name}.command"), command)?;
            }
            if *no_auto {
                config.set(&format!("remote.{name}.auto"), "false")?;
            }
            config.save(dir)?;
            println!("{name} -> {destination}");
            println!(
                "Check it with `agent-meter sync {name} --dry-run`. agent-meter must be installed \
                 there, and ssh must reach it without asking anything."
            );
        }
        RemoteCommand::List => {
            let machines = &engine.config().remote;
            if machines.is_empty() {
                println!("No other machines. Add one with `agent-meter remote add <name> <user@host>`.");
            }
            for (name, machine) in machines {
                let where_ = machine.ssh.clone().unwrap_or_else(|| machine.exec.join(" "));
                let automatic = if machine.is_automatic() {
                    "watched"
                } else {
                    "on request"
                };
                println!("{name:<16} {where_:<30} {automatic}");
            }
        }
        RemoteCommand::Remove { name } => {
            let mut config = engine.config().clone();
            if config.remote.remove(name).is_none() {
                bail!("no machine called {name:?}");
            }
            config.save(dir)?;
            println!("Removed {name}");
        }
    }
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
        let outcomes = tick_and_share(&engine, args.dry_run)?;
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
        let sleep_for = match tick_and_share(&engine, args.dry_run) {
            Ok(outcomes) => wait_after(&outcomes, &engine, interval),
            Err(error) => {
                // A failed check should not end the watch: the provider may be
                // briefly unreachable, and the next tick will try again.
                eprintln!("{} check failed: {error}", Timestamp::now());
                interval
            }
        };
        // Wake often enough to notice Ctrl-C promptly.
        let mut waited = Duration::ZERO;
        while running.load(Ordering::SeqCst) && waited < sleep_for {
            std::thread::sleep(Duration::from_millis(200));
            waited += Duration::from_millis(200);
        }
    }
    println!("Stopped.");
    Ok(ExitCode::SUCCESS)
}

/// One check, and then whatever it changed goes to the other machines.
///
/// This is the whole point of naming them: a credential refreshed here is one
/// they do not have, and the account they hand out next would be the spent
/// one. Only a check that actually changed a credential is worth an ssh
/// connection, so the credentials are fingerprinted either side of it.
fn tick_and_share(engine: &Engine, dry_run: bool) -> Result<Vec<TickOutcome>> {
    let automatic = engine
        .config()
        .remote
        .values()
        .any(crate::config::RemoteConfig::is_automatic);
    let before = (automatic && !dry_run)
        .then(|| engine.credential_mark())
        .transpose()?;

    let outcomes = run_tick(engine, dry_run)?;

    if let Some(before) = before
        && engine.credential_mark()? != before
    {
        push_to_remotes(engine);
    }
    Ok(outcomes)
}

/// How long to wait before the next check.
///
/// When every account is spent there is nothing to poll for until one of them
/// resets, so the watcher waits for that instead of spending requests on a
/// wall it cannot get past. It still wakes at the normal interval at the
/// latest, in case a limit lifts earlier than the provider said.
fn wait_after(outcomes: &[TickOutcome], engine: &Engine, interval: Duration) -> Duration {
    if !engine.config().watch.wait_for_reset {
        return interval;
    }
    let now = Timestamp::now();
    let relief = outcomes
        .iter()
        .filter_map(|outcome| match &outcome.decision {
            Decision::Blocked(Blocked::AllExhausted { relief_at, .. }) => *relief_at,
            _ => None,
        })
        .min();

    match relief {
        // Every provider that is stuck knows when it recovers: sleep until the
        // first one does, plus a moment so the reset has certainly landed.
        Some(at)
            if outcomes
                .iter()
                .all(|o| matches!(o.decision, Decision::Blocked(_))) =>
        {
            let seconds = (at.as_second() - now.as_second()).max(0) as u64 + 15;
            Duration::from_secs(seconds).max(interval)
        }
        _ => interval,
    }
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
        Decision::Stay(Stay::NotWorthARestart { used }) => format!(
            "{used:.0}% used, and so is every other account — staying put rather than \
             restarting your sessions to pick the least busy one"
        ),
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
                Reason::WeekExpiresSooner { target_used } => format!(
                    "{to}'s weekly allowance resets sooner, so spending it first wastes \
                     nothing; it is at {target_used:.0}% and switching interrupts nothing"
                ),
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

    #[test]
    fn a_stuck_watcher_waits_for_the_next_reset() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("data")).unwrap();
        let engine = Engine::with_store(store).unwrap();
        let interval = Duration::from_secs(300);

        let stuck = |relief_in: i64| {
            vec![TickOutcome {
                provider: ProviderKind::Claude,
                decision: Decision::Blocked(Blocked::AllExhausted {
                    relief_at: Some(Timestamp::now() + jiff::SignedDuration::from_secs(relief_in)),
                    relief_account: Some("claude-2".into()),
                }),
                switched: None,
                held: None,
            }]
        };

        // A reset an hour away is worth sleeping through.
        let waited = wait_after(&stuck(3600), &engine, interval);
        assert!(
            waited > Duration::from_secs(3500) && waited < Duration::from_secs(3700),
            "{waited:?}"
        );

        // A reset sooner than the poll interval must not shorten it.
        assert_eq!(wait_after(&stuck(10), &engine, interval), interval);

        // Nothing stuck: the normal interval applies.
        let fine = vec![TickOutcome {
            provider: ProviderKind::Claude,
            decision: Decision::Stay(Stay::BelowThreshold { used: 10.0 }),
            switched: None,
            held: None,
        }];
        assert_eq!(wait_after(&fine, &engine, interval), interval);
    }
}
