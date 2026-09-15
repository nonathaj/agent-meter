//! OpenAI Codex CLI.
//!
//! Codex keeps everything in one file, `<CODEX_HOME>/auth.json`. It reads that
//! file when it starts and not again, so swapping accounts under a running
//! Codex has no effect until it is restarted.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use serde_json::{Map, Value, json};

use super::{Provider, cli_command, config_home_from_env};
use crate::account::{Account, Captured, Credential, Identity, ProviderKind};
use crate::fsutil::{self, Mode};
use crate::usage::{Usage, Window};
use crate::{http, jwt};

pub struct Codex;

/// Environment variable Codex uses to relocate its configuration.
pub const CONFIG_HOME_ENV: &str = "CODEX_HOME";
const DEFAULT_HOME: &str = ".codex";
const AUTH_FILE: &str = "auth.json";

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// The Codex CLI's public OAuth client id.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Namespace of the custom claims OpenAI puts in its tokens.
const AUTH_CLAIMS: &str = "https://api.openai.com/auth";

impl Provider for Codex {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    fn config_home(&self) -> Result<PathBuf> {
        config_home_from_env(CONFIG_HOME_ENV, DEFAULT_HOME)
    }

    fn capture(&self, home: &Path) -> Result<Captured> {
        let path = auth_path(home);
        let blob = read_auth(home)?.ok_or_else(|| {
            anyhow!(
                "no Codex credential found at {}. Run `codex login` first, or use \
                 `agent-meter add codex` to log in to a fresh account.",
                path.display()
            )
        })?;

        match blob.get("auth_mode").and_then(Value::as_str) {
            None | Some("chatgpt") => {}
            Some(mode) => bail!(
                "the Codex credential in {} uses {mode:?} sign-in. agent-meter manages \
                 ChatGPT subscription accounts, whose usage the provider reports.",
                path.display()
            ),
        }
        if blob
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .is_some_and(|k| !k.is_empty())
        {
            bail!(
                "{} holds an API key rather than a subscription sign-in. API-key usage is \
                 billed per token, so there is no quota for agent-meter to meter.",
                path.display()
            );
        }

        let tokens = blob
            .get("tokens")
            .and_then(Value::as_object)
            .context("the Codex credential has no tokens block")?;
        let credential = parse_credential(tokens)?;
        let identity = identity_from_tokens(&credential, tokens);

        let mut provider_data = Map::new();
        for (key, value) in &blob {
            if key != "tokens" {
                provider_data.insert(key.clone(), value.clone());
            }
        }

        Ok(Captured {
            identity,
            credential,
            provider_data,
        })
    }

    fn install(&self, home: &Path, account: &Account) -> Result<()> {
        // Keep whatever else the live file holds: Codex writes settings there
        // that have nothing to do with which account is signed in.
        let mut blob = read_auth(home)?.unwrap_or_default();
        for (key, value) in &account.provider_data {
            blob.insert(key.clone(), value.clone());
        }
        blob.insert("tokens".into(), Value::Object(tokens_object(account)));
        blob.insert("auth_mode".into(), "chatgpt".into());
        blob.insert("OPENAI_API_KEY".into(), Value::Null);
        blob.insert("last_refresh".into(), Timestamp::now().to_string().into());

        let path = auth_path(home);
        fsutil::create_private_dir(home).with_context(|| format!("creating {}", home.display()))?;
        let mut json = serde_json::to_vec_pretty(&blob).context("serializing the Codex credential")?;
        json.push(b'\n');
        // Inherit the existing permissions: on Windows the Codex sandbox reads
        // auth.json through an ACL inherited from its directory, and replacing
        // that with a tighter one would lock the sandbox out.
        fsutil::write_atomic(&path, &json, Mode::InheritExisting)
            .with_context(|| format!("writing {}", path.display()))
    }

