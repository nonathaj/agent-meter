//! agent-meter in the Herdr desktop app: a mod that shows the account in use
//! under the sidebar's footer, every account in a modal, and switches as
//! notices.
//!
//! The mod ships inside this binary and is written into the app's home, so
//! the mod and the agent-meter it runs always come from one release. It reads
//! everything from `agent-meter list --json`, `history --json` and
//! `config show --json`, and changes things through the same commands a
//! person would type.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::fsutil::{self, Mode};

/// The mod's folder name under `mods/`, and so its name in Herdr.
const MOD: &str = "agent-meter";

/// The mod's files, as they are in `integrations/herdr-app/agent-meter/`.
const FILES: &[(&str, &str)] = &[
    (
        "main.js",
        include_str!("../integrations/herdr-app/agent-meter/main.js"),
    ),
    (
        "mod.json",
        include_str!("../integrations/herdr-app/agent-meter/mod.json"),
    ),
    (
        "chevron-left.svg",
        include_str!("../integrations/herdr-app/agent-meter/chevron-left.svg"),
    ),
    (
        "minus.svg",
        include_str!("../integrations/herdr-app/agent-meter/minus.svg"),
    ),
    (
        "plus.svg",
        include_str!("../integrations/herdr-app/agent-meter/plus.svg"),
    ),
    (
        "settings.svg",
        include_str!("../integrations/herdr-app/agent-meter/settings.svg"),
    ),
    (
        "ICONS.md",
        include_str!("../integrations/herdr-app/agent-meter/ICONS.md"),
    ),
];

/// The line in `main.js` naming the agent-meter it runs, which an install
/// points at the binary doing the installing.
const PROGRAM_LINE: &str = r#"const AGENT_METER = "agent-meter";"#;

/// The footer of Herdr's own layout, and the column an install puts it in.
const FOOTER: &str = "sidebar-footer";
const BOTTOM: &str = "sidebar-bottom";
const ROWS: &str = "accounts";

#[derive(Args, Debug)]
pub struct HerdrArgs {
    #[command(subcommand)]
    command: HerdrCommand,
}

#[derive(Subcommand, Debug)]
enum HerdrCommand {
    /// Put agent-meter in the Herdr app: the account in use under the
    /// sidebar's footer, every account a click away
    ///
    /// Herdr's own layouts can't be changed, so where the one shown is
    /// Herdr's, the footer rows need a personal copy of it, which no longer
    /// receives Herdr's updates. This asks before making one.
    Install {
        /// Install the mod only, leaving every layout as it is. It opens with
        /// `mod.open agent-meter` and from its notices.
        #[arg(long)]
        no_footer: bool,
        /// Make a personal copy of a layout of Herdr's without asking
        #[arg(short, long)]
        yes: bool,
    },
    /// Bring an installed mod up to date with this agent-meter
    ///
    /// The mod runs this itself, so after `agent-meter update` Herdr is on the
    /// matching mod within minutes. Layouts are left as they are, and a mod
    /// that is not installed stays that way.
    Refresh,
    /// Take agent-meter out of the Herdr app and its rows out of every layout
    Uninstall,
}

pub fn run(args: &HerdrArgs) -> Result<ExitCode> {
    let home = app_home()?;
    match &args.command {
        HerdrCommand::Install { no_footer, yes } => install(&home, *no_footer, *yes),
        HerdrCommand::Refresh => refresh(&home),
        HerdrCommand::Uninstall => uninstall(&home),
    }
}

/// Where the Herdr app keeps its files: `HERDR_APP_HOME`, else
/// `~/.config/herdr-app` on every system.
fn app_home() -> Result<PathBuf> {
    let home = match std::env::var_os("HERDR_APP_HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => dirs::home_dir()
            .context("cannot find the home folder")?
            .join(".config")
            .join("herdr-app"),
    };
    if !home.join("layouts").is_dir() {
        bail!(
            "the Herdr app is not set up at {}. Start it once, or point HERDR_APP_HOME at its home",
            home.display()
        );
    }
    Ok(home)
}

