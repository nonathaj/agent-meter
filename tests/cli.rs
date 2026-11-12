//! End-to-end tests that run the real binary against fixture agent homes.
//!
//! Nothing here touches the network or the developer's own agent configuration:
//! each test gets a temporary data directory and temporary `CLAUDE_CONFIG_DIR`
//! and `CODEX_HOME` directories holding credentials of the shape the real CLIs
//! write.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::{Value, json};

#[cfg(unix)]
#[test]
fn update_hands_off_to_the_installed_updater_without_opening_the_account_store() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let updater = dir.path().join("agent-meter-update");
    std::fs::write(&updater, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 7\n").unwrap();
    std::fs::set_permissions(&updater, std::fs::Permissions::from_mode(0o755)).unwrap();
    let blocked_store = dir.path().join("not-a-directory");
    std::fs::write(&blocked_store, "not an account store").unwrap();

    let latest = Command::cargo_bin("agent-meter")
        .unwrap()
        .arg("update")
        .env("PATH", dir.path())
        .env("AGENT_METER_DIR", dir.path().join("not-a-directory"))
        .output()
        .unwrap();
    assert_eq!(latest.status.code(), Some(7));
    assert_eq!(String::from_utf8(latest.stdout).unwrap(), "\n");

    let output = Command::cargo_bin("agent-meter")
        .unwrap()
        .args(["update", "--tag", "v1.2.3", "--prerelease"])
        .env("PATH", dir.path())
        .env("AGENT_METER_DIR", blocked_store)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "--tag\nv1.2.3\n--prerelease\n"
    );

    let version = Command::cargo_bin("agent-meter")
        .unwrap()
        .args(["update", "--version", "0.1.2"])
        .env("PATH", dir.path())
        .env("AGENT_METER_DIR", dir.path().join("not-a-directory"))
        .output()
        .unwrap();
    assert_eq!(version.status.code(), Some(7));
    assert_eq!(String::from_utf8(version.stdout).unwrap(), "--version\n0.1.2\n");
}

#[cfg(unix)]
#[test]
fn update_explains_how_to_get_the_missing_release_updater() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::cargo_bin("agent-meter")
        .unwrap()
        .arg("update")
        .env("PATH", dir.path())
        .env("AGENT_METER_DIR", dir.path().join("not-a-directory"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("agent-meter-update was not found"), "{stderr}");
    assert!(stderr.contains("release installer"), "{stderr}");
}

/// Windows will not let the file a running program was started from be
/// written, and `update` is still running while the updater installs over it.
/// The stand-in updater is `cmd`, which, handed commands on its input, does
/// what the installer does: writes the new executable where the old one is.
#[cfg(windows)]
#[test]
fn update_can_replace_the_running_executable_on_windows() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("agent-meter.exe");
    std::fs::copy(assert_cmd::cargo::cargo_bin("agent-meter"), &exe).unwrap();
    let system = std::env::var_os("SystemRoot").expect("SystemRoot is set on Windows");
    std::fs::copy(
        Path::new(&system).join("System32").join("cmd.exe"),
        dir.path().join("agent-meter-update.exe"),
    )
    .unwrap();
    std::fs::write(dir.path().join("new.exe"), "the new release").unwrap();

    let update = |install: &str| {
        assert_cmd::Command::new(&exe)
            .arg("update")
            .current_dir(dir.path())
            .env("AGENT_METER_DIR", dir.path().join("not-a-directory"))
            .write_stdin(format!("{install}\r\nexit %errorlevel%\r\n"))
            .output()
            .unwrap()
    };

    let output = update("copy /y new.exe agent-meter.exe");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "the new release");

    // An install that fails before writing anything leaves agent-meter where
    // it was, still able to run.
    std::fs::copy(assert_cmd::cargo::cargo_bin("agent-meter"), &exe).unwrap();
    let output = update("exit 5");
    assert_eq!(output.status.code(), Some(5));
    assert!(
        exe.is_file(),
        "agent-meter must be put back after a failed install"
    );
    Command::new(&exe).arg("--version").assert().success();
}

/// A temporary machine: an agent-meter data directory plus one home per agent CLI.
struct Fixture {
    _dir: tempfile::TempDir,
    data: PathBuf,
    claude_home: PathBuf,
    codex_home: PathBuf,
}

impl Fixture {
    fn fleet_home(&self, provider: &str) -> PathBuf {
        match provider {
            "claude" => self.sign_in_claude("fleet@example.com", "fleet-user", "fleet"),
            "codex" => self.sign_in_codex("fleet@example.com", "fleet-user", "fleet"),
            _ => unreachable!(),
        }
        self.run(&["import", provider]);
        let source = if provider == "claude" {
            &self.claude_home
        } else {
            &self.codex_home
        };
        let home = self._dir.path().join(format!("fleet-{provider}"));
        std::fs::rename(source, &home).unwrap();
        std::fs::create_dir_all(source).unwrap();
        self.run(&[
            "fleet",
            "register",
            &format!("{provider}-1"),
            "--home",
            home.to_str().unwrap(),
        ]);
        home
    }
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Self {
            data: dir.path().join("data"),
            claude_home: dir.path().join("claude"),
            codex_home: dir.path().join("codex"),
            _dir: dir,
        };
        std::fs::create_dir_all(&fixture.claude_home).unwrap();
        std::fs::create_dir_all(&fixture.codex_home).unwrap();
        fixture
    }

    /// Signs the fixture's Claude Code in to a seat in a named organisation.
    ///
    /// `org` is `None` for a credential whose organisation was never recorded.
    fn sign_in_claude_seat(&self, email: &str, uuid: &str, refresh: &str, org: Option<&str>) {
        self.sign_in_claude(email, uuid, refresh);
        let mut account = json!({"accountUuid": uuid, "emailAddress": email});
        if let Some(org) = org {
            account["organizationUuid"] = json!(org);
            account["organizationName"] = json!(org);
        }
        write_json(
            &self.claude_home.join(".claude.json"),
            &json!({"numStartups": 42, "oauthAccount": account}),
        );
    }

    /// Signs the fixture's Claude Code in to an account.
    fn sign_in_claude(&self, email: &str, uuid: &str, refresh: &str) {
        self.sign_in_claude_expiring(email, uuid, refresh, 1_900_000_000_000);
    }

    /// The same, with a chosen access-token expiry — which is what says which
    /// of two copies of one account was refreshed more recently.
    fn sign_in_claude_expiring(&self, email: &str, uuid: &str, refresh: &str, expires_ms: i64) {
        write_json(
            &self.claude_home.join(".credentials.json"),
            &json!({
                "claudeAiOauth": {
                    "accessToken": format!("sk-ant-oat01-{refresh}"),
                    "refreshToken": format!("sk-ant-ort01-{refresh}"),
                    "expiresAt": expires_ms,
                    "scopes": ["user:inference", "user:profile"],
                    "subscriptionType": "max"
                },
                "mcpOAuth": {"machine": "scoped"}
            }),
        );
        write_json(
            &self.claude_home.join(".claude.json"),
            &json!({
                "numStartups": 42,
                "oauthAccount": {
                    "accountUuid": uuid,
                    "emailAddress": email,
                    "organizationUuid": "org-1",
                    "organizationName": "Example Inc"
                }
            }),
        );
    }

    /// Signs the fixture's Codex in to an account.
    fn sign_in_codex(&self, email: &str, user: &str, refresh: &str) {
        write_json(
            &self.codex_home.join("auth.json"),
            &json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": id_token(email, user),
                    "access_token": format!("access-{refresh}"),
                    "refresh_token": format!("refresh-{refresh}"),
                    "account_id": "chatgpt-acct-1"
                },
                "last_refresh": "2026-09-09T02:30:16.972224500Z"
            }),
        );
    }

    fn claude_credentials(&self) -> Value {
        read_json(&self.claude_home.join(".credentials.json"))
    }

    fn claude_settings(&self) -> Value {
        read_json(&self.claude_home.join(".claude.json"))
    }

    fn codex_auth(&self) -> Value {
        read_json(&self.codex_home.join("auth.json"))
    }

    /// The binary under test, pointed at this fixture.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("agent-meter").unwrap();
        command
            .args(args)
            .env("AGENT_METER_DIR", &self.data)
            .env("CLAUDE_CONFIG_DIR", &self.claude_home)
            .env("CODEX_HOME", &self.codex_home);
        // Tests must never reach a provider: fixture tokens are not real.
        command.env("AGENT_METER_OFFLINE", "1");
        // Never let a developer's real credentials leak into a test run.
        for var in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"] {
            command.env_remove(var);
        }
        // The suite may be run from inside an agent session. Its markers say
        // "a CLI is running you", which is not true of the binary under test,
        // and leaving them set would have a test inherit an answer about its
        // environment that its fixture did not choose.
        for var in [
            "CLAUDECODE",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_EXECPATH",
            "CODEX_THREAD_ID",
            "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
            "AI_AGENT",
        ] {
            command.env_remove(var);
        }
        command
    }

    /// Runs a command and returns its stdout, asserting that it succeeded.
    fn run(&self, args: &[&str]) -> String {
        let output = self.cmd(args).output().unwrap();
        assert!(
            output.status.success(),
            "`agent-meter {}` failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Runs `list --json` and returns the accounts.
    fn accounts(&self) -> Vec<Value> {
        serde_json::from_str(&self.run(&["list", "--json"])).unwrap()
    }
}

/// An unsigned JWT of the shape Codex stores, carrying the account's identity.
fn id_token(email: &str, user: &str) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = json!({
        "email": email,
        "exp": 1_900_000_000,
        "https://api.openai.com/auth": {
            "chatgpt_user_id": user,
            "chatgpt_account_id": "chatgpt-acct-1",
            "chatgpt_plan_type": "pro"
        }
    });
    format!(
        "{}.{}.sig",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

/// Asserts that `reported` names the same directory as `expected`.
///
/// A resolved path is not the path that was handed in. macOS keeps the temp
/// directory behind `/private`, and Windows may have been given a short name
/// like `RUNNER~1`; resolving those is the point of resolving them. What is
/// worth pinning is that the path names the fleet home — and that it is
/// written the way other tools write it, since a verbatim `\\?\C:\...` names
/// the right directory while matching nothing the user ever typed.
fn names_the_same_directory(reported: &str, expected: &Path) {
    assert!(
        !reported.starts_with(r"\\?\"),
        "{reported} is not a path anything else will match"
    );
    let resolve = |path: &Path| {
        std::fs::canonicalize(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    };
    assert_eq!(resolve(Path::new(reported)), resolve(expected));
}

fn write_json(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn an_empty_store_explains_how_to_add_an_account() {
    let fixture = Fixture::new();
    fixture
        .cmd(&["list"])
        .assert()
        .success()
        .stdout(contains("No accounts yet").and(contains("agent-meter import")));
}

#[test]
fn imports_from_every_signed_in_cli() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "one");
    fixture.sign_in_codex("dev@openai.example", "user-1", "one");

    fixture
        .cmd(&["import"])
        .assert()
        .success()
        .stdout(contains("Imported claude-1").and(contains("Imported codex-1")));

    let accounts = fixture.accounts();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0]["id"], "claude-1");
    assert_eq!(accounts[0]["email"], "dev@example.com");
    assert_eq!(accounts[0]["organization"], "Example Inc");
    // Both CLIs are signed in to the account just imported.
    assert_eq!(accounts[0]["active"], true);
    assert_eq!(accounts[1]["id"], "codex-1");
    assert_eq!(accounts[1]["email"], "dev@openai.example");
    assert_eq!(accounts[1]["plan"], "pro");
}

#[test]
fn importing_the_same_account_again_updates_it_rather_than_duplicating() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);

    // The CLI refreshed its token; the same account signs in again.
    fixture.sign_in_claude("dev@example.com", "uuid-1", "two");
    fixture
        .cmd(&["import", "claude"])
        .assert()
        .success()
        .stdout(contains("Updated claude-1"));

    let accounts = fixture.accounts();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["id"], "claude-1");
}