    fn login_command(&self, home: &Path, device_code: bool) -> Result<Command> {
        fsutil::create_private_dir(home).with_context(|| format!("creating {}", home.display()))?;
        let mut command = cli_command("codex", CONFIG_HOME_ENV, home)?;
        command.arg("login");
        if device_code {
            command.arg("--device-auth");
        }
        Ok(command)
    }

    fn fetch_usage(&self, credential: &Credential) -> http::Result<Usage> {
        let response: Value = http::get_json(
            USAGE_URL,
            &[("authorization", &http::bearer(&credential.access_token))],
        )?;
        Ok(parse_usage(&response, Timestamp::now()))
    }

    fn fetch_identity(&self, credential: &Credential) -> http::Result<Identity> {
        // The usage endpoint states the account's identity, so there is no
        // separate profile call to make; the tokens carry the rest.
        let response: Value = http::get_json(
            USAGE_URL,
            &[("authorization", &http::bearer(&credential.access_token))],
        )?;
        let mut identity = identity_from_tokens(credential, &Map::new());
        identity.update_from(&parse_usage_identity(&response));
        Ok(identity)
    }

    fn refresh(&self, credential: &Credential) -> http::Result<Credential> {
        let body = json!({
            "grant_type": "refresh_token",
            "refresh_token": credential.refresh_token,
            "client_id": CLIENT_ID,
        });
        let response: Value = http::post_json(TOKEN_URL, body)?;
        parse_token_response(&response, credential)
            .map_err(|e| http::Error::Transport(e.context("reading the refreshed Codex credential")))
    }

    fn restarts_sessions(&self) -> bool {
        // Codex reads auth.json once at startup.
        true
    }
}

fn auth_path(home: &Path) -> PathBuf {
    home.join(AUTH_FILE)
}

fn read_auth(home: &Path) -> Result<Option<Map<String, Value>>> {
    let path = auth_path(home);
    let Some(bytes) = fsutil::read_optional(&path).with_context(|| format!("reading {}", path.display()))?
    else {
        return Ok(None);
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => bail!("{} is not a JSON object", path.display()),
    }
}

fn parse_credential(tokens: &Map<String, Value>) -> Result<Credential> {
    let token = |key: &str| tokens.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let access_token = token("access_token").context("the Codex credential has no access token")?;
    Ok(Credential {
        // Codex states no expiry of its own; the access token is a JWT that
        // carries one.
        expires_at: jwt::expiry(access_token),
        access_token: access_token.to_string(),
        refresh_token: token("refresh_token")
            .context(
                "the Codex credential has no refresh token, so agent-meter could not keep it \
                 alive. Sign in again with `codex login`.",
            )?
            .to_string(),
        id_token: token("id_token").map(ToString::to_string),
        // The refresh token is opaque, so its lifetime is unknown.
        refresh_expires_at: None,
    })
}

/// Builds the `tokens` block Codex expects.
fn tokens_object(account: &Account) -> Map<String, Value> {
    let mut tokens = Map::new();
    tokens.insert(
        "access_token".into(),
        account.credential.access_token.clone().into(),
    );
    tokens.insert(
        "refresh_token".into(),
        account.credential.refresh_token.clone().into(),
    );
    if let Some(id_token) = &account.credential.id_token {
        tokens.insert("id_token".into(), id_token.clone().into());
    }
    if let Some(account_id) = &account.identity.workspace_id {
        tokens.insert("account_id".into(), account_id.clone().into());
    }
    tokens
}

