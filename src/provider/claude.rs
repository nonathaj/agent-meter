//! Claude Code.
//!
//! Claude Code keeps its OAuth credential in `<home>/.credentials.json` (or the
//! macOS login Keychain) and its account identity in a separate `.claude.json`
//! that also holds unrelated application state. Both files are shared with a
//! running Claude Code, which re-reads them between messages, so writes are
//! merges taken under Claude Code's own lock files.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::secret_store::{self, Backend};
use super::{Provider, cli_command, config_home_from_env};
use crate::account::{Account, Captured, Credential, Identity, ProviderKind};
use crate::fsutil::{self, Mode};
use crate::http;
use crate::lock::DirLock;
use crate::usage::{FIVE_HOURS, ONE_WEEK, Usage, Window};

pub struct Claude;

/// Environment variable Claude Code uses to relocate its configuration.
pub const CONFIG_HOME_ENV: &str = "CLAUDE_CONFIG_DIR";
const DEFAULT_HOME: &str = ".claude";
const CREDENTIALS_FILE: &str = ".credentials.json";
const IDENTITY_FILE: &str = ".claude.json";

/// Key inside `.credentials.json` holding the OAuth credential.
const OAUTH_KEY: &str = "claudeAiOauth";
/// Key inside `.claude.json` describing the signed-in account.
const ACCOUNT_KEY: &str = "oauthAccount";

/// Keys in `.credentials.json` that belong to the machine rather than to the
/// account, and so must stay behind when accounts are swapped.
const MACHINE_KEYS: &[&str] = &[
    "mcpOAuth",
    "mcpOAuthClientConfig",
    "mcpXaaIdp",
    "mcpXaaIdpConfig",
    "pluginSecrets",
];

/// Base name of the macOS Keychain item Claude Code stores its credential in.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code's public OAuth client id. Not a secret; it identifies the CLI.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Opt-in header required by the OAuth-scoped endpoints.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// How long a Claude Code lock directory may sit untouched before it is assumed
/// to belong to a process that died. These mirror Claude Code's own values.
const CREDENTIAL_LOCK_STALE: Duration = Duration::from_secs(60);
const IDENTITY_LOCK_STALE: Duration = Duration::from_secs(10);
/// Long enough to outlast a Claude Code token refresh, short enough that a swap
/// never looks hung.
const LOCK_TIMEOUT: Duration = Duration::from_secs(9);

impl Provider for Claude {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    fn config_home(&self) -> Result<PathBuf> {
        config_home_from_env(CONFIG_HOME_ENV, DEFAULT_HOME)
    }