/// Two tools can hold the same account with different credentials, and only
/// one of them still works: refreshing rotates a single-use token, so the copy
/// refreshed last is live and the other was spent the moment it was replaced.
/// Importing from a store nobody has opened in a week must not hand the
/// account its spent copy and then report it as signed out.
#[test]
fn importing_an_older_store_keeps_the_credential_that_still_works() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "live");
    fixture.run(&["import", "claude"]);
    // Another account takes the CLI, so the one under test is merely stored.
    // The account actually signed in is a case of its own: whatever its CLI
    // holds is what that agent is using, however old.
    fixture.sign_in_claude("other@example.com", "uuid-2", "other");
    fixture.run(&["import", "claude"]);

    let store = fixture.data.parent().unwrap().join("gemctl");
    std::fs::create_dir_all(&store).unwrap();
    let path = store.join("claude-1.json");
    let dir = store.to_string_lossy().into_owned();
    let record = |refresh: &str, expires: i64| {
        json!({
            "schemaVersion": 1, "agent": "claude", "name": "claude-1",
            "email": "dev@example.com", "orgId": "org-1",
            "expiresAt": expires,
            "tokens": {"access_token": "a", "refresh_token": refresh}
        })
    };
    let stored = || read_json(&fixture.data.join("accounts").join("claude-1.json"));

    // Their copy is the older one, so ours is the one that still works.
    write_json(&path, &record("spent", 1_700_000_000));
    fixture.run(&["import", "--from", "gemctl", "--dir", &dir]);
    assert_eq!(
        stored()["credential"]["refresh_token"],
        "sk-ant-ort01-live",
        "a spent copy replaced the live one: {}",
        stored()
    );
    assert!(stored()["needs_login"].is_null(), "{}", stored());
    // What sits beside the credential describes it, so a copy that lost did
    // not replace that either.
    let scopes = || stored()["provider_data"]["oauthExtras"]["scopes"].clone();
    assert_eq!(
        scopes(),
        json!(["user:inference", "user:profile"]),
        "{}",
        stored()
    );

    // Their copy is the newer one, so it is taken.
    write_json(&path, &record("refreshed-elsewhere", 2_000_000_000));
    fixture.run(&["import", "--from", "gemctl", "--dir", &dir]);
    assert_eq!(
        stored()["credential"]["refresh_token"],
        "refreshed-elsewhere",
        "{}",
        stored()
    );
    // gemctl keeps no scopes; taking its token does not erase ours, nor the
    // parts of Claude Code's account record gemctl never kept.
    assert_eq!(
        scopes(),
        json!(["user:inference", "user:profile"]),
        "{}",
        stored()
    );
    assert_eq!(
        stored()["provider_data"]["oauthAccount"]["accountUuid"],
        "uuid-1",
        "{}",
        stored()
    );
}

/// gemctl keeps only the tokens. Claude Code decides whether it is signed in
/// by the `scopes` beside them, so an account taken from gemctl and switched to
/// as it came left every session saying "Not logged in · Please run /login" —
/// while `list`, reading usage with the very same token, called it healthy.
#[test]
fn an_account_imported_from_gemctl_signs_claude_code_in() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("other@example.com", "uuid-2", "other");
    fixture.run(&["import", "claude"]);

    let store = fixture.data.parent().unwrap().join("gemctl");
    std::fs::create_dir_all(&store).unwrap();
    write_json(
        &store.join("claude-1.json"),
        &json!({
            "schemaVersion": 1, "agent": "claude", "name": "claude-1",
            "accountId": "uuid-1", "email": "dev@example.com", "orgId": "org-9",
            "expiresAt": 1_900_000_000,
            "tokens": {"access_token": "sk-ant-oat01-gem", "refresh_token": "sk-ant-ort01-gem"}
        }),
    );
    fixture.run(&["import", "--from", "gemctl", "--dir", &store.to_string_lossy()]);
    fixture.run(&["use", "dev@example.com"]);

    let oauth = &fixture.claude_credentials()["claudeAiOauth"];
    assert_eq!(oauth["refreshToken"], "sk-ant-ort01-gem");
    assert!(
        oauth["scopes"]
            .as_array()
            .is_some_and(|scopes| scopes.contains(&json!("user:inference"))),
        "Claude Code treats a credential without user:inference as no login: {oauth}"
    );
}

/// Signing the account in use in again is how a person fixes a CLI that
/// says it is logged out. The CLI's copy is then the stale one, and adopting
/// it — as a copy the CLI rotated on its own would be — threw the new login
/// away on the next `list` and left the CLI exactly as broken as before.
#[test]
fn a_newer_login_for_the_account_in_use_reaches_the_cli() {
    let fixture = Fixture::new();
    fixture.sign_in_claude_expiring("dev@example.com", "uuid-1", "old", 1_800_000_000_000);
    fixture.run(&["import", "claude"]);

    // The same account arrives signed in afresh: here from gemctl, which takes
    // the same path as `agent-meter add` or a copy from another machine.
    let store = fixture.data.parent().unwrap().join("gemctl");
    std::fs::create_dir_all(&store).unwrap();
    write_json(
        &store.join("claude-1.json"),
        &json!({
            "schemaVersion": 1, "agent": "claude", "name": "claude-1",
            "email": "dev@example.com", "orgId": "org-1",
            "expiresAt": 1_900_000_000,
            "tokens": {"access_token": "sk-ant-oat01-new", "refresh_token": "sk-ant-ort01-new"}
        }),
    );
    fixture.run(&["import", "--from", "gemctl", "--dir", &store.to_string_lossy()]);
    fixture.run(&["list"]);

    let stored = read_json(&fixture.data.join("accounts").join("claude-1.json"));
    assert_eq!(
        stored["credential"]["refresh_token"], "sk-ant-ort01-new",
        "the new login was replaced by the CLI's stale copy: {stored}"
    );
    let oauth = &fixture.claude_credentials()["claudeAiOauth"];
    assert_eq!(oauth["refreshToken"], "sk-ant-ort01-new", "{oauth}");
    // gemctl keeps no scopes, which is not the same as the account having
    // none: the ones Claude Code recorded itself came through.
    assert_eq!(oauth["scopes"], json!(["user:inference", "user:profile"]));
    // Nothing else of the CLI's was disturbed.
    assert_eq!(fixture.claude_credentials()["mcpOAuth"]["machine"], "scoped");
    assert_eq!(fixture.claude_settings()["numStartups"], 42);
}

/// Points `here` at `there`, by running the binary directly instead of ssh.
///
/// This is the `exec` transport doing what it exists for: everything below the
/// choice of how to start the other agent-meter — the conversation, the
/// merge, the rules about which copy of a credential wins — is the same code
/// that runs over ssh.
fn link(here: &Fixture, there: &Fixture, name: &str) {
    let binary = assert_cmd::cargo::cargo_bin("agent-meter");
    let literal = |path: &std::path::Path| format!("'{}'", path.display());
    std::fs::create_dir_all(&here.data).unwrap();
    std::fs::write(
        here.data.join("config.toml"),
        format!(
            "[remote.{name}]\nexec = [{}, 'sync', '--serve']\n\n\
             [remote.{name}.env]\n\
             AGENT_METER_DIR = {}\n\
             CLAUDE_CONFIG_DIR = {}\n\
             CODEX_HOME = {}\n\
             AGENT_METER_OFFLINE = '1'\n",
            literal(&binary),
            literal(&there.data),
            literal(&there.claude_home),
            literal(&there.codex_home),
        ),
    )
    .unwrap();
}

/// Two machines, each with accounts the other has not got, ending up with
/// both — without either being asked to trust what it was sent over what it
/// already had.
#[test]
fn syncing_gives_each_machine_what_the_other_is_holding() {
    let desk = Fixture::new();
    let laptop = Fixture::new();
    desk.sign_in_claude("dev@example.com", "uuid-1", "desk");
    desk.run(&["import", "claude"]);
    laptop.sign_in_claude("other@example.com", "uuid-2", "laptop");
    laptop.run(&["import", "claude"]);
    link(&desk, &laptop, "laptop");

    // Nothing is written until it is asked for.
    let planned = desk.run(&["sync", "laptop", "--dry-run"]);
    // Both are `claude-1` on their own machine, so an id would name either.
    // The report names each by what it is everywhere.
    assert!(
        planned.contains("add     Claude Code dev@example.com (Example Inc) there"),
        "{planned}"
    );
    assert!(
        planned.contains("add     Claude Code other@example.com (Example Inc) here"),
        "{planned}"
    );
    assert!(!planned.contains("claude-1"), "{planned}");
    assert_eq!(laptop.accounts().len(), 1, "a dry run wrote to the other machine");
    assert_eq!(desk.accounts().len(), 1, "a dry run wrote here");

    let done = desk.run(&["sync", "laptop", "--yes"]);
    assert!(done.contains("laptop"), "{done}");

    // Both machines now hold both accounts, under their own names.
    fn emails(fixture: &Fixture) -> Vec<String> {
        fixture
            .accounts()
            .into_iter()
            .map(|account| account["email"].as_str().unwrap_or_default().to_string())
            .collect()
    }
    let here = emails(&desk);
    let there = emails(&laptop);
    assert!(here.iter().any(|email| email == "other@example.com"), "{here:?}");
    assert!(there.iter().any(|email| email == "dev@example.com"), "{there:?}");

    // Doing it again moves nothing: both sides are already current.
    let again = desk.run(&["sync", "laptop", "--dry-run"]);
    assert!(again.contains("already current"), "{again}");
    assert_eq!(desk.accounts().len(), 2);
    assert_eq!(laptop.accounts().len(), 2);
}

/// Claude Code refreshes as it works, and nothing may have looked since. What
/// a sync sends is the copy the CLI holds, not the one stored before it
/// rotated — that one is spent, and the other machine would be handed an
/// account that is already signed out.
#[test]
fn a_sync_sends_the_credential_the_cli_rotated_since_it_was_stored() {
    let desk = Fixture::new();
    let laptop = Fixture::new();
    desk.sign_in_claude_expiring("dev@example.com", "uuid-1", "stored", 1_800_000_000_000);
    desk.run(&["import", "claude"]);
    desk.sign_in_claude_expiring("dev@example.com", "uuid-1", "rotated", 1_900_000_000_000);
    link(&desk, &laptop, "laptop");

    desk.run(&["sync", "laptop", "--yes"]);

    let there = read_json(&laptop.data.join("accounts").join("claude-1.json"));
    assert_eq!(
        there["credential"]["refresh_token"], "sk-ant-ort01-rotated",
        "the other machine was sent the spent copy: {there}"
    );
}

/// The reason this cannot be a file copy. A refresh token is single-use, so
/// two machines hold two copies of one account and only the one refreshed
/// most recently works. Whichever direction the sync runs in, the live copy
/// is the one both machines end up with.
#[test]
fn a_sync_never_replaces_a_live_credential_with_a_spent_one() {
    for direction in ["push", "pull"] {
        let desk = Fixture::new();
        let laptop = Fixture::new();

        // The same account on both machines. The laptop refreshed it later,
        // which spent the copy the desk is holding.
        desk.sign_in_claude_expiring("dev@example.com", "uuid-1", "spent", 1_800_000_000_000);
        desk.run(&["import", "claude"]);
        laptop.sign_in_claude_expiring("dev@example.com", "uuid-1", "live", 1_900_000_000_000);
        laptop.run(&["import", "claude"]);
        link(&desk, &laptop, "laptop");

        desk.run(&["sync", "laptop", &format!("--{direction}"), "--yes"]);

        let token = |fixture: &Fixture| {
            read_json(&fixture.data.join("accounts").join("claude-1.json"))["credential"]["refresh_token"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // Neither machine took the spent copy, whichever way the accounts moved.
        assert_eq!(
            token(&laptop),
            "sk-ant-ort01-live",
            "--{direction} overwrote the live copy"
        );
        if direction == "pull" {
            assert_eq!(
                token(&desk),
                "sk-ant-ort01-live",
                "--pull did not fetch the live copy"
            );
        }
    }
}

/// A machine whose credential the provider has already rejected has nothing
/// worth sending: the only thing its copy could do is replace a working one.
#[test]
fn an_account_known_to_be_signed_out_is_not_pushed() {
    let desk = Fixture::new();
    let laptop = Fixture::new();
    desk.sign_in_claude("dev@example.com", "uuid-1", "desk");
    desk.run(&["import", "claude"]);
    link(&desk, &laptop, "laptop");

    // Mark it the way a rejected refresh does.
    let record = desk.data.join("accounts").join("claude-1.json");
    let mut stored: Value = read_json(&record);
    stored["needs_login"] = json!("the provider rejected its credential");
    write_json(&record, &stored);

    let planned = desk.run(&["sync", "laptop", "--push", "--dry-run"]);
    assert!(!planned.contains("add     claude-1"), "{planned}");
    desk.run(&["sync", "laptop", "--push", "--yes"]);
    assert!(laptop.accounts().is_empty(), "a dead credential was sent anyway");
}

/// A poll lets go of the store lock while it is on the network, so an account
/// removed while it ran has its reading written back after it is gone. A
/// reading holds a stranger's usage and their address, waiting under an id for
/// whoever is given that id next.
#[test]
fn a_poll_drops_readings_no_account_claims() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);

    // A reading left behind by an account that is no longer stored.
    let cache = fixture.data.join("usage.json");
    write_json(
        &cache,
        &json!({"entries": {"claude-9": {"error": "someone else's", "failures": 1}}}),
    );

    fixture.run(&["list", "--refresh"]);

    let entries = read_json(&cache);
    let entries = entries["entries"].as_object().unwrap();
    assert!(
        !entries.contains_key("claude-9"),
        "a reading outlived its account: {entries:?}"
    );
    // The account that is actually stored keeps its own.
    assert!(entries.contains_key("claude-1"), "{entries:?}");
}

