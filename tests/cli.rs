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

    // Their copy is the newer one, so it is taken.
    write_json(&path, &record("refreshed-elsewhere", 2_000_000_000));
    fixture.run(&["import", "--from", "gemctl", "--dir", &dir]);
    assert_eq!(
        stored()["credential"]["refresh_token"],
        "refreshed-elsewhere",
        "{}",
        stored()
    );
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
    assert!(planned.contains("add"), "{planned}");
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
    assert_eq!(plan["home"], home.to_str().unwrap());
    assert_eq!(
        plan["transcript_roots"][0],
        home.join("projects").to_str().unwrap()
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
    assert_eq!(rows[0]["fleetHome"], home.to_str().unwrap());
}

#[cfg(unix)]
#[test]
fn fleet_concurrent_launches_cannot_rebind_a_conversation() {
    use std::os::unix::fs::symlink;
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
    symlink("/bin/true", bin.join("claude")).unwrap();
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
    assert!(!rows.to_string().contains("refresh_token"));
    f.cmd(&["list", "--poll", "--refresh"]).assert().failure();
}