    fn capture(&self, home: &Path) -> Result<Captured> {
        let blob = read_credentials(home)?.ok_or_else(|| {
            anyhow!(
                "no Claude Code credential found in {}. Run `claude auth login` first, \
                 or use `agent-meter add claude` to log in to a fresh account.",
                home.display()
            )
        })?;
        let oauth = blob.get(OAUTH_KEY).and_then(Value::as_object).ok_or_else(|| {
            anyhow!(
                "the Claude Code credential in {} has no {OAUTH_KEY} block; \
                 it may be an API-key login, which agent-meter cannot meter",
                home.display()
            )
        })?;

        let credential = parse_credential(oauth)?;
        let identity_file = read_identity_file(home)?;
        let account = identity_file
            .as_ref()
            .and_then(|v| v.get(ACCOUNT_KEY))
            .and_then(Value::as_object);
        let mut identity = parse_identity(account);
        if identity.plan.is_none() {
            identity.plan = oauth
                .get("subscriptionType")
                .and_then(Value::as_str)
                .map(Into::into);
        }

        // Keep the fields Claude Code expects to find again after a swap: the
        // identity block verbatim, plus the account-scoped extras that live
        // alongside the tokens.
        let mut provider_data = Map::new();
        if let Some(account) = account {
            provider_data.insert(ACCOUNT_KEY.into(), Value::Object(account.clone()));
        }
        let extras: Map<String, Value> = oauth
            .iter()
            .filter(|(k, _)| {
                !matches!(
                    k.as_str(),
                    "accessToken" | "refreshToken" | "expiresAt" | "refreshTokenExpiresAt"
                )
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !extras.is_empty() {
            provider_data.insert("oauthExtras".into(), Value::Object(extras));
        }
        for (key, value) in &blob {
            if key != OAUTH_KEY && !MACHINE_KEYS.contains(&key.as_str()) {
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
        let _locks = lock_claude_code(home)?;

        let live = read_credentials(home)?.unwrap_or_default();
        let merged = merge_credentials(&live, account);
        write_credentials(home, &merged)?;
        install_identity(home, account)?;
        Ok(())
    }

    fn login_command(&self, home: &Path, device_code: bool) -> Result<Command> {
        anyhow::ensure!(!device_code, "Claude Code has no device-code login flow");
        seed_login_home(home)?;
        let mut command = cli_command("claude", CONFIG_HOME_ENV, home)?;
        command.args(["auth", "login"]);
        Ok(command)
    }

    fn fetch_usage(&self, credential: &Credential) -> http::Result<Usage> {
        let response: Value = http::get_json(
            USAGE_URL,
            &[
                ("authorization", &http::bearer(&credential.access_token)),
                ("anthropic-beta", OAUTH_BETA),
            ],
        )?;
        Ok(parse_usage(&response, Timestamp::now()))
    }

    fn fetch_identity(&self, credential: &Credential) -> http::Result<Identity> {
        let response: Value = http::get_json(
            PROFILE_URL,
            &[("authorization", &http::bearer(&credential.access_token))],
        )?;
        Ok(parse_profile(&response))
    }

    fn refresh(&self, credential: &Credential) -> http::Result<Credential> {
        let body = json!({
            "grant_type": "refresh_token",
            "refresh_token": credential.refresh_token,
            "client_id": CLIENT_ID,
        });
        let response: Value = http::post_json(TOKEN_URL, body)?;
        parse_token_response(&response, credential)
            .map_err(|e| http::Error::Transport(e.context("reading the refreshed Claude credential")))
    }

    fn discard_home(&self, home: &Path) -> Result<()> {
        // On macOS the login wrote a Keychain item keyed to this directory;
        // deleting the directory alone would leave it behind forever.
        if cfg!(target_os = "macos") && secret_store::is_available() {
            secret_store::delete(&keychain_service(home)?)?;
        }
        fsutil::remove_dir_all_if_exists(home).with_context(|| format!("removing {}", home.display()))
    }

    fn restarts_sessions(&self) -> bool {
        // Claude Code re-reads its credential between messages, so a running
        // session picks up the new account on its own.
        false
    }
}

/// Path of the file holding the OAuth credential.
fn credentials_path(home: &Path) -> PathBuf {
    home.join(CREDENTIALS_FILE)
}

/// Path of the file holding the account identity.
///
/// Claude Code looks for `$CLAUDE_CONFIG_DIR/.claude.json`, and when that
/// variable is unset for `$HOME/.claude.json` — a sibling of the default
/// `~/.claude` home, not a file inside it.
fn identity_path(home: &Path) -> Result<PathBuf> {
    let default_home = crate::paths::home_dir()?.join(DEFAULT_HOME);
    if home == default_home {
        Ok(crate::paths::home_dir()?.join(IDENTITY_FILE))
    } else {
        Ok(home.join(IDENTITY_FILE))
    }
}

/// Which store holds the credential for `home` on this machine.
fn backend(home: &Path) -> Result<Backend> {
    if cfg!(target_os = "macos") && secret_store::is_available() {
        // A Keychain item shadows the file: Claude Code reads the Keychain
        // first and only falls back to the file when the item is missing.
        if secret_store::read(&keychain_service(home)?)?.is_some() {
            return Ok(Backend::Keychain);
        }
        // No item yet: a fresh macOS install still writes to the Keychain.
        if !credentials_path(home).exists() {
            return Ok(Backend::Keychain);
        }
    }
    Ok(Backend::File)
}

/// The Keychain service name Claude Code uses for `home`.
///
/// The default home uses the bare service name; any other home appends the
/// first 8 hex digits of the SHA-256 of the directory string, so profiles do
/// not collide.
fn keychain_service(home: &Path) -> Result<String> {
    let default_home = crate::paths::home_dir()?.join(DEFAULT_HOME);
    if home == default_home {
        return Ok(KEYCHAIN_SERVICE.to_string());
    }
    let normalized: String = home.to_string_lossy().nfc().collect();
    let digest = Sha256::digest(normalized.as_bytes());
    let suffix: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    Ok(format!("{KEYCHAIN_SERVICE}-{suffix}"))
}

/// Reads the raw credential blob for `home`, from whichever store holds it.
fn read_credentials(home: &Path) -> Result<Option<Map<String, Value>>> {
    let text = match backend(home)? {
        Backend::Keychain => secret_store::read(&keychain_service(home)?)?,
        Backend::File => {
            let path = credentials_path(home);
            fsutil::read_optional(&path)
                .with_context(|| format!("reading {}", path.display()))?
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        }
    };
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(&text).context("parsing the Claude Code credential")?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => bail!("the Claude Code credential is not a JSON object"),
    }
}

/// Writes the credential blob for `home`.
fn write_credentials(home: &Path, blob: &Map<String, Value>) -> Result<()> {
    let text = serde_json::to_string_pretty(blob).context("serializing the Claude Code credential")?;
    let path = credentials_path(home);
    match backend(home)? {
        Backend::Keychain => {
            secret_store::write(&keychain_service(home)?, &text)?;
            // Claude Code caches the Keychain read for a short time but watches
            // the file's mtime. Rewriting an existing file (never creating one)
            // makes a running session notice the swap immediately.
            if path.exists() {
                fsutil::write_atomic(&path, text.as_bytes(), Mode::InheritExisting)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
        }
        Backend::File => fsutil::write_atomic(&path, text.as_bytes(), Mode::InheritExisting)
            .with_context(|| format!("writing {}", path.display()))?,
    }
    Ok(())
}

/// Reads `.claude.json`, if it exists and parses.
fn read_identity_file(home: &Path) -> Result<Option<Value>> {
    let path = identity_path(home)?;
    let Some(bytes) = fsutil::read_optional(&path).with_context(|| format!("reading {}", path.display()))?
    else {
        return Ok(None);
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Replaces only the `oauthAccount` block of `.claude.json`, leaving the
/// hundreds of unrelated settings in that file untouched.
fn install_identity(home: &Path, account: &Account) -> Result<()> {
    let Some(block) = account.provider_data.get(ACCOUNT_KEY).cloned() else {
        return Ok(());
    };
    let path = identity_path(home)?;
    let mut root = match fsutil::read_optional(&path).with_context(|| format!("reading {}", path.display()))? {
        Some(bytes) => serde_json::from_slice::<Value>(&bytes).with_context(|| {
            format!(
                "{} is not valid JSON. Claude Code owns this file; fix or remove it before swapping accounts.",
                path.display()
            )
        })?,
        None => json!({}),
    };
    let Some(map) = root.as_object_mut() else {
        bail!("{} is not a JSON object", path.display());
    };
    map.insert(ACCOUNT_KEY.to_string(), block);

    let mut json = serde_json::to_vec_pretty(&root).context("serializing the Claude Code settings")?;
    json.push(b'\n');
    fsutil::write_atomic(&path, &json, Mode::InheritExisting)
        .with_context(|| format!("writing {}", path.display()))
}

/// Builds the credential blob to install: the stored account's tokens, the
/// machine's own secrets, and whatever else the account carried.
fn merge_credentials(live: &Map<String, Value>, account: &Account) -> Map<String, Value> {
    let mut oauth = Map::new();
    if let Some(Value::Object(extras)) = account.provider_data.get("oauthExtras") {
        oauth.extend(extras.clone());
    }
    let credential = &account.credential;
    oauth.insert("accessToken".into(), credential.access_token.clone().into());
    oauth.insert("refreshToken".into(), credential.refresh_token.clone().into());
    if let Some(expires) = credential.expires_at {
        oauth.insert("expiresAt".into(), expires.as_millisecond().into());
    }
    if let Some(expires) = credential.refresh_expires_at {
        oauth.insert("refreshTokenExpiresAt".into(), expires.as_millisecond().into());
    }

    let mut blob = Map::new();
    for (key, value) in &account.provider_data {
        if key != "oauthExtras" && key != ACCOUNT_KEY {
            blob.insert(key.clone(), value.clone());
        }
    }
    blob.insert(OAUTH_KEY.into(), Value::Object(oauth));
    for key in MACHINE_KEYS {
        if let Some(value) = live.get(*key) {
            blob.insert((*key).into(), value.clone());
        }
    }
    // An API key would take precedence over the OAuth credential we just wrote.
    blob.remove("primaryApiKey");
    blob
}

/// Writes the minimum settings that stop Claude Code from showing its
/// first-run onboarding inside a throwaway login directory.
fn seed_login_home(home: &Path) -> Result<()> {
    fsutil::create_private_dir(home).with_context(|| format!("creating {}", home.display()))?;
    let path = identity_path(home)?;
    if path.exists() {
        return Ok(());
    }
    let seed = json!({"hasCompletedOnboarding": true, "theme": "dark"});
    let json = format!("{}\n", serde_json::to_string_pretty(&seed)?);
    fsutil::write_atomic(&path, json.as_bytes(), Mode::Private)
        .with_context(|| format!("writing {}", path.display()))
}

/// Takes the lock files Claude Code itself uses, so it cannot refresh or
/// rewrite the credential while it is being replaced.
///
/// The locks are released when the returned value is dropped.
fn lock_claude_code(home: &Path) -> Result<Vec<DirLock>> {
    let mut locks = Vec::new();
    let mut take = |path: PathBuf, stale: Duration| -> Result<()> {
        locks.push(DirLock::acquire(path, stale, LOCK_TIMEOUT)?);
        Ok(())
    };
    take(home.join(".oauth_refresh.lock"), CREDENTIAL_LOCK_STALE)?;
    // Claude Code's older credential lock is a sibling of the home directory.
    let sibling = home.with_file_name(format!(
        "{}.lock",
        home.file_name().unwrap_or_default().to_string_lossy()
    ));
    take(sibling, CREDENTIAL_LOCK_STALE)?;
    let identity = identity_path(home)?;
    take(
        identity.with_file_name(format!(
            "{}.lock",
            identity.file_name().unwrap_or_default().to_string_lossy()
        )),
        IDENTITY_LOCK_STALE,
    )?;
    Ok(locks)
}

/// Reads the tokens out of a `claudeAiOauth` block. Expiries there are epoch
/// milliseconds.
fn parse_credential(oauth: &Map<String, Value>) -> Result<Credential> {
    let token = |key: &str| oauth.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let millis = |key: &str| {
        oauth
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|ms| Timestamp::from_millisecond(ms).ok())
    };
    Ok(Credential {
        access_token: token("accessToken")
            .context("the Claude Code credential has no access token")?
            .to_string(),
        refresh_token: token("refreshToken")
            .context(
                "the Claude Code credential has no refresh token, so agent-meter could not \
                 keep it alive. Sign in again with `claude auth login`.",
            )?
            .to_string(),
        id_token: None,
        expires_at: millis("expiresAt"),
        refresh_expires_at: millis("refreshTokenExpiresAt"),
    })
}

fn parse_identity(account: Option<&Map<String, Value>>) -> Identity {
    let get = |key: &str| {
        account
            .and_then(|a| a.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
    };
    Identity {
        user_id: get("accountUuid"),
        email: get("emailAddress"),
        workspace_id: get("organizationUuid"),
        workspace_name: get("organizationName"),
        plan: None,
    }
}

/// Reads `/api/oauth/profile`.
fn parse_profile(response: &Value) -> Identity {
    let account = response.get("account");
    let organization = response.get("organization");
    let string = |value: Option<&Value>, key: &str| {
        value?
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
    };
    let flag = |key: &str| {
        account
            .and_then(|a| a.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    Identity {
        user_id: string(account, "uuid"),
        email: string(account, "email").or_else(|| string(account, "email_address")),
        workspace_id: string(organization, "uuid"),
        workspace_name: string(organization, "name"),
        plan: if flag("has_claude_max") {
            Some("max".into())
        } else if flag("has_claude_pro") {
            Some("pro".into())
        } else {
            None
        },
    }
}

/// Reads `/api/oauth/usage`.
///
/// The response describes the same limits twice: a `limits` array and a set of
/// named top-level objects. The array is preferred because it names per-model
/// windows; the named objects are the fallback for older responses.
fn parse_usage(response: &Value, observed_at: Timestamp) -> Usage {
    let mut windows = Vec::new();

    if let Some(limits) = response.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let Some(window_secs) = limit.get("group").and_then(Value::as_str).and_then(group_seconds) else {
                continue;
            };
            let Some(used_percent) = limit.get("percent").and_then(Value::as_f64) else {
                continue;
            };
            windows.push(Window {
                window_secs,
                scope: limit
                    .pointer("/scope/model/display_name")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                used_percent,
                resets_at: limit.get("resets_at").and_then(parse_rfc3339),
            });
        }
    }

    if windows.is_empty() {
        for (key, window_secs) in [("five_hour", FIVE_HOURS), ("seven_day", ONE_WEEK)] {
            let Some(block) = response.get(key).filter(|v| v.is_object()) else {
                continue;
            };
            let Some(used_percent) = block.get("utilization").and_then(Value::as_f64) else {
                continue;
            };
            windows.push(Window {
                window_secs,
                scope: None,
                used_percent,
                resets_at: block.get("resets_at").and_then(parse_rfc3339),
            });
        }
    }

    Usage {
        observed_at,
        windows,
        limit_reached: false,
    }
}

fn group_seconds(group: &str) -> Option<u64> {
    match group {
        "session" => Some(FIVE_HOURS),
        "weekly" => Some(ONE_WEEK),
        _ => None,
    }
}

fn parse_rfc3339(value: &Value) -> Option<Timestamp> {
    value.as_str()?.parse().ok()
}

/// Applies a token response to the credential it refreshed. Anthropic rotates
/// refresh tokens, but only returns one when it did.
fn parse_token_response(response: &Value, previous: &Credential) -> Result<Credential> {
    let access_token = response
        .get("access_token")
        .and_then(Value::as_str)
        .context("the token response contained no access token")?
        .to_string();
    let refresh_token = response
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| previous.refresh_token.clone());
    let expires_at = response
        .get("expires_in")
        .and_then(Value::as_i64)
        .map(|secs| Timestamp::now() + jiff::SignedDuration::from_secs(secs));
    Ok(Credential {
        access_token,
        refresh_token,
        id_token: None,
        expires_at: expires_at.or(previous.expires_at),
        refresh_expires_at: previous.refresh_expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::SCHEMA_VERSION;

    fn credentials_json() -> Value {
        json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-old",
                "refreshToken": "sk-ant-ort01-old",
                "expiresAt": 1_789_203_929_051i64,
                "refreshTokenExpiresAt": 1_791_657_650_051i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max",
                "rateLimitTier": "default_claude_max_20x"
            },
            "mcpOAuth": {"server": "machine-scoped"},
            "pluginSecrets": {"a": "b"},
            "trustedDeviceToken": "account-scoped"
        })
    }

    fn account_with(credential: Credential, provider_data: Map<String, Value>) -> Account {
        Account {
            schema_version: SCHEMA_VERSION,
            id: "claude-1".into(),
            provider: ProviderKind::Claude,
            label: None,
            identity: Identity::default(),
            credential,
            provider_data,
            added_at: Timestamp::from_second(0).unwrap(),
            needs_login: None,
        }
    }

    #[test]
    fn parses_credential_millisecond_expiries() {
        let blob = credentials_json();
        let oauth = blob[OAUTH_KEY].as_object().unwrap();
        let credential = parse_credential(oauth).unwrap();
        assert_eq!(credential.access_token, "sk-ant-oat01-old");
        assert_eq!(credential.expires_at.unwrap().as_millisecond(), 1_789_203_929_051);
        assert_eq!(
            credential.refresh_expires_at.unwrap().as_millisecond(),
            1_791_657_650_051
        );
    }

    #[test]
    fn credential_without_refresh_token_is_rejected() {
        let oauth = json!({"accessToken": "only-access"});
        let err = parse_credential(oauth.as_object().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("refresh token"), "{err}");
    }

    #[test]
    fn merge_keeps_machine_secrets_and_replaces_account_scoped_ones() {
        let live = credentials_json().as_object().unwrap().clone();
        let mut provider_data = Map::new();
        provider_data.insert(
            "oauthExtras".into(),
            json!({"scopes": ["user:inference"], "subscriptionType": "pro"}),
        );
        provider_data.insert("trustedDeviceToken".into(), json!("belongs-to-new-account"));
        let account = account_with(
            Credential {
                access_token: "new-access".into(),
                refresh_token: "new-refresh".into(),
                id_token: None,
                expires_at: Timestamp::from_millisecond(1_800_000_000_000).ok(),
                refresh_expires_at: None,
            },
            provider_data,
        );

        let merged = merge_credentials(&live, &account);
        let oauth = merged[OAUTH_KEY].as_object().unwrap();
        assert_eq!(oauth["accessToken"], "new-access");
        assert_eq!(oauth["refreshToken"], "new-refresh");
        assert_eq!(oauth["expiresAt"], json!(1_800_000_000_000i64));
        assert_eq!(oauth["subscriptionType"], "pro");
        // The outgoing account's refresh expiry must not linger.
        assert!(!oauth.contains_key("refreshTokenExpiresAt"));
        // Machine-scoped secrets stay; account-scoped ones follow the account.
        assert_eq!(merged["mcpOAuth"], live["mcpOAuth"]);
        assert_eq!(merged["pluginSecrets"], live["pluginSecrets"]);
        assert_eq!(merged["trustedDeviceToken"], "belongs-to-new-account");
    }

    #[test]
    fn merge_drops_a_conflicting_api_key() {
        let mut live = credentials_json().as_object().unwrap().clone();
        live.insert("primaryApiKey".into(), json!("sk-ant-api03-xyz"));
        let account = account_with(
            Credential {
                access_token: "a".into(),
                refresh_token: "r".into(),
                id_token: None,
                expires_at: None,
                refresh_expires_at: None,
            },
            Map::new(),
        );
        assert!(!merge_credentials(&live, &account).contains_key("primaryApiKey"));
    }

    #[test]
    fn parses_the_limits_array_including_per_model_windows() {
        let response = json!({
            "five_hour": {"utilization": 94.0, "resets_at": "2026-09-15T10:20:00.631008-04:00"},
            "seven_day": {"utilization": 90.0, "resets_at": "2026-09-20T07:00:00.631034-04:00"},
            "limits": [
                {"kind": "session", "group": "session", "percent": 94,
                 "resets_at": "2026-09-15T10:20:00.631008-04:00", "scope": null},
                {"kind": "weekly_all", "group": "weekly", "percent": 90,
                 "resets_at": "2026-09-20T07:00:00.631034-04:00", "scope": null},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 12,
                 "resets_at": "2026-09-20T06:59:59.631251-04:00",
                 "scope": {"model": {"id": null, "display_name": "Opus"}}}
            ]
        });
        let now = Timestamp::from_second(1_789_000_000).unwrap();
        let usage = parse_usage(&response, now);
        assert_eq!(usage.windows.len(), 3);
        assert_eq!(usage.windows[0].label(), "5h");
        assert_eq!(usage.windows[1].label(), "weekly");
        assert_eq!(usage.windows[2].label(), "weekly Opus");
        assert_eq!(usage.used_at(now), 94.0);
        assert!(usage.windows[0].resets_at.is_some());
    }

    #[test]
    fn falls_back_to_the_named_windows() {
        let response = json!({
            "five_hour": {"utilization": 35.0, "resets_at": "2026-09-13T21:30:00.228982+00:00"},
            "seven_day": {"utilization": 7.0, "resets_at": "2026-09-20T11:00:00.229003+00:00"},
            "limits": []
        });
        let usage = parse_usage(&response, Timestamp::from_second(1).unwrap());
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].window_secs, FIVE_HOURS);
        assert_eq!(usage.windows[1].window_secs, ONE_WEEK);
    }

    #[test]
    fn parses_the_profile_response() {
        let identity = parse_profile(&json!({
            "account": {"uuid": "acct-1", "email": "dev@example.com", "has_claude_max": true},
            "organization": {"uuid": "org-1", "name": "Example Inc"}
        }));
        assert_eq!(identity.user_id.unwrap(), "acct-1");
        assert_eq!(identity.email.unwrap(), "dev@example.com");
        assert_eq!(identity.workspace_name.unwrap(), "Example Inc");
        assert_eq!(identity.plan.unwrap(), "max");
    }

    #[test]
    fn token_response_keeps_the_old_refresh_token_when_none_is_returned() {
        let previous = Credential {
            access_token: "old".into(),
            refresh_token: "keep-me".into(),
            id_token: None,
            expires_at: None,
            refresh_expires_at: Timestamp::from_second(9_000_000_000).ok(),
        };
        let refreshed =
            parse_token_response(&json!({"access_token": "new", "expires_in": 3600}), &previous).unwrap();
        assert_eq!(refreshed.access_token, "new");
        assert_eq!(refreshed.refresh_token, "keep-me");
        assert_eq!(refreshed.refresh_expires_at, previous.refresh_expires_at);
        assert!(refreshed.expires_at.unwrap() > Timestamp::now());

        let rotated =
            parse_token_response(&json!({"access_token": "n", "refresh_token": "fresh"}), &previous).unwrap();
        assert_eq!(rotated.refresh_token, "fresh");
    }

    #[test]
    fn capture_then_install_round_trips_through_a_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(CREDENTIALS_FILE),
            serde_json::to_vec(&credentials_json()).unwrap(),
        )
        .unwrap();
        std::fs::write(
            home.join(IDENTITY_FILE),
            serde_json::to_vec(&json!({
                "numStartups": 17,
                "oauthAccount": {
                    "accountUuid": "acct-1",
                    "emailAddress": "dev@example.com",
                    "organizationUuid": "org-1",
                    "organizationName": "Example Inc"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let captured = Claude.capture(&home).unwrap();
        assert_eq!(captured.identity.email.as_deref(), Some("dev@example.com"));
        assert_eq!(captured.identity.plan.as_deref(), Some("max"));
        assert_eq!(captured.credential.refresh_token, "sk-ant-ort01-old");

        // Install a different account over the top of the same home.
        let mut account = account_with(
            Credential {
                access_token: "sk-ant-oat01-new".into(),
                refresh_token: "sk-ant-ort01-new".into(),
                id_token: None,
                expires_at: None,
                refresh_expires_at: None,
            },
            captured.provider_data.clone(),
        );
        account.provider_data.insert(
            ACCOUNT_KEY.into(),
            json!({"accountUuid": "acct-2", "emailAddress": "other@example.com"}),
        );
        Claude.install(&home, &account).unwrap();

        let after = Claude.capture(&home).unwrap();
        assert_eq!(after.credential.access_token, "sk-ant-oat01-new");
        assert_eq!(after.identity.email.as_deref(), Some("other@example.com"));

        // Unrelated Claude Code settings survive the swap.
        let settings: Value =
            serde_json::from_slice(&std::fs::read(home.join(IDENTITY_FILE)).unwrap()).unwrap();
        assert_eq!(settings["numStartups"], 17);
        // ... and so do the machine-scoped secrets.
        let creds: Value =
            serde_json::from_slice(&std::fs::read(home.join(CREDENTIALS_FILE)).unwrap()).unwrap();
        assert_eq!(creds["mcpOAuth"]["server"], "machine-scoped");
    }

    #[test]
    fn keychain_service_is_stable_and_per_home() {
        let a = keychain_service(Path::new("/tmp/profile-a")).unwrap();
        let b = keychain_service(Path::new("/tmp/profile-b")).unwrap();
        assert_ne!(a, b);
        assert_eq!(a, keychain_service(Path::new("/tmp/profile-a")).unwrap());
        assert!(a.starts_with(&format!("{KEYCHAIN_SERVICE}-")));
        assert_eq!(a.len(), KEYCHAIN_SERVICE.len() + 1 + 8);
    }
}