#[test]
fn switching_replaces_the_live_credential_and_keeps_unrelated_settings() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("first@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);
    fixture.sign_in_claude("second@example.com", "uuid-2", "two");
    fixture.run(&["import", "claude"]);

    // Two accounts are stored, and the second is the one Claude Code is using.
    let accounts = fixture.accounts();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[1]["active"], true);

    fixture
        .cmd(&["use", "first@example.com"])
        .assert()
        .success()
        .stdout(contains("claude-2 -> claude-1"));

    // The live credential is the first account's again...
    let credentials = fixture.claude_credentials();
    assert_eq!(credentials["claudeAiOauth"]["refreshToken"], "sk-ant-ort01-one");
    assert_eq!(credentials["claudeAiOauth"]["accessToken"], "sk-ant-oat01-one");
    // ... the machine-scoped secrets stayed behind ...
    assert_eq!(credentials["mcpOAuth"]["machine"], "scoped");
    // ... the identity block followed the account ...
    let settings = fixture.claude_settings();
    assert_eq!(settings["oauthAccount"]["emailAddress"], "first@example.com");
    assert_eq!(settings["oauthAccount"]["accountUuid"], "uuid-1");
    // ... and unrelated Claude Code settings survived.
    assert_eq!(settings["numStartups"], 42);

    let accounts = fixture.accounts();
    assert_eq!(accounts[0]["active"], true);
    assert_eq!(accounts[1]["active"], false);
}

#[test]
fn switching_codex_warns_that_running_sessions_need_a_restart() {
    let fixture = Fixture::new();
    fixture.sign_in_codex("first@example.com", "user-1", "one");
    fixture.run(&["import", "codex"]);
    fixture.sign_in_codex("second@example.com", "user-2", "two");
    fixture.run(&["import", "codex"]);

    fixture
        .cmd(&["use", "codex-1"])
        .assert()
        .success()
        .stdout(contains("Restart any running Codex session"));

    let auth = fixture.codex_auth();
    assert_eq!(auth["tokens"]["refresh_token"], "refresh-one");
    assert_eq!(auth["auth_mode"], "chatgpt");
}

#[test]
fn a_credential_the_cli_rotated_is_adopted_rather_than_overwritten() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);

    // Claude Code refreshes on its own: same account, new tokens. Refresh
    // tokens are single-use, so agent-meter must take the new one.
    fixture.sign_in_claude("dev@example.com", "uuid-1", "rotated");
    fixture.run(&["list"]);

    let stored: Value = read_json(&fixture.data.join("accounts").join("claude-1.json"));
    assert_eq!(stored["credential"]["refresh_token"], "sk-ant-ort01-rotated");
}

/// Two agent-meter processes must not trip over each other. A long `watch`
/// used to hold the store lock through its network calls, so a `list` in
/// another terminal would sit there and then fail.
#[test]
fn a_second_process_can_read_while_another_is_working() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);

    let mut children: Vec<_> = (0..4)
        .map(|_| fixture.cmd(&["list", "--refresh"]).spawn().unwrap())
        .collect();
    for child in &mut children {
        let status = child.wait().unwrap();
        assert!(status.success(), "concurrent read failed: {status}");
    }
}

/// One address can hold a personal seat and a seat in a team. On Claude they
/// share a user id as well, so only the organisation tells them apart — and
/// treating them as one account overwrites one credential with the other's.
/// The overwritten one is gone, not hidden.
#[test]
fn a_second_seat_under_the_same_address_is_a_separate_account() {
    let fixture = Fixture::new();
    fixture.sign_in_claude_seat("dev@example.com", "same-person", "team", Some("team-org"));
    fixture.run(&["import", "claude"]);

    // The person signs in to their personal organisation: same address, same
    // user, different organisation, and a credential of its own.
    fixture.sign_in_claude_seat("dev@example.com", "same-person", "personal", Some("personal-org"));
    fixture.run(&["import", "claude"]);

    let accounts = fixture.accounts();
    assert_eq!(accounts.len(), 2, "two seats, two accounts: {accounts:#?}");
    assert_eq!(accounts[0]["organization"], "team-org");
    assert_eq!(accounts[1]["organization"], "personal-org");
    // Only the seat that is actually signed in is marked live.
    assert_eq!(accounts[0]["active"], false);
    assert_eq!(accounts[1]["active"], true);

    // The team credential still exists. This is the part the bug destroyed.
    let stored: Value = read_json(&fixture.data.join("accounts").join("claude-1.json"));
    assert_eq!(stored["credential"]["refresh_token"], "sk-ant-ort01-team");

    // Both rows show the same address, so the column that tells them apart has
    // to be there, and naming the address alone cannot choose between them.
    let table = fixture.run(&["list"]);
    assert!(
        table.contains("team-org") && table.contains("personal-org"),
        "{table}"
    );
    fixture
        .cmd(&["use", "dev@example.com"])
        .assert()
        .failure()
        .stderr(contains("matches several accounts").and(contains("claude-1")));
}

/// The same trap with the organisation missing rather than different: nothing
/// contradicts, the user id matches, and adopting on that alone would replace a
/// credential that belongs to the other seat.
#[test]
fn an_unstated_organization_never_adopts_another_seats_credential() {
    let fixture = Fixture::new();
    fixture.sign_in_claude_seat("dev@example.com", "same-person", "team", None);
    fixture.run(&["import", "claude"]);

    fixture.sign_in_claude_seat("dev@example.com", "same-person", "personal", Some("personal-org"));
    // `list` is enough: it reconciles what the CLI is signed in to.
    fixture.run(&["list"]);

    let stored: Value = read_json(&fixture.data.join("accounts").join("claude-1.json"));
    assert_eq!(
        stored["credential"]["refresh_token"], "sk-ant-ort01-team",
        "an unstated organisation must not confirm a match"
    );
    assert_eq!(fixture.accounts()[0]["active"], false);
}

/// The floor under the rule above: when the credential itself is the same, the
/// accounts are the same whatever the organisation says, so a `.claude.json`
/// that drifts cannot split one account into two rows.
#[test]
fn an_unchanged_credential_is_the_same_account_however_the_organization_reads() {
    let fixture = Fixture::new();
    fixture.sign_in_claude_seat("dev@example.com", "same-person", "one", Some("team-org"));
    fixture.run(&["import", "claude"]);

    // Same credential, but the recorded organisation changed underneath it.
    fixture.sign_in_claude_seat("dev@example.com", "same-person", "one", None);
    fixture
        .cmd(&["import", "claude"])
        .assert()
        .success()
        .stdout(contains("Updated claude-1"));
    assert_eq!(fixture.accounts().len(), 1);
}

#[test]
fn removing_an_account_forgets_it_without_touching_the_cli() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);

    fixture
        .cmd(&["remove", "claude-1", "--yes"])
        .assert()
        .success()
        .stdout(contains("Removed claude-1"));
    assert!(fixture.accounts().is_empty());
    // The CLI is still signed in; only agent-meter forgot the account.
    assert_eq!(
        fixture.claude_credentials()["claudeAiOauth"]["refreshToken"],
        "sk-ant-ort01-one"
    );

    fixture
        .cmd(&["remove", "claude-1", "--yes"])
        .assert()
        .failure()
        .stderr(contains("no account matches"));
}

#[test]
fn settings_round_trip_through_the_config_commands() {
    let fixture = Fixture::new();
    fixture.run(&["config", "set", "watch.threshold", "80"]);
    fixture.run(&["config", "set", "provider.codex.enabled", "false"]);

    let shown = fixture.run(&["config", "show"]);
    assert!(shown.contains("threshold = 80"), "{shown}");
    assert!(shown.contains("enabled = false"), "{shown}");

    fixture
        .cmd(&["config", "set", "watch.threshold", "900"])
        .assert()
        .failure()
        .stderr(contains("between 1 and 100"));

    let path = fixture.run(&["config", "path"]);
    assert!(path.trim().ends_with("config.toml"), "{path}");
}

#[test]
fn watch_reports_what_it_would_do_without_changing_anything() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);

    // With no usage reading (the network is not reachable in tests), the
    // watcher must hold rather than guess.
    let output = fixture.run(&["watch", "--once", "--dry-run"]);
    assert!(output.contains("Claude Code:"), "{output}");
    assert!(
        output.contains("usage unknown") || output.contains("staying put"),
        "{output}"
    );

    assert_eq!(
        fixture.claude_credentials()["claudeAiOauth"]["refreshToken"],
        "sk-ant-ort01-one"
    );
}

/// The account in use had its refresh token rejected (`invalid_grant`) while
/// its last reading was well under the threshold. The watcher judged it on that
/// reading, "stayed put", and left Claude Code signed out.
#[test]
fn watch_moves_claude_code_off_an_account_the_provider_signed_out() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("spare@example.com", "uuid-spare", "spare");
    fixture.run(&["import", "claude"]);
    fixture.sign_in_claude("dead@example.com", "uuid-dead", "dead");
    fixture.run(&["import", "claude"]);

    // Mark it the way a rejected refresh does.
    let record = fixture.data.join("accounts").join("claude-2.json");
    let mut stored: Value = read_json(&record);
    stored["needs_login"] = json!("the provider rejected its credential: invalid_grant");
    write_json(&record, &stored);

    let now = jiff::Timestamp::now();
    let reading = |used: f64| {
        json!({
            "observed_at": now,
            "windows": [{
                "window_secs": 18000,
                "used_percent": used,
                "resets_at": now + jiff::SignedDuration::from_hours(3),
            }],
            "limit_reached": false,
        })
    };
    write_json(
        &fixture.data.join("usage.json"),
        &json!({"entries": {
            "claude-1": {"usage": reading(10.0), "failures": 0},
            "claude-2": {"usage": reading(40.0), "failures": 0},
        }}),
    );
    // A switch moments ago does not hold it there: there is nothing to flap to.
    write_json(
        &fixture.data.join("state.json"),
        &json!({"last_switch": {"claude": {
            "at": now - jiff::SignedDuration::from_secs(60),
            "to": "claude-2",
        }}}),
    );

    let output = fixture.run(&["watch", "--once"]);
    assert!(output.contains("claude-2 -> claude-1"), "{output}");
    assert_eq!(
        fixture.claude_credentials()["claudeAiOauth"]["refreshToken"],
        "sk-ant-ort01-spare"
    );
}

/// agent-meter leaves renewing the account in use to its CLI, so the CLI's
/// renewal is what ends a run of failed polls. The backoff those failures set
/// must not hold the new credential out until it lapses.
#[test]
fn a_renewal_by_the_cli_ends_the_backoff_its_old_credential_earned() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "old");
    fixture.run(&["import", "claude"]);

    let now = jiff::Timestamp::now();
    let cache = fixture.data.join("usage.json");
    write_json(
        &cache,
        &json!({"entries": {"claude-1": {
            "error": "token expired",
            "failed_at": now,
            "retry_after": now + jiff::SignedDuration::from_mins(30),
            "failures": 3,
        }}}),
    );

    // Claude Code renews: a new refresh token, expiring later.
    fixture.sign_in_claude_expiring("dev@example.com", "uuid-1", "new", 1_950_000_000_000);
    fixture.run(&["watch", "--once", "--dry-run"]);

    // Polled again at once rather than half an hour later. Tests are offline,
    // so that poll's own failure is the evidence it happened.
    let entry = &read_json(&cache)["entries"]["claude-1"];
    assert_eq!(entry["failures"], 1, "{entry}");
    assert!(entry["error"].as_str().unwrap().contains("offline"), "{entry}");
    assert_eq!(
        fixture.accounts()[0]["id"],
        "claude-1",
        "the renewal is the same account, not a new one"
    );
}