fn install(home: &Path, no_footer: bool, yes: bool) -> Result<ExitCode> {
    let program = std::env::current_exe().context("finding this agent-meter")?;
    write_mod(home, &program)?;
    println!(
        "Installed the agent-meter mod in {}",
        home.join("mods").join(MOD).display()
    );
    if no_footer {
        println!("Open it from Herdr's palette, or with `herdr-app do mod.open '{{\"name\":\"{MOD}\"}}'`.");
        return Ok(ExitCode::SUCCESS);
    }

    let shown = shown_layout(home);
    let layout = if is_herdrs(home, &shown)? {
        let copy = free_name(home, &format!("{shown}-agent-meter"));
        println!(
            "The layout shown, {shown}, is Herdr's own, and can't be changed. To put the account in \
             use under its footer, agent-meter needs a personal copy of it, {copy}. That copy no \
             longer receives Herdr's updates to {shown}."
        );
        if !yes && !ask(&format!("Make {copy} and switch to it? [y/N] "))? {
            println!(
                "The footer is as it was. Open the accounts from Herdr's palette, or run this \
                 again with a personal layout shown."
            );
            return Ok(ExitCode::SUCCESS);
        }
        herdr_app(&[
            "do",
            "layout.clone",
            &json!({"name": shown, "as": copy}).to_string(),
        ])
        .context("asking Herdr for a personal copy of the layout; is the Herdr app running?")?;
        copy
    } else {
        shown
    };

    let path = home.join("layouts").join(format!("{layout}.json"));
    let mut tree = read_layout(&path)?;
    match add_rows(&mut tree) {
        Placed::Added => {
            write_layout(&path, &tree)?;
            println!("The account in use now shows under the footer of {layout}.");
        }
        Placed::Already => println!("{layout} already shows the account in use under its footer."),
        Placed::NoFooter => println!(
            "{layout} has no `{FOOTER}` block, so the footer is as it was. Open the accounts from \
             Herdr's palette, or place a block with {{\"mod\": \"{MOD}\", \"props\": {{\"compact\": true}}}} \
             where you want the rows."
        ),
    }
    Ok(ExitCode::SUCCESS)
}

