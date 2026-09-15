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
        write_json(
            &self.claude_home.join(".credentials.json"),
            &json!({
                "claudeAiOauth": {
                    "accessToken": format!("sk-ant-oat01-{refresh}"),
                    "refreshToken": format!("sk-ant-ort01-{refresh}"),
                    "expiresAt": 1_900_000_000_000i64,
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