#[test]
fn an_api_key_login_is_refused_with_an_explanation() {
    let fixture = Fixture::new();
    write_json(
        &fixture.codex_home.join("auth.json"),
        &json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-proj-abc", "tokens": null}),
    );
    fixture
        .cmd(&["import", "codex"])
        .assert()
        .failure()
        .stderr(contains("subscription").and(contains("agent-meter manages")));
}

#[test]
fn stored_credentials_are_not_world_readable() {
    let fixture = Fixture::new();
    fixture.sign_in_claude("dev@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);

    let path = fixture.data.join("accounts").join("claude-1.json");
    assert!(path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "account files must be readable by their owner only");
    }
}

/// Exporting writes into somebody else's store, so it merges: what that tool
/// holds and agent-meter does not is left exactly as it was.
#[test]
fn exporting_adds_accounts_without_taking_away_the_tools_own() {
    let fixture = Fixture::new();
    fixture.sign_in_claude_seat("dev@example.com", "uuid-1", "one", Some("team-org"));
    fixture.run(&["import", "claude"]);

    // A store that already holds an account of its own.
    let store = fixture.data.parent().unwrap().join("gemctl");
    std::fs::create_dir_all(&store).unwrap();
    write_json(
        &store.join("claude-9.json"),
        &json!({
            "schemaVersion": 1, "agent": "claude", "name": "claude-9",
            "email": "someone-else@example.com", "orgId": "other-org",
            "tokens": {"access_token": "a", "refresh_token": "theirs"}
        }),
    );

    let dir = store.to_string_lossy().into_owned();
    // Nothing is written until it is confirmed.
    let planned = fixture.run(&["export", "--to", "gemctl", "--dir", &dir, "--yes"]);
    assert!(planned.contains("add"), "{planned}");
    assert!(planned.contains("1 account(s) gemctl holds"), "{planned}");

    // Their account is untouched, ours is there.
    let theirs: Value = read_json(&store.join("claude-9.json"));
    assert_eq!(theirs["tokens"]["refresh_token"], "theirs");
    let ours: Value = read_json(&store.join("claude-1.json"));
    assert_eq!(ours["email"], "dev@example.com");
    assert_eq!(ours["tokens"]["refresh_token"], "sk-ant-ort01-one");
    assert_eq!(ours["orgId"], "team-org");

    // Exporting again updates in place rather than adding a second copy.
    let again = fixture.run(&["export", "--to", "gemctl", "--dir", &dir, "--yes"]);
    assert!(again.contains("update"), "{again}");
    let names: Vec<_> = std::fs::read_dir(&store)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(names.len(), 2, "no duplicate slot: {names:?}");
}

/// The other tool may be refreshing its own copy as it works, and a refresh
/// token is single-use. Exporting our older copy over its newer one would sign
/// the account out of that tool, so it is skipped and the person told how to
/// take theirs instead.
#[test]
fn an_export_never_replaces_a_newer_copy_the_other_tool_holds() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    for tool in ["gemctl", "cswap"] {
        let fixture = Fixture::new();
        fixture.sign_in_claude_expiring("dev@example.com", "uuid-1", "ours", 1_800_000_000_000);
        fixture.run(&["import", "claude"]);
        let store = fixture.data.parent().unwrap().join(tool);
        let dir = store.to_string_lossy().into_owned();
        fixture.run(&["export", "--to", tool, "--dir", &dir, "--yes"]);

        // The tool refreshes its copy after the export.
        let (path, rewrite): (PathBuf, fn(&Path)) = if tool == "gemctl" {
            (store.join("claude-1.json"), |path| {
                let mut record = read_json(path);
                record["tokens"]["refresh_token"] = json!("sk-ant-ort01-theirs");
                record["expiresAt"] = json!(1_900_000_000);
                write_json(path, &record);
            })
        } else {
            let credentials = store.join("credentials");
            let file = std::fs::read_dir(&credentials).unwrap().next().unwrap().unwrap();
            (file.path(), |path| {
                let decoded = STANDARD.decode(std::fs::read(path).unwrap()).unwrap();
                let mut blob: Value = serde_json::from_slice(&decoded).unwrap();
                blob["claudeAiOauth"]["refreshToken"] = json!("sk-ant-ort01-theirs");
                blob["claudeAiOauth"]["expiresAt"] = json!(1_900_000_000_000i64);
                std::fs::write(path, STANDARD.encode(serde_json::to_vec(&blob).unwrap())).unwrap();
            })
        };
        rewrite(&path);
        let before = std::fs::read(&path).unwrap();

        let output = fixture.run(&["export", "--to", tool, "--dir", &dir, "--yes"]);
        assert!(output.contains("holds a newer copy"), "{tool}: {output}");
        assert!(
            output.contains(&format!("import --from {tool}")),
            "{tool}: {output}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{tool}: our older copy was written over theirs"
        );
    }
}

/// `personal` is our word for an organisation named after the address beside
/// it — a column's shorthand, not the organisation's name. Writing it into
/// another tool's store would rename the account on that tool's own screens.
#[test]
fn an_export_writes_the_name_the_provider_gave_not_our_shorthand() {
    let fixture = Fixture::new();
    fixture.sign_in_claude_seat(
        "dev@example.com",
        "uuid-1",
        "one",
        Some("dev@example.com's Organization"),
    );
    fixture.run(&["import", "claude"]);

    // Our own table shortens it.
    let listed = fixture.run(&["list"]);
    assert!(listed.contains("personal"), "{listed}");

    // An earlier version wrote the shorthand into the record itself. The
    // provider's own name survives beside the credential, so an export made
    // from such a record still says what the provider said.
    let record = fixture.data.join("accounts").join("claude-1.json");
    let mut stored: Value = read_json(&record);
    stored["identity"]["workspace_name"] = json!("personal");
    write_json(&record, &stored);

    let root = fixture.data.parent().unwrap().to_path_buf();
    for tool in ["cswap", "gemctl"] {
        let store = root.join(tool);
        let dir = store.to_string_lossy().into_owned();
        fixture.run(&["export", "--to", tool, "--dir", &dir, "--yes"]);

        let written = std::fs::read_dir(&store)
            .unwrap()
            .flat_map(|entry| {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    std::fs::read_dir(&path)
                        .unwrap()
                        .map(|e| e.unwrap().path())
                        .collect()
                } else {
                    vec![path]
                }
            })
            .map(|path| std::fs::read_to_string(&path).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            written.contains("dev@example.com's Organization"),
            "{tool} should carry the provider's name: {written}"
        );
        assert!(
            !written.contains("\"personal\""),
            "{tool} should not carry our shorthand: {written}"
        );
    }
}