/// Rewrites the installed mod where it differs from the one in this binary.
///
/// The mod is a copy, and `agent-meter update` replaces only the binary: left
/// alone, the old mod would go on reading the new agent-meter's JSON. Only the
/// mod's files are compared, so this costs nothing when they match, and Herdr
/// reloads the mod only when one changed.
fn refresh(home: &Path) -> Result<ExitCode> {
    let folder = home.join("mods").join(MOD);
    if !folder.is_dir() {
        println!("The agent-meter mod is not installed; `agent-meter herdr install` puts it in.");
        return Ok(ExitCode::SUCCESS);
    }
    let program = std::env::current_exe().context("finding this agent-meter")?;
    if write_mod(home, &program)? {
        println!("Brought the agent-meter mod in {} up to date.", folder.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn uninstall(home: &Path) -> Result<ExitCode> {
    let layouts = home.join("layouts");
    for entry in std::fs::read_dir(&layouts).with_context(|| format!("reading {}", layouts.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if is_herdrs(home, &name)? {
            continue;
        }
        let Ok(mut tree) = read_layout(&path) else {
            continue;
        };
        if remove_rows(&mut tree) {
            write_layout(&path, &tree)?;
            println!("Took the account rows out of {name}.");
        }
    }
    let folder = home.join("mods").join(MOD);
    if folder.exists() {
        std::fs::remove_dir_all(&folder).with_context(|| format!("removing {}", folder.display()))?;
        println!("Removed {}", folder.display());
    } else {
        println!("The agent-meter mod was not installed.");
    }
    Ok(ExitCode::SUCCESS)
}

/// Writes the mod's files, pointing it at `program`, and says whether any
/// file changed.
fn write_mod(home: &Path, program: &Path) -> Result<bool> {
    let folder = home.join("mods").join(MOD);
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let mut changed = false;
    for (name, text) in FILES {
        let text = if *name == "main.js" {
            point_at(text, program)?
        } else {
            (*text).to_string()
        };
        let path = folder.join(name);
        // Writing a file the mod already has reloads it in Herdr for nothing.
        if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            continue;
        }
        fsutil::write_atomic(&path, text.as_bytes(), Mode::InheritExisting)
            .with_context(|| format!("writing {}", path.display()))?;
        changed = true;
    }
    Ok(changed)
}

/// `main.js` running `program` rather than whichever agent-meter is first on
/// the app's PATH, which on some systems has none.
fn point_at(main: &str, program: &Path) -> Result<String> {
    if !main.contains(PROGRAM_LINE) {
        bail!("the mod's main.js no longer names its agent-meter as {PROGRAM_LINE}");
    }
    let program = program
        .to_str()
        .context("this agent-meter's path is not valid text")?;
    Ok(main.replacen(
        PROGRAM_LINE,
        &format!("const AGENT_METER = {};", Value::from(program)),
        1,
    ))
}

/// The layout the app shows now, as its shared state has it.
fn shown_layout(home: &Path) -> String {
    std::fs::read(home.join("shared.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|shared| shared.get("layout")?.as_str().map(str::to_string))
        .unwrap_or_else(|| "main".into())
}

/// Whether `layout` is one Herdr ships, and so replaces whole on every start.
fn is_herdrs(home: &Path, layout: &str) -> Result<bool> {
    let installed = std::fs::read(home.join("installed.json")).unwrap_or_default();
    let installed: Value = serde_json::from_slice(&installed).unwrap_or(Value::Null);
    Ok(installed.get(format!("layout:{layout}")).is_some())
}

/// `wanted`, or `wanted-2` and so on where a layout has that name.
fn free_name(home: &Path, wanted: &str) -> String {
    let taken = |name: &str| home.join("layouts").join(format!("{name}.json")).exists();
    if !taken(wanted) {
        return wanted.to_string();
    }
    (2..)
        .map(|n| format!("{wanted}-{n}"))
        .find(|name| !taken(name))
        .expect("some number is free")
}

fn read_layout(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("{} is not valid JSON", path.display()))
}

fn write_layout(path: &Path, tree: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(tree)?;
    text.push('\n');
    fsutil::write_atomic(path, text.as_bytes(), Mode::InheritExisting)
        .with_context(|| format!("writing {}", path.display()))
}

#[derive(Debug, PartialEq)]
enum Placed {
    Added,
    Already,
    NoFooter,
}

/// Puts the footer and the account rows in one column where the footer was.
///
/// The column has no divider, so the rows read as part of the footer's pane.
/// The mod sizes it to its rows.
fn add_rows(tree: &mut Value) -> Placed {
    if find(tree, ROWS).is_some() {
        return Placed::Already;
    }
    let Some(footer) = find_mut(tree, FOOTER) else {
        return Placed::NoFooter;
    };
    let height = footer.pointer("/size/px").and_then(Value::as_f64).unwrap_or(69.0);
    let mut column = json!({
        "id": BOTTOM,
        "type": "column",
        "size": { "px": height },
        "resizable": false,
        "children": [
            footer.clone(),
            {
                "id": ROWS,
                "type": "block",
                "size": { "px": 0 },
                "content": { "mod": MOD, "props": { "compact": true } },
            },
        ],
    });
    std::mem::swap(footer, &mut column);
    Placed::Added
}

/// Undoes [`add_rows`]: the footer goes back where the column was.
fn remove_rows(tree: &mut Value) -> bool {
    let Some(bottom) = find_mut(tree, BOTTOM) else {
        return false;
    };
    let footer = bottom
        .get("children")
        .and_then(Value::as_array)
        .and_then(|children| {
            children
                .iter()
                .find(|c| c.get("id").and_then(Value::as_str) == Some(FOOTER))
        })
        .cloned();
    match footer {
        Some(footer) => {
            *bottom = footer;
            true
        }
        None => false,
    }
}

fn find<'a>(node: &'a Value, id: &str) -> Option<&'a Value> {
    if node.get("id").and_then(Value::as_str) == Some(id) {
        return Some(node);
    }
    node.get("children")?
        .as_array()?
        .iter()
        .find_map(|child| find(child, id))
}

fn find_mut<'a>(node: &'a mut Value, id: &str) -> Option<&'a mut Value> {
    if node.get("id").and_then(Value::as_str) == Some(id) {
        return Some(node);
    }
    node.get_mut("children")?
        .as_array_mut()?
        .iter_mut()
        .find_map(|child| find_mut(child, id))
}