/// Derives identity from the tokens themselves, which carry the signed-in
/// user's email, ChatGPT ids and plan.
fn identity_from_tokens(credential: &Credential, tokens: &Map<String, Value>) -> Identity {
    let claims = credential
        .id_token
        .as_deref()
        .and_then(jwt::claims)
        .or_else(|| jwt::claims(&credential.access_token))
        .unwrap_or(Value::Null);
    let auth = claims.get(AUTH_CLAIMS);
    let auth_claim = |key: &str| {
        auth?
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
    };

    Identity {
        user_id: auth_claim("chatgpt_user_id").or_else(|| auth_claim("user_id")),
        email: claims
            .get("email")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        workspace_id: tokens
            .get("account_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| auth_claim("chatgpt_account_id")),
        workspace_name: None,
        plan: plan_word(auth_claim("chatgpt_plan_type").as_deref()),
        // ChatGPT states no quota multiplier, so there is nothing to say about
        // the size of this account's tank.
        capacity: None,
    }
}

/// The plan as one of our own words.
///
/// Known values are matched, not echoed. An unrecognised one is not printed
/// verbatim either: this string reaches a terminal row, and one carrying escape
/// sequences could repaint it. It is reduced to plain lowercase ASCII, which is
/// what every plan name the provider actually uses already is, or dropped when
/// nothing recognisable survives.
fn plan_word(plan: Option<&str>) -> Option<String> {
    let plan = plan?.trim();
    let known = ["free", "plus", "pro", "team", "business", "enterprise", "edu"];
    if let Some(word) = known.iter().find(|word| plan.eq_ignore_ascii_case(word)) {
        return Some((*word).to_string());
    }
    let sanitised: String = plan
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(16)
        .collect::<String>()
        .to_ascii_lowercase();
    (!sanitised.is_empty()).then_some(sanitised)
}

/// Reads the identity fields the usage endpoint reports.
fn parse_usage_identity(response: &Value) -> Identity {
    let string = |key: &str| {
        response
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
    };
    Identity {
        user_id: string("user_id"),
        email: string("email"),
        workspace_id: string("account_id"),
        workspace_name: None,
        plan: plan_word(string("plan_type").as_deref()),
        capacity: None,
    }
}

/// Reads `/backend-api/wham/usage`.
///
/// Each limit is reported as up to two windows whose lengths are stated in the
/// payload, so the window kind is read from `limit_window_seconds` rather than
/// assumed from its position.
fn parse_usage(response: &Value, observed_at: Timestamp) -> Usage {
    let mut windows = Vec::new();
    if let Some(rate_limit) = response.get("rate_limit") {
        collect_windows(rate_limit, None, &mut windows);
    }
    if let Some(extra) = response.get("additional_rate_limits").and_then(Value::as_array) {
        for entry in extra {
            let scope = entry.get("limit_name").and_then(Value::as_str);
            if let Some(rate_limit) = entry.get("rate_limit") {
                collect_windows(rate_limit, scope, &mut windows);
            }
        }
    }
    let limit_reached = response
        .pointer("/rate_limit/limit_reached")
        .and_then(Value::as_bool)
        .or_else(|| {
            response
                .pointer("/rate_limit/allowed")
                .and_then(Value::as_bool)
                .map(|a| !a)
        })
        .unwrap_or(false);

    Usage {
        observed_at,
        windows,
        limit_reached,
    }
}

fn collect_windows(rate_limit: &Value, scope: Option<&str>, windows: &mut Vec<Window>) {
    for key in ["primary_window", "secondary_window"] {
        let Some(window) = rate_limit.get(key).filter(|v| v.is_object()) else {
            continue;
        };
        let Some(used_percent) = window.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let Some(window_secs) = window.get("limit_window_seconds").and_then(Value::as_u64) else {
            continue;
        };
        windows.push(Window {
            window_secs,
            scope: scope.map(ToString::to_string),
            used_percent,
            resets_at: window
                .get("reset_at")
                .and_then(Value::as_i64)
                .and_then(|s| Timestamp::from_second(s).ok()),
        });
    }
}

fn parse_token_response(response: &Value, previous: &Credential) -> Result<Credential> {
    let string = |key: &str| {
        response
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let access_token = string("access_token")
        .context("the token response contained no access token")?
        .to_string();
    let expires_at = jwt::expiry(&access_token).or_else(|| {
        response
            .get("expires_in")
            .and_then(Value::as_i64)
            .map(|secs| Timestamp::now() + jiff::SignedDuration::from_secs(secs))
    });
    Ok(Credential {
        access_token,
        refresh_token: string("refresh_token")
            .unwrap_or(&previous.refresh_token)
            .to_string(),
        id_token: string("id_token")
            .map(ToString::to_string)
            .or_else(|| previous.id_token.clone()),
        expires_at,
        refresh_expires_at: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::SCHEMA_VERSION;
    use crate::usage::{FIVE_HOURS, ONE_WEEK};

    fn id_token(email: &str, user: &str, account: &str, plan: &str) -> String {
        jwt::encode_unsigned(&json!({
            "email": email,
            "exp": 1_800_000_000,
            AUTH_CLAIMS: {
                "chatgpt_user_id": user,
                "chatgpt_account_id": account,
                "chatgpt_plan_type": plan
            }
        }))
    }

    fn auth_json() -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id_token("dev@example.com", "user-1", "acct-1", "pro"),
                "access_token": jwt::encode_unsigned(&json!({"exp": 1_800_000_000})),
                "refresh_token": "refresh-old",
                "account_id": "acct-1"
            },
            "last_refresh": "2026-09-09T02:30:16.972224500Z"
        })
    }

    fn write_home(blob: &Value) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(AUTH_FILE), serde_json::to_vec(blob).unwrap()).unwrap();
        (tmp, home)
    }

    #[test]
    fn captures_identity_from_the_id_token() {
        let (_tmp, home) = write_home(&auth_json());
        let captured = Codex.capture(&home).unwrap();
        assert_eq!(captured.identity.email.as_deref(), Some("dev@example.com"));
        assert_eq!(captured.identity.user_id.as_deref(), Some("user-1"));
        assert_eq!(captured.identity.workspace_id.as_deref(), Some("acct-1"));
        assert_eq!(captured.identity.plan.as_deref(), Some("pro"));
        assert_eq!(captured.credential.expires_at.unwrap().as_second(), 1_800_000_000);
        assert_eq!(
            captured.provider_data["last_refresh"],
            "2026-09-09T02:30:16.972224500Z"
        );
    }

    /// The plan reaches a terminal row, so a provider string never does.
    #[test]
    fn plan_names_are_matched_or_reduced_to_plain_text() {
        assert_eq!(plan_word(Some("pro")).as_deref(), Some("pro"));
        assert_eq!(plan_word(Some("Team")).as_deref(), Some("team"));
        assert_eq!(plan_word(Some(" enterprise ")).as_deref(), Some("enterprise"));

        // Unknown but plausible: kept, in plain lowercase ASCII.
        assert_eq!(plan_word(Some("pro-max")).as_deref(), Some("pro-max"));

        // An escape sequence or an override character cannot repaint the row.
        assert_eq!(plan_word(Some("\u{1b}[31mpro")).as_deref(), Some("31mpro"));
        assert_eq!(plan_word(Some("pro\u{202e}x")).as_deref(), Some("prox"));
        assert_eq!(plan_word(Some("\u{1b}[2J")).as_deref(), Some("2j"));
        assert!(plan_word(Some("\u{202e}\u{1b}")).is_none());
        assert!(plan_word(Some("")).is_none());
        assert!(plan_word(None).is_none());

        // Bounded, so a long string cannot stretch the row either.
        assert_eq!(plan_word(Some(&"a".repeat(100))).unwrap().len(), 16);
    }

    #[test]
    fn refuses_api_key_and_unknown_sign_in_modes() {
        let mut blob = auth_json();
        blob["OPENAI_API_KEY"] = json!("sk-proj-abc");
        let (_tmp, home) = write_home(&blob);
        let err = Codex.capture(&home).unwrap_err().to_string();
        assert!(err.contains("API key"), "{err}");

        let mut blob = auth_json();
        blob["auth_mode"] = json!("apikey");
        let (_tmp, home) = write_home(&blob);
        let err = Codex.capture(&home).unwrap_err().to_string();
        assert!(err.contains("sign-in"), "{err}");
    }

    #[test]
    fn missing_credential_names_the_path_and_the_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Codex.capture(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("codex login"), "{err}");
    }

    #[test]
    fn install_replaces_tokens_and_keeps_unrelated_settings() {
        let mut blob = auth_json();
        blob["some_codex_setting"] = json!({"keep": true});
        let (_tmp, home) = write_home(&blob);

        let account = Account {
            schema_version: SCHEMA_VERSION,
            id: "codex-2".into(),
            provider: ProviderKind::Codex,
            label: None,
            identity: Identity {
                workspace_id: Some("acct-2".into()),
                ..Default::default()
            },
            credential: Credential {
                access_token: "new-access".into(),
                refresh_token: "new-refresh".into(),
                id_token: Some("new-id".into()),
                expires_at: None,
                refresh_expires_at: None,
            },
            provider_data: Map::new(),
            added_at: Timestamp::from_second(0).unwrap(),
            needs_login: None,
        };
        Codex.install(&home, &account).unwrap();

        let written: Value = serde_json::from_slice(&std::fs::read(home.join(AUTH_FILE)).unwrap()).unwrap();
        assert_eq!(written["tokens"]["access_token"], "new-access");
        assert_eq!(written["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(written["tokens"]["account_id"], "acct-2");
        assert_eq!(written["auth_mode"], "chatgpt");
        assert!(written["OPENAI_API_KEY"].is_null());
        assert_eq!(written["some_codex_setting"]["keep"], true);
        assert_ne!(written["last_refresh"], auth_json()["last_refresh"]);
    }

    #[test]
    fn parses_usage_windows_by_stated_length() {
        let response = json!({
            "user_id": "user-1",
            "email": "dev@example.com",
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 0, "limit_window_seconds": 18000, "reset_at": 1789497203},
                "secondary_window": {"used_percent": 8, "limit_window_seconds": 604800, "reset_at": 1790008763}
            },
            "additional_rate_limits": [{
                "limit_name": "GPT-5.3-Codex-Spark",
                "rate_limit": {
                    "primary_window": {"used_percent": 42, "limit_window_seconds": 18000, "reset_at": 1789497203}
                }
            }]
        });
        let now = Timestamp::from_second(1_789_000_000).unwrap();
        let usage = parse_usage(&response, now);
        assert_eq!(usage.windows.len(), 3);
        assert_eq!(usage.windows[0].window_secs, FIVE_HOURS);
        assert_eq!(usage.windows[1].window_secs, ONE_WEEK);
        assert_eq!(usage.windows[2].label(), "5h GPT-5.3-Codex-Spark");
        assert_eq!(usage.used_at(now), 42.0);
        assert!(!usage.limit_reached);

        assert_eq!(parse_usage_identity(&response).email.unwrap(), "dev@example.com");
    }

    #[test]
    fn a_refused_account_is_reported_as_limit_reached() {
        let response = json!({"rate_limit": {"allowed": false, "limit_reached": true}});
        let usage = parse_usage(&response, Timestamp::from_second(1).unwrap());
        assert!(usage.limit_reached);
        assert!(usage.is_exhausted_at(Timestamp::from_second(1).unwrap()));
    }

    #[test]
    fn token_response_prefers_the_jwt_expiry() {
        let previous = Credential {
            access_token: "old".into(),
            refresh_token: "old-refresh".into(),
            id_token: Some("old-id".into()),
            expires_at: None,
            refresh_expires_at: None,
        };
        let access = jwt::encode_unsigned(&json!({"exp": 1_900_000_000}));
        let refreshed =
            parse_token_response(&json!({"access_token": access, "expires_in": 60}), &previous).unwrap();
        assert_eq!(refreshed.expires_at.unwrap().as_second(), 1_900_000_000);
        assert_eq!(refreshed.refresh_token, "old-refresh");
        assert_eq!(refreshed.id_token.as_deref(), Some("old-id"));
    }
}