/// What agent-meter writes, agent-meter can read: the shapes have to match the
/// tools' own, and a round trip is the cheapest proof that they do.
#[test]
fn accounts_survive_a_round_trip_through_each_tool() {
    for (tool, seats) in [("cswap", 2), ("gemctl", 2)] {
        let fixture = Fixture::new();
        fixture.sign_in_claude_seat("dev@example.com", "uuid-1", "one", Some("team-org"));
        fixture.run(&["import", "claude"]);
        fixture.sign_in_claude_seat("dev@example.com", "uuid-1", "two", Some("personal-org"));
        fixture.run(&["import", "claude"]);
        assert_eq!(fixture.accounts().len(), seats);

        let dir = fixture
            .data
            .parent()
            .unwrap()
            .join(tool)
            .to_string_lossy()
            .into_owned();
        fixture.run(&["export", "--to", tool, "--dir", &dir, "--yes"]);

        // A second machine, importing what the first exported.
        let other = Fixture::new();
        other.run(&["import", "--from", tool, "--dir", &dir]);

        let mut got: Vec<(String, String)> = other
            .accounts()
            .iter()
            .map(|a| {
                (
                    a["email"].as_str().unwrap_or_default().to_string(),
                    a["organization"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        got.sort();
        assert_eq!(got.len(), seats, "{tool}: {got:?}");
        // Two seats under one address stay two accounts through the trip.
        assert_eq!(got[0].0, "dev@example.com");
        assert_ne!(got[0].1, got[1].1, "{tool}: the organisations tell them apart");
    }
}

#[test]
fn fleet_dry_run_is_read_only_and_reports_transcript_roots() {
    let f = Fixture::new();
    let home = f.fleet_home("claude");
    let plan: Value = serde_json::from_str(&f.run(&[
        "run",
        "--provider",
        "claude",
        "--account",
        "claude-1",
        "--scope",
        "city-a",
        "--session",
        "worker-1",
        "--dry-run",
        "--",
        "--resume",
        "conversation-1",
    ]))
    .unwrap();
    names_the_same_directory(plan["home"].as_str().unwrap(), &home);
    // The transcript root is stated relative to the home the plan named, so it
    // is checked against that rather than against the path handed in.
    assert_eq!(
        PathBuf::from(plan["transcript_roots"][0].as_str().unwrap()),
        PathBuf::from(plan["home"].as_str().unwrap()).join("projects")
    );
    let state = read_json(&f.data.join("fleet.json"));
    assert_eq!(state["bindings"].as_array().unwrap().len(), 0);
    assert!(!plan.to_string().contains("sk-ant"));
}

#[test]
fn fleet_refuses_shared_credentials_and_does_not_damage_default_home() {
    let f = Fixture::new();
    f.sign_in_claude("fleet@example.com", "fleet-user", "same-token");
    f.run(&["import", "claude"]);
    let home = f._dir.path().join("copied-home");
    std::fs::create_dir(&home).unwrap();
    for name in [".credentials.json", ".claude.json"] {
        std::fs::copy(f.claude_home.join(name), home.join(name)).unwrap();
    }
    f.cmd(&["fleet", "register", "claude-1", "--home", home.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("refresh credential"));
    assert_eq!(
        f.claude_credentials()["claudeAiOauth"]["refreshToken"],
        "sk-ant-ort01-same-token"
    );
}

#[test]
fn fleet_poll_adopts_native_refresh_and_global_switching_cannot_touch_it() {
    let f = Fixture::new();
    let home = f.fleet_home("claude");
    let path = home.join(".credentials.json");
    let mut credential = read_json(&path);
    credential["claudeAiOauth"]["refreshToken"] = json!("rotated-by-native-cli");
    credential["claudeAiOauth"]["accessToken"] = json!("rotated-access");
    write_json(&path, &credential);
    f.run(&["list", "--refresh", "--json"]);
    assert_eq!(
        read_json(&f.data.join("accounts/claude-1.json"))["credential"]["refresh_token"],
        "rotated-by-native-cli"
    );
    assert_eq!(read_json(&path), credential);
    f.cmd(&["use", "claude-1"])
        .assert()
        .failure()
        .stderr(contains("fleet"));
    f.cmd(&["remove", "claude-1", "--yes"])
        .assert()
        .failure()
        .stderr(contains("fleet"));
}

#[test]
fn fleet_corrupt_state_never_silently_loses_affinity() {
    let f = Fixture::new();
    f.fleet_home("claude");
    std::fs::write(f.data.join("fleet.json"), "{").unwrap();
    f.cmd(&[
        "run",
        "--provider",
        "claude",
        "--account",
        "claude-1",
        "--scope",
        "city",
        "--session",
        "worker",
        "--dry-run",
    ])
    .assert()
    .failure()
    .stderr(contains("fleet.json"));
}

#[cfg(unix)]
#[test]
fn fleet_exec_preserves_arguments_exit_status_and_resume_affinity_for_both_providers() {
    use std::os::unix::fs::PermissionsExt;
    for provider in ["claude", "codex"] {
        let f = Fixture::new();
        let home = f.fleet_home(provider);
        let bin = f._dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let script = bin.join(provider);
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$AGENT_METER_ACCOUNT\" \"$AGENT_METER_SESSION\" \"$CLAUDE_CONFIG_DIR\" \"$CODEX_HOME\" \"${ANTHROPIC_API_KEY-unset}\" \"${CODEX_ACCESS_TOKEN-unset}\" \"$@\"\nexit 23\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let account = format!("{provider}-1");
        f.cmd(&[
            "run",
            "--provider",
            provider,
            "--account",
            &account,
            "--scope",
            "city",
            "--session",
            "worker",
            "--",
            "--model",
            "a model",
            "$(must-stay-literal)",
        ])
        .env("PATH", &path)
        .env("ANTHROPIC_API_KEY", "should-not-inherit")
        .env("CODEX_ACCESS_TOKEN", "should-not-inherit")
        .assert()
        .code(23)
        .stdout(
            contains(home.to_str().unwrap())
                .and(contains("a model\n$(must-stay-literal)"))
                .and(contains("unset\nunset")),
        );
        f.cmd(&[
            "run",
            "--provider",
            provider,
            "--scope",
            "city",
            "--session",
            "worker",
            "--",
            "resume",
            "conversation",
        ])
        .env("PATH", &path)
        .assert()
        .code(23)
        .stdout(contains(&account).and(contains("resume\nconversation")));
        f.cmd(&[
            "run",
            "--provider",
            provider,
            "--account",
            "different-account",
            "--scope",
            "city",
            "--session",
            "worker",
            "--dry-run",
        ])
        .assert()
        .failure();
        f.cmd(&[
            "run",
            "--provider",
            provider,
            "--scope",
            "other-city",
            "--session",
            "worker",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(contains("--account"));
    }
}

#[test]
fn fleet_rejects_a_different_login_in_a_registered_home() {
    let f = Fixture::new();
    let home = f.fleet_home("claude");
    let path = home.join(".claude.json");
    let mut identity = read_json(&path);
    identity["oauthAccount"]["organizationUuid"] = json!("other-workspace");
    write_json(&path, &identity);
    f.cmd(&[
        "run",
        "--provider",
        "claude",
        "--account",
        "claude-1",
        "--scope",
        "city",
        "--session",
        "worker",
        "--dry-run",
    ])
    .assert()
    .failure()
    .stderr(contains("identity"));
    f.run(&["list", "--refresh", "--json"]);
    assert_eq!(
        read_json(&f.data.join("accounts/claude-1.json"))["identity"]["workspace_id"],
        "org-1"
    );
}

#[test]
fn fleet_expired_tokens_are_left_to_the_native_cli() {
    let f = Fixture::new();
    let home = f.fleet_home("claude");
    let path = home.join(".credentials.json");
    let mut credential = read_json(&path);
    credential["claudeAiOauth"]["expiresAt"] = json!(1);
    write_json(&path, &credential);
    f.run(&["list", "--refresh", "--json"]);
    let stored = read_json(&f.data.join("accounts/claude-1.json"));
    assert!(
        stored["needs_login"].is_null(),
        "a failed meter refresh must not sign out the fleet"
    );
    assert_eq!(read_json(&path), credential);
}

#[test]
fn fleet_watch_inside_managed_home_never_switches_to_another_account() {
    let f = Fixture::new();
    let home = f.fleet_home("claude");
    f.sign_in_claude("other@example.com", "other-user", "other");
    f.run(&["import", "claude"]);
    f.cmd(&["use", "claude-2"])
        .env("CLAUDE_CONFIG_DIR", &home)
        .assert()
        .failure()
        .stderr(contains("fleet homes"));
    let before = read_json(&home.join(".credentials.json"));
    f.cmd(&["watch", "--once"])
        .env("CLAUDE_CONFIG_DIR", &home)
        .assert()
        .success();
    assert_eq!(read_json(&home.join(".credentials.json")), before);
    let rows = f.accounts();
    names_the_same_directory(rows[0]["fleetHome"].as_str().unwrap(), &home);
}

#[cfg(unix)]
#[test]
fn fleet_concurrent_launches_cannot_rebind_a_conversation() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.fleet_home("claude");
    let second = Fixture::new();
    let second_home = second.fleet_home("claude");
    let mut account = read_json(&second.data.join("accounts/claude-1.json"));
    account["id"] = json!("claude-2");
    // A separate native login, with a different identity.
    account["identity"]["user_id"] = json!("second-user");
    let mut identity = read_json(&second_home.join(".claude.json"));
    identity["oauthAccount"]["accountUuid"] = json!("second-user");
    write_json(&second_home.join(".claude.json"), &identity);
    write_json(&f.data.join("accounts/claude-2.json"), &account);
    f.run(&[
        "fleet",
        "register",
        "claude-2",
        "--home",
        second_home.to_str().unwrap(),
    ]);
    let bin = f._dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    // Written rather than symlinked to /bin/true: macOS keeps `true` in
    // /usr/bin, so the link dangled there and every launch failed to exec.
    let stand_in = bin.join("claude");
    std::fs::write(
        &stand_in,
        "#!/bin/sh
exit 0
",
    )
    .unwrap();
    std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
    let children: Vec<_> = (0..12)
        .map(|i| {
            let account = if i % 2 == 0 { "claude-1" } else { "claude-2" };
            f.cmd(&[
                "run",
                "--provider",
                "claude",
                "--account",
                account,
                "--scope",
                "city",
                "--session",
                "same-session",
            ])
            .env("PATH", &bin)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
        })
        .collect();
    let successes = children
        .into_iter()
        .map(|mut child| child.wait().unwrap().success())
        .filter(|success| *success)
        .count();
    assert_eq!(successes, 6);
    let state = read_json(&f.data.join("fleet.json"));
    assert_eq!(state["bindings"].as_array().unwrap().len(), 1);
}

#[test]
fn fleet_runtime_home_mismatch_does_not_record_a_binding() {
    let f = Fixture::new();
    f.fleet_home("codex");
    f.cmd(&[
        "run",
        "--provider",
        "codex",
        "--account",
        "codex-1",
        "--scope",
        "city",
        "--session",
        "worker",
        "--expect-home",
        f.codex_home.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(contains("--expect-home"));
    assert_eq!(
        read_json(&f.data.join("fleet.json"))["bindings"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[cfg(unix)]
#[test]
fn fleet_city_wrappers_preserve_city_identity_and_native_resume_arguments() {
    use std::os::unix::fs::PermissionsExt;
    for provider in ["claude", "codex"] {
        let f = Fixture::new();
        let home = f.fleet_home(provider);
        let bin = f._dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let native = bin.join(provider);
        std::fs::write(
            &native,
            "#!/bin/sh\nprintf '%s\\n' \"$GC_SESSION_ID\" \"$GT_ROOT\" \"$AGENT_METER_ACCOUNT\" \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o755)).unwrap();
        let wrapper =
            Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("examples/gascity/agent-meter-{provider}"));
        let mut command = Command::new(wrapper);
        let fixture_command = f.cmd(&[]);
        for (key, value) in fixture_command.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        let home_var = if provider == "claude" {
            "CLAUDE_CONFIG_DIR"
        } else {
            "CODEX_HOME"
        };
        let resume = if provider == "claude" {
            "--resume"
        } else {
            "resume"
        };
        command
            .env("PATH", &bin)
            .env("AGENT_METER_BIN", fixture_command.get_program())
            .env("AGENT_METER_ACCOUNT", format!("{provider}-1"))
            .env(home_var, &home)
            .env("GT_ROOT", "/cities/federation")
            .env("GC_SESSION_ID", "gc-durable-identity")
            .args([resume, "native-conversation-id"])
            .assert()
            .success()
            .stdout(
                contains("gc-durable-identity\n/cities/federation")
                    .and(contains(format!("{resume}\nnative-conversation-id"))),
            );
        assert_eq!(
            read_json(&f.data.join("fleet.json"))["bindings"][0]["session"],
            "gc-durable-identity"
        );
    }
}

#[test]
fn fleet_credentials_are_not_exported_or_synced() {
    let f = Fixture::new();
    f.fleet_home("claude");
    let out = f._dir.path().join("foreign-store");
    f.cmd(&[
        "export",
        "--to",
        "gemctl",
        "--dir",
        out.to_str().unwrap(),
        "--yes",
    ])
    .assert()
    .failure()
    .stderr(contains("fleet credentials"));
    assert!(!out.exists());
    let store = agent_meter::store::Store::open(f.data.clone()).unwrap();
    let engine = agent_meter::engine::Engine::with_store(store).unwrap();
    assert!(engine.records().unwrap().is_empty());
}

#[test]
fn json_usage_exposes_machine_scopes_and_rate_limited_polling() {
    let f = Fixture::new();
    f.sign_in_claude("user@example.com", "user", "refresh");
    f.run(&["import", "claude"]);
    let now = jiff::Timestamp::now();
    let cache = json!({"entries": {"claude-1": {
        "usage": {"observed_at": now.to_string(), "windows": [
            {"window_secs": 18000, "scope": null, "used_percent": 23.0, "resets_at": (now + std::time::Duration::from_secs(3600)).to_string()},
            {"window_secs": 604800, "scope": "Opus", "used_percent": 55.0}
        ], "limit_reached": false}, "failures": 0
    }}});
    write_json(&f.data.join("usage.json"), &cache);
    let rows: Value = serde_json::from_str(&f.run(&["list", "--poll", "--json"])).unwrap();
    assert!(
        rows[0]["error"].is_null(),
        "fresh cache must not be polled while offline"
    );
    assert_eq!(rows[0]["usage"]["windows"][0]["kind"], "five_hour");
    assert!(rows[0]["usage"]["windows"][0]["scope"].is_null());
    assert_eq!(rows[0]["usage"]["windows"][1]["scope"], "Opus");
    assert_eq!(rows[0]["pollIntervalSeconds"], 300);
    assert!(rows[0]["usage"]["limitResets"].is_null(), "not said is not none");
    assert!(!rows.to_string().contains("refresh_token"));
    f.cmd(&["list", "--poll", "--refresh"]).assert().failure();
}

/// An account's limit resets are listed with it: how many, when they lapse
/// and what each is — and one that lapsed since the reading is gone from both.
#[test]
fn limit_resets_are_listed_with_their_deadlines() {
    let f = Fixture::new();
    f.sign_in_claude("user@example.com", "user", "refresh");
    f.run(&["import", "claude"]);
    let now = jiff::Timestamp::now();
    let later = now + jiff::SignedDuration::from_hours(24 * 20 + 1);
    let cache = json!({"entries": {"claude-1": {
        "usage": {"observed_at": now.to_string(), "windows": [], "resets": {
            "available": 2,
            "grants": [
                {"title": "Launch reset", "left": 1, "expires_at": later.to_string(),
                 "refills": ["five_hour", "weekly"]},
                {"title": "Lapsed", "left": 1,
                 "expires_at": (now - jiff::SignedDuration::from_hours(1)).to_string()}
            ]
        }}, "failures": 0
    }}});
    write_json(&f.data.join("usage.json"), &cache);

    let rows: Value = serde_json::from_str(&f.run(&["list", "--json"])).unwrap();
    let resets = &rows[0]["usage"]["limitResets"];
    assert_eq!(resets["available"], 1);
    assert_eq!(resets["nextExpiresAt"], later.to_string());
    assert_eq!(resets["grants"].as_array().unwrap().len(), 1);
    assert_eq!(resets["grants"][0]["title"], "Launch reset");
    assert_eq!(resets["grants"][0]["refills"], json!(["five_hour", "weekly"]));

    let table = f.run(&["list"]);
    assert!(table.contains("LIMIT RESETS"), "{table}");
    assert!(table.contains("1, expires in 20d"), "{table}");
}

/// Five Claude accounts in each place the queue can put one: the one in use,
/// two that could be switched to, one that is spent and one the provider
/// signed out. Readings are taken at the returned instant.
fn standing_fixture() -> (Fixture, jiff::Timestamp) {
    let fixture = Fixture::new();
    for (email, uuid, refresh) in [
        ("spare@example.com", "uuid-1", "one"),
        ("dev@example.com", "uuid-2", "two"),
        ("spent@example.com", "uuid-3", "three"),
        ("lapsed@example.com", "uuid-4", "four"),
        ("later@example.com", "uuid-5", "five"),
    ] {
        fixture.sign_in_claude(email, uuid, refresh);
        fixture.run(&["import", "claude"]);
    }
    fixture.run(&["use", "claude-2"]);

    let record = fixture.data.join("accounts").join("claude-4.json");
    let mut stored = read_json(&record);
    stored["needs_login"] = json!("the provider rejected its credential: invalid_grant");
    write_json(&record, &stored);

    let now = jiff::Timestamp::now();
    let window = |secs: u64, used: f64, resets_in: i64| json!({"window_secs": secs, "used_percent": used, "resets_at": standing_reset(now, resets_in)});
    let reading = |windows: Vec<Value>| json!({"usage": {"observed_at": now, "windows": windows, "limit_reached": false}, "failures": 0});
    write_json(
        &fixture.data.join("usage.json"),
        &json!({"entries": {
            "claude-1": reading(vec![
                window(18000, 10.0, 2 * HOUR + 30 * 60),
                window(604800, 60.0, 3 * DAY + 12 * HOUR),
            ]),
            "claude-2": reading(vec![
                window(18000, 50.0, HOUR),
                window(604800, 30.0, 6 * DAY),
                json!({"window_secs": 604800, "scope": "Opus", "used_percent": 20.0}),
            ]),
            "claude-3": reading(vec![
                window(18000, 100.0, 4 * HOUR),
                window(604800, 70.0, DAY),
            ]),
            "claude-5": reading(vec![
                window(18000, 20.0, 3 * HOUR),
                window(604800, 40.0, 5 * DAY),
            ]),
        }}),
    );
    (fixture, now)
}

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

/// When a window of the standing fixture resets: `secs` after `now`, and fifty
/// seconds more, so the countdowns the table prints hold still for as long as
/// a test could take to run.
fn standing_reset(now: jiff::Timestamp, secs: i64) -> String {
    (now + jiff::SignedDuration::from_secs(secs + 50)).to_string()
}

/// `list --json` with only the fields it has always had.
///
/// Fields added later go at the end of their objects, so cutting their lines
/// out of the printed text, with the comma that came before them, leaves the
/// bytes an older version printed.
fn as_originally_listed(printed: &str) -> String {
    const ADDED: [&str; 3] = ["queue", "pace", "counted"];
    let added = |line: &str| {
        let text = line.trim_start();
        ADDED.iter().any(|key| text.starts_with(&format!("\"{key}\": ")))
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut lines = printed.lines().peekable();
    while let Some(line) = lines.next() {
        if !added(line) {
            kept.push(line);
            continue;
        }
        let text = line.trim_start();
        let indent = &line[..line.len() - text.len()];
        // The first of a run of them takes the comma before it along.
        let before = kept.pop().unwrap();
        kept.push(before.strip_suffix(',').unwrap_or(before));
        let last = if text.ends_with('{') {
            let close = format!("{indent}}}");
            lines.find(|inner| inner.starts_with(&close)).unwrap()
        } else {
            line
        };
        assert!(
            !last.ends_with(',') || lines.peek().is_some_and(|next| added(next)),
            "only fields added later follow a field added later: {line}"
        );
    }
    format!("{}\n", kept.join("\n"))
}

/// What `list` and `watch` print is read by programs as well as people — a
/// service wrapper matches the watcher's lines — so it stays exactly as it
/// was, byte for byte, whatever is added beside it.
#[test]
fn list_and_watch_print_exactly_what_they_always_have() {
    let (fixture, now) = standing_fixture();

    let table = [
        "     ID         PROVIDER      ACCOUNT              ORGANIZATION   PLAN   USED   WINDOWS                               RESETS IN   LIMIT RESETS                ",
        "──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────",
        "     claude-1   Claude Code   spare@example.com    Example Inc    -      60%    5h 10%  weekly 60%                    3d12h       ?                           ",
        " *   claude-2   Claude Code   dev@example.com      Example Inc    -      50%    5h 50%  weekly 30%  weekly Opus 20%   1h          ?                           ",
        "     claude-3   Claude Code   spent@example.com    Example Inc    -      100%   5h 100%  weekly 70%                   4h          ?                           ",
        "     claude-4   Claude Code   lapsed@example.com   Example Inc    -      ?                                            -           ?              login needed ",
        "     claude-5   Claude Code   later@example.com    Example Inc    -      40%    5h 20%  weekly 40%                    5d          ?                           ",
        "  ! claude-4: the provider rejected its credential: invalid_grant",
        "",
    ]
    .join("\n");
    assert_eq!(fixture.run(&["list"]), table);

    let expected = r#"[
  {
    "id": "claude-1",
    "provider": "claude",
    "label": null,
    "email": "spare@example.com",
    "plan": null,
    "capacityMultiplier": null,
    "organization": "Example Inc",
    "active": false,
    "fleetHome": null,
    "needsLogin": null,
    "pollIntervalSeconds": 300,
    "usage": {
      "observedAt": "@now",
      "usedPercent": 60.0,
      "exhausted": false,
      "windows": [
        {
          "label": "5h",
          "kind": "five_hour",
          "scope": null,
          "windowSeconds": 18000,
          "usedPercent": 10.0,
          "resetsAt": "@2h30m"
        },
        {
          "label": "weekly",
          "kind": "weekly",
          "scope": null,
          "windowSeconds": 604800,
          "usedPercent": 60.0,
          "resetsAt": "@3d12h"
        }
      ],
      "limitResets": null
    },
    "error": null,
    "errorAt": null
  },
  {
    "id": "claude-2",
    "provider": "claude",
    "label": null,
    "email": "dev@example.com",
    "plan": null,
    "capacityMultiplier": null,
    "organization": "Example Inc",
    "active": true,
    "fleetHome": null,
    "needsLogin": null,
    "pollIntervalSeconds": 300,
    "usage": {
      "observedAt": "@now",
      "usedPercent": 50.0,
      "exhausted": false,
      "windows": [
        {
          "label": "5h",
          "kind": "five_hour",
          "scope": null,
          "windowSeconds": 18000,
          "usedPercent": 50.0,
          "resetsAt": "@1h"
        },
        {
          "label": "weekly",
          "kind": "weekly",
          "scope": null,
          "windowSeconds": 604800,
          "usedPercent": 30.0,
          "resetsAt": "@6d"
        },
        {
          "label": "weekly Opus",
          "kind": "weekly",
          "scope": "Opus",
          "windowSeconds": 604800,
          "usedPercent": 20.0,
          "resetsAt": null
        }
      ],
      "limitResets": null
    },
    "error": null,
    "errorAt": null
  },
  {
    "id": "claude-3",
    "provider": "claude",
    "label": null,
    "email": "spent@example.com",
    "plan": null,
    "capacityMultiplier": null,
    "organization": "Example Inc",
    "active": false,
    "fleetHome": null,
    "needsLogin": null,
    "pollIntervalSeconds": 300,
    "usage": {
      "observedAt": "@now",
      "usedPercent": 100.0,
      "exhausted": true,
      "windows": [
        {
          "label": "5h",
          "kind": "five_hour",
          "scope": null,
          "windowSeconds": 18000,
          "usedPercent": 100.0,
          "resetsAt": "@4h"
        },
        {
          "label": "weekly",
          "kind": "weekly",
          "scope": null,
          "windowSeconds": 604800,
          "usedPercent": 70.0,
          "resetsAt": "@1d"
        }
      ],
      "limitResets": null
    },
    "error": null,
    "errorAt": null
  },
  {
    "id": "claude-4",
    "provider": "claude",
    "label": null,
    "email": "lapsed@example.com",
    "plan": null,
    "capacityMultiplier": null,
    "organization": "Example Inc",
    "active": false,
    "fleetHome": null,
    "needsLogin": "the provider rejected its credential: invalid_grant",
    "pollIntervalSeconds": 300,
    "usage": null,
    "error": null,
    "errorAt": null
  },
  {
    "id": "claude-5",
    "provider": "claude",
    "label": null,
    "email": "later@example.com",
    "plan": null,
    "capacityMultiplier": null,
    "organization": "Example Inc",
    "active": false,
    "fleetHome": null,
    "needsLogin": null,
    "pollIntervalSeconds": 300,
    "usage": {
      "observedAt": "@now",
      "usedPercent": 40.0,
      "exhausted": false,
      "windows": [
        {
          "label": "5h",
          "kind": "five_hour",
          "scope": null,
          "windowSeconds": 18000,
          "usedPercent": 20.0,
          "resetsAt": "@3h"
        },
        {
          "label": "weekly",
          "kind": "weekly",
          "scope": null,
          "windowSeconds": 604800,
          "usedPercent": 40.0,
          "resetsAt": "@5d"
        }
      ],
      "limitResets": null
    },
    "error": null,
    "errorAt": null
  }
]
"#;
    let mut expected = expected.replace("@now", &now.to_string());
    for (token, secs) in [
        ("@2h30m", 2 * HOUR + 30 * 60),
        ("@3d12h", 3 * DAY + 12 * HOUR),
        ("@1h", HOUR),
        ("@6d", 6 * DAY),
        ("@4h", 4 * HOUR),
        ("@1d", DAY),
        ("@3h", 3 * HOUR),
        ("@5d", 5 * DAY),
    ] {
        expected = expected.replace(token, &standing_reset(now, secs));
    }
    assert_eq!(as_originally_listed(&fixture.run(&["list", "--json"])), expected);

    // Claude Code re-reads its credential, so spending a week that resets
    // sooner costs nothing — which is what the watcher would do here.
    assert_eq!(
        fixture.run(&["watch", "--once", "--dry-run"]),
        "Claude Code: would switch to claude-1 (claude-1's weekly allowance resets sooner, so \
         spending it first wastes nothing; it is at 60% and switching interrupts nothing)\n"
    );
}

/// Each row says where its account stands in the order the watcher would take
/// its provider's accounts, worked out by the ranking the watcher switches on.
#[test]
fn each_row_says_where_its_account_stands_in_the_queue() {
    let (fixture, _now) = standing_fixture();
    fixture.sign_in_codex("dev@openai.example", "user-1", "one");
    fixture.run(&["import", "codex"]);

    let rows = fixture.accounts();
    let place = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap()["queue"].clone();
    assert_eq!(place("claude-2"), json!({"position": 1, "role": "in_use"}));
    // Its week resets sooner than claude-5's, so it is spent first.
    assert_eq!(place("claude-1"), json!({"position": 2, "role": "next_up"}));
    assert_eq!(place("claude-5"), json!({"position": 3, "role": "queued"}));
    // Spent, then signed out: neither can be switched to.
    assert_eq!(place("claude-3"), json!({"position": 4, "role": "unusable"}));
    assert_eq!(place("claude-4"), json!({"position": 5, "role": "unusable"}));
    // Every provider has a queue of its own.
    assert_eq!(place("codex-1"), json!({"position": 1, "role": "in_use"}));

    let only_codex: Vec<Value> =
        serde_json::from_str(&fixture.run(&["list", "--provider", "codex", "--json"])).unwrap();
    assert_eq!(only_codex[0]["queue"], json!({"position": 1, "role": "in_use"}));
}

/// A weekly window says how far ahead of an even pace through the week it has
/// been spent; one that is not ahead, or that nothing dates, says null. A
/// five-hour window is a rate rather than a budget and says nothing of pace.
#[test]
fn weekly_windows_say_how_far_ahead_of_pace_they_are() {
    let (fixture, _now) = standing_fixture();
    let rows = fixture.accounts();
    let windows = |id: &str| {
        rows.iter().find(|row| row["id"] == id).unwrap()["usage"]["windows"]
            .as_array()
            .unwrap()
            .clone()
    };
    let ahead = |window: &Value| window["pace"]["aheadPercent"].as_f64();

    // Half the week gone and 60% of it spent: ten points ahead.
    let spare = windows("claude-1");
    assert!(spare[0].get("pace").is_none(), "{}", spare[0]);
    assert!((ahead(&spare[1]).unwrap() - 10.0).abs() < 0.1, "{}", spare[1]);

    // A day gone and 30% spent: 30 − 100 × 1/7.
    let in_use = windows("claude-2");
    assert!(
        (ahead(&in_use[1]).unwrap() - (30.0 - 100.0 / 7.0)).abs() < 0.1,
        "{}",
        in_use[1]
    );
    // A per-model week with no reset time has no clock to be ahead of.
    assert_eq!(in_use[2]["pace"], json!({"aheadPercent": null}));

    // Six days gone and 70% spent: behind, not ahead.
    assert_eq!(windows("claude-3")[1]["pace"], json!({"aheadPercent": null}));
}

/// A watcher left running, and the lines it prints as it prints them. Lines
/// on standard error arrive prefixed `stderr: `.
struct Watcher {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Watcher {
    fn start(fixture: &Fixture, args: &[&str]) -> Self {
        let mut child = fixture
            .cmd(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        let forward = |stream: Box<dyn std::io::Read + Send>, prefix: &'static str| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stream).lines().map_while(Result::ok) {
                    if tx.send(format!("{prefix}{line}")).is_err() {
                        break;
                    }
                }
            });
        };
        forward(Box::new(child.stdout.take().unwrap()), "");
        forward(Box::new(child.stderr.take().unwrap()), "stderr: ");
        Self { child, lines }
    }

    /// Waits for the next line that contains `what`, and returns it.
    ///
    /// Far sooner than the watcher's own interval, which is a minute at the
    /// least: a line that only comes when that runs out is a line that did not
    /// come in time.
    #[track_caller]
    fn next(&self, what: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(what) => return line,
                Ok(_) => {}
                Err(_) => panic!("the watcher never printed {what:?}"),
            }
        }
    }

    /// Waits for the next line on standard error that contains `error` and
    /// the next line on standard output that contains `output`, and returns
    /// both.
    ///
    /// The two streams are read on two threads, so a line on one can arrive
    /// before a line the watcher printed earlier on the other. Waiting for
    /// one and then the other would throw away the second when it came first.
    #[track_caller]
    fn next_of_each(&self, error: &str, output: &str) -> (String, String) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let (mut from_error, mut from_output): (Option<String>, Option<String>) = (None, None);
        loop {
            if let (Some(e), Some(o)) = (&from_error, &from_output) {
                return (e.clone(), o.clone());
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => match line.strip_prefix("stderr: ") {
                    Some(rest) if rest.contains(error) => {
                        from_error.get_or_insert(line);
                    }
                    None if line.contains(output) => {
                        from_output.get_or_insert(line);
                    }
                    _ => {}
                },
                Err(_) => {
                    let missing = if from_error.is_none() { error } else { output };
                    panic!("the watcher never printed {missing:?}")
                }
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Replaces `path` with `contents` in one step, as an editor saving the file
/// would. Written in place, the file is empty for an instant, and a watcher
/// that read it then would see no settings at all.
///
/// The rename is retried for a moment: on Windows a virus scanner looking at
/// the new file can briefly refuse it, as `fsutil` allows for.
fn replace_file(path: &Path, contents: &str) {
    let staged = path.with_extension("staged");
    std::fs::write(&staged, contents).unwrap();
    for _ in 0..50 {
        if std::fs::rename(&staged, path).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::rename(&staged, path).unwrap();
}

/// Two Claude accounts with only five-hour windows, so that the threshold
/// alone decides: the one in use at 50%, a spare at 10%.
fn threshold_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.sign_in_claude("spare@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);
    fixture.sign_in_claude("dev@example.com", "uuid-2", "two");
    fixture.run(&["import", "claude"]);
    let now = jiff::Timestamp::now();
    let reading = |used: f64| {
        json!({"usage": {
            "observed_at": now,
            "windows": [{"window_secs": 18000, "used_percent": used, "resets_at": standing_reset(now, HOUR)}],
            "limit_reached": false,
        }, "failures": 0})
    };
    write_json(
        &fixture.data.join("usage.json"),
        &json!({"entries": {"claude-1": reading(10.0), "claude-2": reading(50.0)}}),
    );
    fixture
}

const WOULD_SWITCH: &str = "Claude Code: would switch to claude-1 (50% used; claude-1 is at 10%)";
const STAYS: &str = "Claude Code: 50% used, staying put";

/// `config set` reaches a watcher that is already running: the settings are
/// read again before every check, and a change to them brings the next check
/// forward rather than leaving it to the end of the interval.
#[test]
fn a_running_watcher_takes_up_changed_settings_at_its_next_check() {
    let fixture = threshold_fixture();
    let watcher = Watcher::start(&fixture, &["watch", "--dry-run"]);
    watcher.next("Watching every 1m at a 90% threshold");
    assert_eq!(watcher.next("Claude Code:"), STAYS);

    fixture.run(&["config", "set", "watch.threshold", "40"]);
    assert_eq!(watcher.next("Claude Code:"), WOULD_SWITCH);

    // A threshold set for the provider still wins over the general one.
    fixture.run(&["config", "set", "provider.claude.threshold", "95"]);
    assert_eq!(watcher.next("Claude Code:"), STAYS);
}

/// Two Claude accounts whose own limits are comfortable, each with a week of
/// Fable beside them: the one in use at `in_use` percent of it, the spare at
/// `spare`. Both weeks turn over together, so neither is the sooner to expire.
fn fable_fixture(in_use: f64, spare: f64) -> Fixture {
    let fixture = Fixture::new();
    fixture.sign_in_claude("spare@example.com", "uuid-1", "one");
    fixture.run(&["import", "claude"]);
    fixture.sign_in_claude("dev@example.com", "uuid-2", "two");
    fixture.run(&["import", "claude"]);
    let now = jiff::Timestamp::now();
    let reading = |five_hour: f64, fable: f64| {
        json!({"usage": {
            "observed_at": now,
            "windows": [
                {"window_secs": 18000, "used_percent": five_hour, "resets_at": standing_reset(now, HOUR)},
                {"window_secs": 604_800, "scope": "Fable", "used_percent": fable,
                 "resets_at": standing_reset(now, 3 * DAY)},
            ],
            "limit_reached": false,
        }, "failures": 0})
    };
    write_json(
        &fixture.data.join("usage.json"),
        &json!({"entries": {"claude-1": reading(10.0, spare), "claude-2": reading(50.0, in_use)}}),
    );
    fixture
}

/// A model's own limit stops that model only, so it moves nothing — until the
/// settings say the work is done in that model. Then it is the account's:
/// nearing it is what the watcher leaves an account for.
#[test]
fn a_models_limit_switches_accounts_once_its_model_is_counted() {
    let fixture = fable_fixture(95.0, 20.0);
    let check = || fixture.run(&["watch", "--once", "--dry-run"]);
    let listed = |id: &str| {
        let rows = fixture.accounts();
        rows.into_iter().find(|row| row["id"] == id).unwrap()
    };

    assert_eq!(check(), "Claude Code: 50% used, staying put\n");
    let in_use = listed("claude-2");
    assert_eq!(in_use["usage"]["usedPercent"], json!(50.0));
    assert_eq!(in_use["usage"]["windows"][0]["counted"], json!(true));
    assert_eq!(in_use["usage"]["windows"][1]["counted"], json!(false));

    // Named however it is typed.
    fixture.run(&["config", "set", "provider.claude.models", "fable"]);
    assert_eq!(
        check(),
        "Claude Code: would switch to claude-1 (95% used; claude-1 is at 20%)\n"
    );
    let in_use = listed("claude-2");
    assert_eq!(in_use["usage"]["usedPercent"], json!(95.0));
    assert_eq!(in_use["usage"]["windows"][1]["counted"], json!(true));
    assert_eq!(listed("claude-1")["queue"]["role"], json!("next_up"));
    assert!(
        fixture.run(&["config", "show"]).contains("models = [\"fable\"]"),
        "the setting is in the file"
    );

    // It is Claude's setting, and Codex's is its own.
    fixture.run(&["config", "set", "provider.claude.models", ""]);
    fixture.run(&["config", "set", "provider.codex.models", "Fable"]);
    assert_eq!(check(), "Claude Code: 50% used, staying put\n");
}

/// An account that has spent a counted model's week is spent, to somebody
/// working in that model: it is not somewhere to switch to.
#[test]
fn an_account_that_has_spent_a_counted_model_is_not_switched_to() {
    let fixture = fable_fixture(95.0, 100.0);
    fixture.run(&["config", "set", "provider.claude.models", "Fable"]);

    let rows = fixture.accounts();
    let spare = rows.iter().find(|row| row["id"] == "claude-1").unwrap();
    assert_eq!(spare["usage"]["exhausted"], json!(true));
    assert_eq!(spare["queue"]["role"], json!("unusable"));
    fixture
        .cmd(&["watch", "--once", "--dry-run"])
        .assert()
        .stdout(contains("every account is out of quota; claude-1 frees up in 3d"));
}

/// A threshold given on the command line is this run's, and outlasts any
/// change to the file.
#[test]
fn a_watchers_own_threshold_outlasts_the_file() {
    let fixture = threshold_fixture();
    let watcher = Watcher::start(&fixture, &["watch", "--dry-run", "--threshold", "95"]);
    assert_eq!(watcher.next("Claude Code:"), STAYS);

    fixture.run(&["config", "set", "watch.threshold", "40"]);
    assert_eq!(watcher.next("Claude Code:"), STAYS);
}

/// A settings file edited by hand into something that does not read leaves
/// the watcher on the last settings that did, rather than stopping it or
/// dropping back to the defaults, and it says so once.
#[test]
fn a_settings_file_that_no_longer_reads_leaves_the_last_good_settings() {
    let fixture = threshold_fixture();
    let watcher = Watcher::start(&fixture, &["watch", "--dry-run"]);
    assert_eq!(watcher.next("Claude Code:"), STAYS);
    fixture.run(&["config", "set", "watch.threshold", "40"]);
    assert_eq!(watcher.next("Claude Code:"), WOULD_SWITCH);

    let config = fixture.data.join("config.toml");
    replace_file(&config, "[watch]\nthreshold = \"high\"\n");
    let (warning, check) = watcher.next_of_each("config.toml", "Claude Code:");
    assert!(warning.contains("could not be read"), "{warning}");
    assert_eq!(check, WOULD_SWITCH);

    replace_file(&config, "[watch]\nthreshold = 95.0\n");
    assert_eq!(watcher.next("Claude Code:"), STAYS);
}

/// What the stand-in `claude` prints as the sign-in page.
#[cfg(target_os = "linux")]
const SIGN_IN_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&state=x";

/// A stand-in for `claude auth login`, first on the PATH: it prints Claude
/// Code's prompt, hyperlink and all, reads a code, and with `good-code`
/// signs in to the account `FAKE_EMAIL` names. With `FAKE_NO_CODE` set it
/// asks for nothing and signs in at once, as when the browser's callback
/// reaches the CLI. It writes its process id to `FAKE_PID`.
///
/// Linux only: on macOS a login's credential goes to the Keychain, which a
/// test must not touch, and Windows has no `sh`.
#[cfg(target_os = "linux")]
fn fake_claude(fixture: &Fixture) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = fixture._dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = r#"#!/bin/sh
echo $$ > "$FAKE_PID"
printf 'Opening browser to sign in…\n'
printf "If the browser didn't open, visit: \033]8;;URL\007URL\033]8;;\007\n"
if [ -z "$FAKE_NO_CODE" ]; then
  printf 'Paste code here if prompted > '
  read code
  if [ "$code" != "good-code" ]; then
    echo 'OAuth error: Invalid code'
    exit 1
  fi
fi
cat > "$CLAUDE_CONFIG_DIR/.credentials.json" <<JSON
{"claudeAiOauth": {"accessToken": "sk-ant-oat01-$FAKE_REFRESH", "refreshToken": "sk-ant-ort01-$FAKE_REFRESH",
 "expiresAt": 1900000000000, "scopes": ["user:inference", "user:profile"], "subscriptionType": "max"}}
JSON
cat > "$CLAUDE_CONFIG_DIR/.claude.json" <<JSON
{"hasCompletedOnboarding": true, "oauthAccount": {"accountUuid": "$FAKE_UUID", "emailAddress": "$FAKE_EMAIL",
 "organizationUuid": "org-1", "organizationName": "Example Inc"}}
JSON
echo 'Login successful.'
"#
    .replace("URL", SIGN_IN_URL);
    let path = bin.join("claude");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Points `command` at the stand-in `claude`, signing in to `email`, and
/// returns where the stand-in will write its process id.
#[cfg(target_os = "linux")]
fn with_fake_claude(fixture: &Fixture, command: &mut Command, email: &str) -> PathBuf {
    let bin = fake_claude(fixture);
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let pid_file = fixture._dir.path().join("claude.pid");
    command
        .env("PATH", path)
        .env("FAKE_PID", &pid_file)
        .env("FAKE_EMAIL", email)
        .env("FAKE_UUID", format!("uuid-{email}"))
        .env("FAKE_REFRESH", email.replace(['@', '.'], "-"));
    pid_file
}

/// A login driven the way a program drives one: JSON lines in and out.
#[cfg(target_os = "linux")]
struct ProgramLogin {
    child: std::process::Child,
    events: std::sync::mpsc::Receiver<Value>,
    stdout_lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pid_file: PathBuf,
}

#[cfg(target_os = "linux")]
impl ProgramLogin {
    /// Runs `agent-meter <args>` with the stand-in `claude` signing in to
    /// `email`.
    fn start(fixture: &Fixture, args: &[&str], email: &str) -> Self {
        Self::start_with(fixture, args, email, |_| {})
    }

    /// [`Self::start`], with `configure` given the command last.
    fn start_with(
        fixture: &Fixture,
        args: &[&str],
        email: &str,
        configure: impl FnOnce(&mut Command),
    ) -> Self {
        let mut command = fixture.cmd(args);
        let pid_file = with_fake_claude(fixture, &mut command, email);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = std::sync::mpsc::channel();
        let stdout_lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = stdout_lines.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                seen.lock().unwrap().push(line.clone());
                let event: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("stdout carries JSON lines only, not {line:?}"));
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            events,
            stdout_lines,
            pid_file,
        }
    }

    fn next(&self) -> Value {
        self.events
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the login said nothing more")
    }

    fn send(&mut self, line: &str) {
        use std::io::Write;
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Waits for it to exit, and returns how it did, everything it printed
    /// on stdout, and its stderr.
    fn finish(mut self) -> (std::process::ExitStatus, Vec<String>, String) {
        use std::io::Read;
        drop(self.child.stdin.take());
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        let status = self.child.wait().unwrap();
        // Whatever is left on stdout has been read once the pipe closes.
        while self
            .events
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok()
        {}
        let stdout = self.stdout_lines.lock().unwrap().clone();
        (status, stdout, stderr)
    }
}

/// Every throwaway login directory has been removed.
#[cfg(target_os = "linux")]
fn no_login_left_behind(fixture: &Fixture) {
    let logins = fixture.data.join("logins");
    let left: Vec<_> = std::fs::read_dir(&logins)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// A program signs an account in by reading JSON lines and writing them: it
/// is told the sign-in page, told when a code is wanted, hands the code back,
/// and is told which account that stored. Nothing else is on stdout, and the
/// code is printed nowhere.
#[cfg(target_os = "linux")]
#[test]
fn a_program_can_drive_a_login_over_json_lines() {
    let fixture = Fixture::new();
    let mut login = ProgramLogin::start(&fixture, &["add", "claude", "--json-io"], "new@example.com");
    assert_eq!(login.next(), json!({"event": "url", "url": SIGN_IN_URL}));
    assert_eq!(login.next(), json!({"event": "need_code"}));

    // Lines that are not a code are passed over, never to the CLI.
    login.send("good-code");
    login.send(r#"{"code": "good-code\nsecond line"}"#);
    login.send(r#"{"code": "good-code"}"#);
    assert_eq!(
        login.next(),
        json!({"event": "done", "account": "claude-1", "added": true})
    );
    let (status, stdout, stderr) = login.finish();
    assert!(status.success(), "{stderr}");
    assert_eq!(stdout.len(), 3, "{stdout:?}");
    assert!(!stderr.contains("good-code"), "{stderr}");
    no_login_left_behind(&fixture);
    assert_eq!(fixture.accounts()[0]["email"], "new@example.com");

    // Signing the same account in again updates it.
    let mut again = ProgramLogin::start(&fixture, &["login", "claude-1", "--json-io"], "new@example.com");
    assert_eq!(again.next()["event"], "url");
    assert_eq!(again.next()["event"], "need_code");
    again.send(r#"{"code": "good-code"}"#);
    assert_eq!(
        again.next(),
        json!({"event": "done", "account": "claude-1", "added": false})
    );
    assert!(again.finish().0.success());

    // Whoever actually signs in is the account stored. The caller, who asked
    // for claude-1, is the one to see that this is a different account.
    let mut other = ProgramLogin::start(&fixture, &["login", "claude-1", "--json-io"], "other@example.com");
    other.next();
    other.next();
    other.send(r#"{"code": "good-code"}"#);
    assert_eq!(
        other.next(),
        json!({"event": "done", "account": "claude-2", "added": true})
    );
    assert!(other.finish().0.success());
    no_login_left_behind(&fixture);
}

/// A login that fails ends with one error line and stores nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_login_ends_with_an_error_and_stores_nothing() {
    let fixture = Fixture::new();
    let mut login = ProgramLogin::start(&fixture, &["add", "claude", "--json-io"], "new@example.com");
    login.next();
    login.next();
    login.send(r#"{"code": "wrong-code"}"#);
    let error = login.next();
    assert_eq!(error["event"], "error", "{error}");
    assert!(
        error["message"].as_str().unwrap().contains("Nothing was added"),
        "{error}"
    );
    let (status, stdout, _) = login.finish();
    assert!(!status.success());
    assert_eq!(stdout.len(), 3, "{stdout:?}");
    assert!(fixture.accounts().is_empty());
    no_login_left_behind(&fixture);

    // An account that does not exist cannot be signed in to again, and a
    // program is told so in the same way.
    let missing = ProgramLogin::start(&fixture, &["login", "nobody", "--json-io"], "new@example.com");
    let error = missing.next();
    assert_eq!(error["event"], "error", "{error}");
    assert!(
        error["message"].as_str().unwrap().contains("no account matches"),
        "{error}"
    );
    let (status, stdout, _) = missing.finish();
    assert!(!status.success());
    assert_eq!(stdout.len(), 1, "{stdout:?}");
}

/// SIGTERM is how a program gives up on a login. It stops the CLI, removes
/// the throwaway directory with whatever credential was in it, and says so.
#[cfg(target_os = "linux")]
#[test]
fn a_terminated_login_stops_the_cli_and_leaves_nothing_behind() {
    let fixture = Fixture::new();
    let login = ProgramLogin::start(&fixture, &["add", "claude", "--json-io"], "new@example.com");
    login.next();
    login.next();
    let cli: u32 = std::fs::read_to_string(&login.pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    Command::new("kill")
        .args(["-TERM", &login.child.id().to_string()])
        .status()
        .unwrap();
    assert_eq!(
        login.next(),
        json!({"event": "error", "message": "Login cancelled. Nothing was added."})
    );
    let (status, _, _) = login.finish();
    assert!(!status.success());
    assert!(
        !Path::new(&format!("/proc/{cli}")).exists(),
        "the CLI must not outlive the login"
    );
    assert!(fixture.accounts().is_empty());
    no_login_left_behind(&fixture);
}

/// `login <account>` is also an ordinary command, for a person at a terminal:
/// the CLI has the terminal, and the code goes to it directly.
#[cfg(target_os = "linux")]
#[test]
fn login_signs_a_stored_account_in_again_at_a_terminal() {
    let fixture = Fixture::new();
    let at_terminal = |email: &str| {
        let mut command = fixture.cmd(&["login", "claude-1"]);
        with_fake_claude(&fixture, &mut command, email);
        let output = assert_cmd::Command::from_std(command)
            .write_stdin("good-code\n")
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stdout}{stderr}");
        (stdout, stderr)
    };

    let mut first = ProgramLogin::start(&fixture, &["add", "claude", "--json-io"], "new@example.com");
    first.next();
    first.next();
    first.send(r#"{"code": "good-code"}"#);
    first.next();
    assert!(first.finish().0.success());

    let (stdout, stderr) = at_terminal("new@example.com");
    assert!(
        stdout.contains("Signing in to claude-1 (new@example.com) again"),
        "{stdout}"
    );
    assert!(stdout.contains("Updated claude-1"), "{stdout}");
    assert!(!stderr.contains("rather than"), "{stderr}");

    // Signing in to some other account stores that one, and says so.
    let (stdout, stderr) = at_terminal("other@example.com");
    assert!(stdout.contains("Added claude-2"), "{stdout}");
    assert!(
        stderr.contains("That signed in to claude-2 rather than claude-1"),
        "{stderr}"
    );
    no_login_left_behind(&fixture);
}

/// The process id the stand-in `claude` of `login` wrote.
#[cfg(target_os = "linux")]
fn fake_claude_pid(login: &ProgramLogin) -> u32 {
    std::fs::read_to_string(&login.pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Once a code has been asked for, stdin closing before one is sent means
/// none ever can be: the program driving the login has gone. The login is
/// given up there and then, as on SIGTERM, rather than left holding a live
/// credential until the CLI's own timeout.
#[cfg(target_os = "linux")]
#[test]
fn closing_stdin_while_a_code_is_wanted_gives_the_login_up() {
    let fixture = Fixture::new();
    let given_up = json!({"event": "error", "message": "stdin closed while a code was needed"});

    let mut login = ProgramLogin::start(&fixture, &["add", "claude", "--json-io"], "new@example.com");
    assert_eq!(login.next()["event"], "url");
    assert_eq!(login.next()["event"], "need_code");
    drop(login.child.stdin.take());
    assert_eq!(login.next(), given_up);
    let cli = fake_claude_pid(&login);
    let (status, stdout, _) = login.finish();
    assert!(!status.success());
    assert_eq!(stdout.len(), 3, "{stdout:?}");
    assert!(
        !Path::new(&format!("/proc/{cli}")).exists(),
        "the CLI must not outlive the login"
    );
    no_login_left_behind(&fixture);

    // Closed from the start, it is given up as soon as a code is asked for.
    let closed = ProgramLogin::start_with(
        &fixture,
        &["add", "claude", "--json-io"],
        "new@example.com",
        |c| {
            c.stdin(std::process::Stdio::null());
        },
    );
    assert_eq!(closed.next()["event"], "url");
    assert_eq!(closed.next()["event"], "need_code");
    assert_eq!(closed.next(), given_up);
    assert!(!closed.finish().0.success());
    assert!(fixture.accounts().is_empty());
    no_login_left_behind(&fixture);

    // A code sent before stdin closes is all the login needs.
    let mut sent = ProgramLogin::start(&fixture, &["add", "claude", "--json-io"], "new@example.com");
    sent.next();
    sent.next();
    sent.send(r#"{"code": "good-code"}"#);
    drop(sent.child.stdin.take());
    assert_eq!(
        sent.next(),
        json!({"event": "done", "account": "claude-1", "added": true})
    );
    assert!(sent.finish().0.success());
}

/// A login the browser's callback finishes asks for no code, so it needs
/// nothing on stdin and is not given up when there is nothing there.
#[cfg(target_os = "linux")]
#[test]
fn a_login_that_asks_for_no_code_needs_no_stdin() {
    let fixture = Fixture::new();
    let login = ProgramLogin::start_with(
        &fixture,
        &["add", "claude", "--json-io"],
        "new@example.com",
        |c| {
            c.stdin(std::process::Stdio::null()).env("FAKE_NO_CODE", "1");
        },
    );
    assert_eq!(login.next()["event"], "url");
    assert_eq!(
        login.next(),
        json!({"event": "done", "account": "claude-1", "added": true})
    );
    assert!(login.finish().0.success());
    no_login_left_behind(&fixture);
}

/// A login a program drives ends with an `error` line even when it fails
/// before it starts, so the program always has one last line to read.
#[test]
fn a_json_login_that_cannot_start_still_ends_with_an_error_line() {
    let fixture = Fixture::new();
    let blocked = fixture._dir.path().join("not-a-directory");
    std::fs::write(&blocked, "not an account store").unwrap();
    for args in [
        &["add", "claude", "--json-io"][..],
        &["login", "claude-1", "--json-io"],
    ] {
        let output = fixture
            .cmd(args)
            .env("AGENT_METER_DIR", &blocked)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 1, "{args:?}: {stdout}");
        let event: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(event["event"], "error", "{event}");
        assert!(
            event["message"].as_str().unwrap().contains("not-a-directory"),
            "{event}"
        );
    }

    // Without --json-io it is an ordinary error, on stderr.
    let output = fixture
        .cmd(&["add", "claude"])
        .env("AGENT_METER_DIR", &blocked)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("not-a-directory")
    );
}

/// A settings file that does not parse is named in the `error` line, but the
/// line the parser stopped at is not repeated there: the file can hold
/// secrets, and whoever reads the program's output may be somewhere else.
#[test]
fn a_json_login_never_repeats_a_settings_file_that_does_not_parse() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(&fixture.data).unwrap();
    for broken in [
        "[remote.box]\nenv = { TOKEN = \"not-to-be-repeated\" \n",
        "[remote.box]\nenv = { TOKEN = 987654321 }\n",
    ] {
        std::fs::write(fixture.data.join("config.toml"), broken).unwrap();
        for args in [
            &["add", "claude", "--json-io"][..],
            &["login", "claude-1", "--json-io"],
        ] {
            let output = fixture
                .cmd(args)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(!output.status.success());
            let stdout = String::from_utf8(output.stdout).unwrap();
            let lines: Vec<&str> = stdout.lines().collect();
            assert_eq!(lines.len(), 1, "{args:?}: {stdout}");
            let event: Value = serde_json::from_str(lines[0]).unwrap();
            assert_eq!(event["event"], "error", "{event}");
            let message = event["message"].as_str().unwrap();
            assert!(message.contains("config.toml"), "{message}");
            assert!(message.contains("agent-meter config show"), "{message}");
            for quoted in ["TOKEN", "not-to-be-repeated", "987654321"] {
                assert!(!stdout.contains(quoted), "{args:?} repeated {quoted}: {stdout}");
            }
        }
    }
}