/// Runs the Herdr app's command line.
fn herdr_app(args: &[&str]) -> Result<()> {
    let program = which::which("herdr-app").context("herdr-app is not on PATH")?;
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .context("running herdr-app")?;
    if !output.status.success() {
        bail!(
            "herdr-app {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn ask(prompt: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("{prompt}needs an answer; pass --yes to make the copy without asking");
    }
    print!("{prompt}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Value {
        json!({
            "id": "root",
            "type": "row",
            "children": [{
                "id": "sidebar",
                "type": "column",
                "children": [
                    { "id": "spaces", "type": "block", "content": { "mod": "spaces" } },
                    { "id": FOOTER, "type": "block", "size": { "px": 69 },
                      "content": { "mod": "workspace-chrome", "props": { "footer": true } } },
                ],
            }],
        })
    }

    #[test]
    fn the_rows_go_under_the_footer_once_and_come_out_again() {
        let before = layout();
        let mut tree = before.clone();
        assert_eq!(add_rows(&mut tree), Placed::Added);
        let bottom = find(&tree, BOTTOM).expect("the column is where the footer was");
        assert_eq!(bottom["size"]["px"], 69.0);
        assert_eq!(bottom["children"][0], before["children"][0]["children"][1]);
        assert_eq!(bottom["children"][1]["content"]["mod"], MOD);
        assert_eq!(
            tree["children"][0]["children"][1]["id"], BOTTOM,
            "the footer keeps its place among its siblings"
        );

        assert_eq!(add_rows(&mut tree), Placed::Already);
        assert!(remove_rows(&mut tree));
        assert_eq!(tree, before);
        assert!(!remove_rows(&mut tree));
    }

    #[test]
    fn a_layout_without_the_footer_is_left_alone() {
        let mut tree = json!({ "id": "root", "type": "row", "children": [] });
        assert_eq!(add_rows(&mut tree), Placed::NoFooter);
    }

    #[test]
    fn the_mod_runs_the_agent_meter_that_installed_it() {
        let main = FILES.iter().find(|(name, _)| *name == "main.js").unwrap().1;
        let program = Path::new(r"C:\Users\ada\.local\bin\agent-meter.exe");
        let pointed = point_at(main, program).unwrap();
        assert!(pointed.contains(r#"const AGENT_METER = "C:\\Users\\ada\\.local\\bin\\agent-meter.exe";"#));
        assert!(!pointed.contains(PROGRAM_LINE));
    }

    #[test]
    fn install_writes_the_mod_and_uninstall_takes_it_away() {
        let home = tempfile::tempdir().unwrap();
        let layouts = home.path().join("layouts");
        std::fs::create_dir_all(&layouts).unwrap();
        std::fs::write(home.path().join("shared.json"), r#"{"layout":"mine"}"#).unwrap();
        std::fs::write(
            home.path().join("installed.json"),
            r#"{"layout:main":{"json":"x"}}"#,
        )
        .unwrap();
        std::fs::write(
            layouts.join("mine.json"),
            serde_json::to_string(&layout()).unwrap(),
        )
        .unwrap();

        assert_eq!(install(home.path(), false, false).unwrap(), ExitCode::SUCCESS);
        let folder = home.path().join("mods").join(MOD);
        for (name, _) in FILES {
            assert!(folder.join(name).is_file(), "{name} is written");
        }
        let tree = read_layout(&layouts.join("mine.json")).unwrap();
        assert!(
            find(&tree, ROWS).is_some(),
            "a personal layout is changed in place"
        );

        assert_eq!(uninstall(home.path()).unwrap(), ExitCode::SUCCESS);
        assert!(!folder.exists());
        assert_eq!(read_layout(&layouts.join("mine.json")).unwrap(), layout());
    }

    /// After an update the installed mod is an older release's; refreshing
    /// brings it level with this binary and touches nothing else.
    #[test]
    fn refresh_brings_an_installed_mod_up_to_date_and_installs_nothing() {
        let home = tempfile::tempdir().unwrap();
        let layouts = home.path().join("layouts");
        std::fs::create_dir_all(&layouts).unwrap();
        std::fs::write(
            layouts.join("mine.json"),
            serde_json::to_string(&layout()).unwrap(),
        )
        .unwrap();
        let folder = home.path().join("mods").join(MOD);

        // Not installed: it stays that way.
        assert_eq!(refresh(home.path()).unwrap(), ExitCode::SUCCESS);
        assert!(!folder.exists());

        let program = std::env::current_exe().unwrap();
        assert!(
            write_mod(home.path(), &program).unwrap(),
            "a first write changes files"
        );
        assert!(
            !write_mod(home.path(), &program).unwrap(),
            "the same mod again changes nothing, so Herdr does not reload it"
        );

        // An older release's mod, and one of its files gone.
        std::fs::write(folder.join("main.js"), "// an older mod").unwrap();
        std::fs::remove_file(folder.join("plus.svg")).unwrap();
        assert_eq!(refresh(home.path()).unwrap(), ExitCode::SUCCESS);
        let main = std::fs::read_to_string(folder.join("main.js")).unwrap();
        assert_eq!(main, point_at(FILES[0].1, &program).unwrap());
        assert!(folder.join("plus.svg").is_file());
        assert_eq!(
            read_layout(&layouts.join("mine.json")).unwrap(),
            layout(),
            "layouts are install's business"
        );
    }
}
