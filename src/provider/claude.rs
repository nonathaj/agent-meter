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
use super::{Provider, Spent, cli_command, config_home_from_env, sanitize_text};
use crate::account::{Account, Captured, Credential, Identity, ProviderKind};
use crate::fsutil::{self, Mode};
use crate::http;
use crate::lock::DirLock;
use crate::usage::{FIVE_HOURS, ONE_WEEK, ResetGrant, Resets, Usage, Window, WindowKind};

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

/// The scopes Claude Code's own login grants, as it records them.
///
/// Claude Code decides whether it is signed in by reading `scopes` out of the
/// credential file, not by asking the provider: without `user:inference` there
/// it reports "Not logged in" and never sends the token at all. Other tools'
/// stores keep only the tokens, so an account imported from one has no scopes
/// of its own to write back, and gets these. They are what `claude auth login`
/// asked for, so they are what the token was issued with.
const LOGIN_SCOPES: &[&str] = &[
    "user:file_upload",
    "user:inference",
    "user:mcp_servers",
    "user:plugins",
    "user:profile",
    "user:sessions:claude_code",
];

/// Base name of the macOS Keychain item Claude Code stores its credential in.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// The usage endpoint, asked to include the limit resets the account holds
/// (`cedar_ember`) and to leave out the spending block agent-meter never reads.
/// These are the parameters Claude Code's own `/limit-reset` sends.
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
/// The User-Agent sent with the usage request.
///
/// Anthropic describes an account's limit resets only to a client it
/// recognises: under agent-meter's own name the block comes back with
/// `ineligible_reason: "surface"` and no grants, and under Claude Code's it
/// lists them — measured on one machine, 2026-09-29. The version is the one
/// that was measured. The server can also refuse with `cli_version`, which
/// would mean this needs raising to a newer Claude Code release.
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.284 (external, cli)";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// Where an organization's limit resets are spent, as Claude Code's own
/// `/limit-reset` spends them.
const ORGANIZATIONS_URL: &str = "https://api.anthropic.com/api/organizations";
/// The programme limit resets are granted under, as Claude Code names it.
const RESET_PROGRAM: &str = "cedar_ember";
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
        let identity_file = read_identity_file(home)?;
        let account = identity_file
            .as_ref()
            .and_then(|v| v.get(ACCOUNT_KEY))
            .and_then(Value::as_object);
        captured_from_parts(&blob, account)
            .with_context(|| format!("reading the Claude Code credential in {}", home.display()))
    }

    fn signed_out(&self, home: &Path) -> Result<bool> {
        // When the provider rejects its refresh token, Claude Code drops the
        // OAuth block and keeps everything else, `oauthAccount` included — so
        // the identity file goes on naming an account it holds no login for.
        let holds_token = |oauth: &Value| {
            ["accessToken", "refreshToken"].iter().any(|key| {
                oauth
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
            })
        };
        if read_credentials(home)?
            .as_ref()
            .and_then(|blob| blob.get(OAUTH_KEY))
            .is_some_and(holds_token)
        {
            return Ok(false);
        }
        // A Console login keeps an API key beside the identity instead.
        let api_key = read_identity_file(home)?
            .as_ref()
            .and_then(|root| root.get("primaryApiKey"))
            .and_then(Value::as_str)
            .is_some_and(|key| !key.is_empty());
        Ok(!api_key)
    }

    fn credential_files(&self, home: &Path) -> Vec<PathBuf> {
        vec![credentials_path(home)]
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
                ("user-agent", CLAUDE_CODE_USER_AGENT),
            ],
        )?;
        Ok(parse_usage(&response, Timestamp::now()))
    }

    fn spend_reset(
        &self,
        account: &Account,
        credential: &Credential,
        grant: &str,
        request: &str,
    ) -> http::Result<Spent> {
        let refuse = |why: String| Err(http::Error::Transport(anyhow!(why)));
        // The organisation goes into the path, and the ids are checked as
        // Claude Code checks them before it sends either.
        let Some(organization) = account
            .identity
            .workspace_id
            .as_deref()
            .filter(|org| !org.is_empty() && org.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        else {
            return refuse(format!(
                "agent-meter does not know which organization {} is in; read its usage first",
                account.id
            ));
        };
        if !is_id(grant, 40, false) || !is_id(request, 64, true) {
            return refuse(format!("{grant:?} is not a limit reset Anthropic would know"));
        }
        let response: Value = http::post_json_with(
            &format!("{ORGANIZATIONS_URL}/{organization}/reset_rate_limits"),
            &[
                ("authorization", &http::bearer(&credential.access_token)),
                ("anthropic-beta", OAUTH_BETA),
                ("user-agent", CLAUDE_CODE_USER_AGENT),
            ],
            json!({"program": RESET_PROGRAM, "grant_id": grant, "request_id": request}),
        )?;
        Ok(parse_claim(&response))
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

    fn poll_interval(&self) -> Duration {
        // Anthropic tolerates roughly 28-30 requests an hour per account, and
        // the penalty for passing it is not one skipped reading: the endpoint
        // stays saturated for about an hour, which is an hour with nothing to
        // switch on. Five minutes is twelve an hour, leaving room for the token
        // refreshes and profile reads that come out of the same budget.
        super::CAUTIOUS_POLL_INTERVAL
    }
}

/// Builds an account from the two things Claude Code keeps about it: the
/// credential blob, and the `oauthAccount` block naming whose it is.
///
/// Shared with the importers, which read those two pieces out of other tools'
/// backups rather than out of a live Claude Code home — so there is one
/// definition of the shape rather than one per source.
pub(crate) fn captured_from_parts(
    blob: &Map<String, Value>,
    account: Option<&Map<String, Value>>,
) -> Result<Captured> {
    let oauth = blob.get(OAUTH_KEY).and_then(Value::as_object).ok_or_else(|| {
        anyhow!(
            "it has no {OAUTH_KEY} block; it may be an API-key login, \
             which agent-meter cannot meter"
        )
    })?;
    let credential = parse_credential(oauth)?;
    // Deliberately no plan. The blob carries `subscriptionType` and
    // `rateLimitTier`, but both drift from what the account is actually on —
    // measured on one machine, 2026-09-15: `max` and `default_claude_max_20x`
    // for an account the provider reported as a Team seat on
    // `default_claude_max_5x`. The profile endpoint answers this, and `Engine`
    // asks it.
    let identity = parse_identity(account);

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
    for (key, value) in blob {
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
pub(crate) fn merge_credentials(live: &Map<String, Value>, account: &Account) -> Map<String, Value> {
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
    // A credential without scopes is one Claude Code will not use. See
    // `LOGIN_SCOPES`.
    let has_scopes = oauth
        .get("scopes")
        .and_then(Value::as_array)
        .is_some_and(|scopes| !scopes.is_empty());
    if !has_scopes {
        oauth.insert("scopes".into(), json!(LOGIN_SCOPES));
    }
    // Claude Code chooses its default model and labels the session by this.
    // The provider's answer is the one to give it when the account arrived
    // without Claude Code's own copy.
    if !oauth.contains_key("subscriptionType")
        && let Some(plan) = &account.identity.plan
    {
        oauth.insert("subscriptionType".into(), plan.clone().into());
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
        workspace_name: get("organizationName").as_deref().and_then(sanitize_text),
        // The plan and the size of the quota come from the provider, not from
        // this file; see `capture`.
        plan: None,
        capacity: None,
    }
}

/// Reads `/api/oauth/profile`.
///
/// This endpoint is the authority on which plan an account is on and how large
/// its quota is; the copies of both in the local credential file drift away
/// from it. Measured on one machine, 2026-09-15: the file said `max` and
/// `default_claude_max_20x` for the account this endpoint reported as
/// `claude_team` on `default_claude_max_5x`.
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
    let email = string(account, "email").or_else(|| string(account, "email_address"));
    let capacity = multiplier(organization.and_then(|o| o.get("rate_limit_tier")));
    Identity {
        user_id: string(account, "uuid"),
        workspace_id: string(organization, "uuid"),
        workspace_name: organization
            .and_then(|o| o.get("name"))
            .and_then(Value::as_str)
            .and_then(sanitize_text),
        plan: parse_plan(account, organization, capacity),
        capacity,
        email,
    }
}

/// The plans Claude sells, in our own words.
const PLANS: [&str; 6] = ["free", "pro", "max", "ultra", "team", "enterprise"];

/// Which plan an account is on.
///
/// The provider's own name for it, `plan_display_name`, decides it when it is
/// one we know: it is the name Claude shows the person, and it followed the
/// rename of 2026-10-30 that `organization_type` did not. Measured on one
/// machine, 2026-11-17: an Ultra account reported `"Ultra"` there, and
/// `claude_max` on `default_claude_max_20x` beside it.
///
/// Without that, `organization_type` decides it, because the booleans cannot:
/// a Team seat sets `has_claude_max` exactly as a personal Max one does, so
/// reading the booleans alone reports a Team account as "max" — a plan the
/// person is not on. Measured on one machine, 2026-09-15, where the two seats
/// differed in nothing else the response carries.
///
/// Known names and types are matched to our own words rather than echoed, and
/// one we have not seen falls through to the next instead of being printed.
/// That keeps the property the booleans had: nothing a provider writes reaches
/// the screen through this field.
fn parse_plan(
    account: Option<&Value>,
    organization: Option<&Value>,
    capacity: Option<u32>,
) -> Option<String> {
    let flag = |key: &str| {
        account
            .and_then(|a| a.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let named = organization
        .and_then(|o| o.get("plan_display_name"))
        .and_then(Value::as_str)
        .and_then(|name| {
            PLANS
                .into_iter()
                .find(|plan| name.trim().eq_ignore_ascii_case(plan))
        });
    if let Some(plan) = named {
        return Some(plan.into());
    }
    let plan = match organization
        .and_then(|o| o.get("organization_type"))
        .and_then(Value::as_str)
    {
        Some("claude_team") => "team",
        Some("claude_enterprise") => "enterprise",
        Some("claude_max") => personal_plan(capacity),
        Some("claude_pro") => "pro",
        // The wider plan wins: naming the narrower one would understate what
        // the account can do.
        _ if flag("has_claude_max") => personal_plan(capacity),
        _ if flag("has_claude_pro") => "pro",
        _ => return None,
    };
    Some(plan.into())
}

/// What a personal plan above Pro is called, from the size of its quota.
///
/// On 2026-10-30 Claude renamed Max 5x to Max and Max 20x to Ultra, and kept
/// both on the organization type and rate-limit tiers they had before. So the
/// tier is what tells them apart where the provider's name for the plan is
/// missing.
fn personal_plan(capacity: Option<u32>) -> &'static str {
    if capacity == Some(20) { "ultra" } else { "max" }
}

/// Gives a stored account the name its plan has now.
///
/// Records written before the rename say `max` for an Ultra account, and only
/// the account in use asks the provider again, so an idle one would show the
/// retired name for good. The rename was one-to-one, so no request is needed:
/// a `max` on the 20x tier is Ultra by definition.
pub fn rename_retired_plan(identity: &mut Identity) {
    if identity.plan.as_deref() == Some("max") {
        identity.plan = Some(personal_plan(identity.capacity).into());
    }
}

/// The quota multiplier in a `rate_limit_tier`: `default_claude_max_20x` is 20.
///
/// Rebuilt from digits that parsed as a number, never sliced out of the
/// provider's string, so a hostile `rate_limit_tier` has no route onto the
/// screen. Anything not ending in `<digits>x` yields `None` rather than a guess.
fn multiplier(tier: Option<&Value>) -> Option<u32> {
    let digits = tier?.as_str()?.rsplit('_').next()?.strip_suffix('x')?;
    // A tier is a small multiple. The bound keeps a long digit run out of the
    // parse, and zero out of a ranking that multiplies by it.
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    digits.parse().ok().filter(|times| *times > 0)
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
        resets: parse_resets(response.get("cedar_ember")),
        ignored: Vec::new(),
    }
}

/// Reads the `cedar_ember` block: the limit resets the account holds.
///
/// `eligible: false` means two different things, and only the reason tells
/// them apart. Either the account itself holds nothing — its plan or seat does
/// not qualify, or nothing was granted — which is a definite none; or the
/// server would not say to this client, which is an unknown and must not be
/// shown as none.
fn parse_resets(block: Option<&Value>) -> Option<Resets> {
    let block = block.filter(|b| b.is_object())?;
    if block.get("eligible").and_then(Value::as_bool) != Some(true) {
        return match block.get("ineligible_reason").and_then(Value::as_str) {
            Some("tier" | "seat" | "tenure" | "no_grant") => Some(Resets::none()),
            _ => None,
        };
    }
    let grants = block
        .get("grants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_grant)
        .collect();
    Some(Resets::from_grants(grants))
}

/// One grant, or `None` for one with nothing left to spend right now.
fn parse_grant(grant: &Value) -> Option<ResetGrant> {
    // A paused grant cannot be spent until it is resumed, so it is not a
    // reset the account can reach for.
    if grant.get("paused").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let left = grant
        .get("resets_left")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)?;
    let mut refills = Vec::new();
    for window in grant
        .get("clears")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        // Only the windows agent-meter shows are named; the rest, such as the
        // overage allowance, have nothing on screen to point at.
        let kind = match window.as_str() {
            Some("five_hour") => WindowKind::FiveHour,
            Some("seven_day") => WindowKind::Weekly,
            _ => continue,
        };
        if !refills.contains(&kind) {
            refills.push(kind);
        }
    }
    Some(ResetGrant {
        id: grant
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| is_id(id, 40, false))
            .map(ToString::to_string),
        title: grant.get("label").and_then(Value::as_str).and_then(sanitize_text),
        left,
        expires_at: grant.get("ends_at").and_then(parse_rfc3339),
        refills,
        usable_now: grant.get("usable_now").and_then(Value::as_bool),
        only_at_limit: grant.get("use_requires_limit").and_then(Value::as_bool) == Some(true),
    })
}

/// Whether `id` is one Claude Code would send: at most `max` letters, digits,
/// `-` and `_`, the letters lowercase unless `any_case`.
fn is_id(id: &str, max: usize, any_case: bool) -> bool {
    (1..=max).contains(&id.len())
        && id.chars().all(|c| {
            c.is_ascii_digit()
                || c == '-'
                || c == '_'
                || c.is_ascii_lowercase()
                || (any_case && c.is_ascii_uppercase())
        })
}

/// Reads the answer to spending a reset.
///
/// Every answer but two settles the request. `unavailable` and `error` say
/// the server could not finish it, and Claude Code asks again with the same
/// request id for those, which is what [`Spent::Unconfirmed`] has the caller
/// do.
fn parse_claim(response: &Value) -> Spent {
    let reason = response
        .get("reason")
        .and_then(Value::as_str)
        .and_then(sanitize_text)
        .map(|reason| reason.replace('_', " "));
    match response.get("result").and_then(Value::as_str) {
        Some("reset") => Spent::Reset {
            left: response
                .get("resets_left")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok()),
        },
        Some("not_limited") => Spent::NotNeeded,
        Some("already_used") => Spent::AlreadyUsed,
        Some("cooldown") => Spent::Refused(
            match response
                .get("cooldown_until")
                .and_then(parse_rfc3339)
                .filter(|at| *at > Timestamp::now())
            {
                Some(at) => format!(
                    "another reset can be used in {}",
                    crate::timefmt::until(Timestamp::now(), at)
                ),
                None => "another reset cannot be used yet".into(),
            },
        ),
        Some("ineligible") => Spent::Refused(match reason {
            Some(reason) => format!("Anthropic says this account cannot use it ({reason})"),
            None => "Anthropic says this account cannot use it".into(),
        }),
        _ => Spent::Unconfirmed("Anthropic could not confirm the reset".into()),
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
pub(crate) fn parse_token_response(response: &Value, previous: &Credential) -> Result<Credential> {
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
    // The refresh token is single-use, so the one just replaced has no expiry
    // to carry forward; only an unrotated one keeps its own.
    let rotated = refresh_token != previous.refresh_token;
    Ok(Credential {
        access_token,
        refresh_token,
        id_token: None,
        // Never the previous expiry. That timestamp describes the token this
        // one just replaced, and it is the field that decides when to renew:
        // a renewed credential wearing a spent clock is due again immediately,
        // renews again on the next poll, and burns another single-use refresh
        // token every time — until two rotations collide and the provider
        // answers `invalid_grant`, which cannot be undone without signing in.
        //
        // `None` where the provider stated no lifetime is the honest answer,
        // and it is also what closes the loop: an unknown expiry reads as not
        // due, so the next renewal waits for a real reason.
        expires_at,
        refresh_expires_at: (!rotated).then_some(previous.refresh_expires_at).flatten(),
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
            entitlement_checked_at: None,
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

    /// Claude Code reads `scopes` to decide whether it is signed in at all, and
    /// an account imported from another tool's store has none. Installed as it
    /// came, it leaves every session saying "Not logged in" while the same
    /// token reads usage perfectly well.
    #[test]
    fn an_account_imported_without_scopes_is_installed_with_the_login_ones() {
        let live = credentials_json().as_object().unwrap().clone();
        let mut provider_data = Map::new();
        provider_data.insert("oauthAccount".into(), json!({"emailAddress": "dev@example.com"}));
        let mut account = account_with(
            Credential {
                access_token: "a".into(),
                refresh_token: "r".into(),
                id_token: None,
                expires_at: None,
                refresh_expires_at: None,
            },
            provider_data,
        );
        account.identity.plan = Some("team".into());

        let merged = merge_credentials(&live, &account);
        let oauth = merged[OAUTH_KEY].as_object().unwrap();
        assert!(
            oauth["scopes"]
                .as_array()
                .unwrap()
                .contains(&json!("user:inference")),
            "without user:inference Claude Code will not use the token: {oauth:?}"
        );
        // The provider's word on the plan, since there is no copy of Claude
        // Code's own to keep.
        assert_eq!(oauth["subscriptionType"], "team");
    }

    /// The scopes and plan Claude Code wrote itself are the ones it keeps.
    #[test]
    fn scopes_claude_code_recorded_are_kept_as_they_are() {
        let live = credentials_json().as_object().unwrap().clone();
        let mut provider_data = Map::new();
        provider_data.insert(
            "oauthExtras".into(),
            json!({"scopes": ["user:inference"], "subscriptionType": "pro"}),
        );
        let mut account = account_with(
            Credential {
                access_token: "a".into(),
                refresh_token: "r".into(),
                id_token: None,
                expires_at: None,
                refresh_expires_at: None,
            },
            provider_data,
        );
        account.identity.plan = Some("max".into());

        let merged = merge_credentials(&live, &account);
        assert_eq!(merged[OAUTH_KEY]["scopes"], json!(["user:inference"]));
        assert_eq!(merged[OAUTH_KEY]["subscriptionType"], "pro");
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

    /// The `cedar_ember` block as Claude Code's own client is sent it —
    /// measured on one machine, 2026-09-29 — with a spent and a paused grant
    /// added beside the live one.
    #[test]
    fn reads_the_limit_resets_an_account_holds() {
        let response = json!({
            "limits": [],
            "cedar_ember": {
                "eligible": true, "ineligible_reason": null, "at_limit": false, "exhausted": [],
                "grants": [
                    {"id": "opus55-launch-promax-20260921",
                     "label": "Claude Opus 5.5 launch: one usage-limit reset for Pro and Max",
                     "resets_total": 1, "resets_left": 1,
                     "starts_at": "2026-09-22T16:00:00+00:00", "ends_at": "2026-10-22T16:00:00+00:00",
                     "clears": ["five_hour", "seven_day", "seven_day_overage_included"],
                     "paused": false, "usable_now": true, "use_requires_limit": false,
                     "percent_used": {"five_hour": 0, "seven_day": 92}, "blocking": [], "arm": null},
                    {"id": "spent", "label": "Spent", "resets_total": 1, "resets_left": 0,
                     "ends_at": "2026-10-22T16:00:00+00:00", "clears": ["five_hour"], "paused": false},
                    {"id": "paused", "label": "Paused", "resets_total": 1, "resets_left": 1,
                     "ends_at": "2026-10-22T16:00:00+00:00", "clears": ["five_hour"], "paused": true}
                ],
                "next_grant_id": "opus55-launch-promax-20260921",
                "weekly_resets_at": "2026-09-30T14:00:00+00:00", "cooldown_until": null
            }
        });
        let resets = parse_usage(&response, Timestamp::from_second(1).unwrap())
            .resets
            .unwrap();
        assert_eq!(resets.available, 1);
        let grants = resets.grants.unwrap();
        assert_eq!(
            grants.len(),
            1,
            "spent and paused grants are not the account's to spend"
        );
        assert_eq!(
            grants[0].title.as_deref(),
            Some("Claude Opus 5.5 launch: one usage-limit reset for Pro and Max")
        );
        assert_eq!(grants[0].left, 1);
        assert_eq!(grants[0].expires_at.unwrap().to_string(), "2026-10-22T16:00:00Z");
        // The overage allowance has nothing on screen to point at.
        assert_eq!(grants[0].refills, [WindowKind::FiveHour, WindowKind::Weekly]);
        // What spending one names, and whether Anthropic would take it now.
        assert_eq!(grants[0].id.as_deref(), Some("opus55-launch-promax-20260921"));
        assert_eq!(grants[0].usable_now, Some(true));
        assert!(!grants[0].only_at_limit);
        assert_eq!(grants[0].hindrance(), None);
    }

    /// A reset that can only be used at a limit says so while the account is
    /// not at one; an id Claude Code would refuse to send is not kept.
    #[test]
    fn a_grant_says_when_anthropic_would_not_take_it() {
        let grant = parse_grant(&json!({
            "id": "weekly-wall", "resets_left": 1, "usable_now": false, "use_requires_limit": true
        }))
        .unwrap();
        assert_eq!(grant.hindrance(), Some("usable only once a limit is reached"));
        let odd = parse_grant(&json!({"id": "Not/An Id", "resets_left": 1})).unwrap();
        assert_eq!(odd.id, None);
        assert_eq!(odd.usable_now, None, "not said is not a no");
    }

    /// Every answer Claude Code knows, as it reads them: two leave the
    /// request open to be asked again, the rest settle it.
    #[test]
    fn reads_the_answer_to_spending_a_reset() {
        let claim = |body: Value| parse_claim(&body);
        assert_eq!(
            claim(json!({"result": "reset", "resets_left": 0, "cleared": ["five_hour"]})),
            Spent::Reset { left: Some(0) }
        );
        assert_eq!(claim(json!({"result": "not_limited"})), Spent::NotNeeded);
        assert_eq!(claim(json!({"result": "already_used"})), Spent::AlreadyUsed);
        assert_eq!(
            claim(json!({"result": "ineligible", "reason": "no_grant"})),
            Spent::Refused("Anthropic says this account cannot use it (no grant)".into())
        );
        let soon = (Timestamp::now() + jiff::SignedDuration::from_mins(90)).to_string();
        let Spent::Refused(cooldown) = claim(json!({"result": "cooldown", "cooldown_until": soon})) else {
            panic!("a cooldown is a refusal");
        };
        assert!(
            cooldown.starts_with("another reset can be used in 1h"),
            "{cooldown}"
        );
        for open in [
            json!({"result": "unavailable"}),
            json!({"result": "error"}),
            json!({}),
        ] {
            let answer = claim(open);
            assert!(!answer.is_settled(), "{answer:?}");
        }
        assert!(Spent::NotNeeded.is_settled());
    }

    #[test]
    fn only_ids_claude_code_would_send_are_sent() {
        assert!(is_id("opus55-launch-promax-20260921", 40, false));
        assert!(!is_id("Upper", 40, false));
        assert!(is_id("Upper_and-9", 64, true));
        assert!(!is_id("", 40, false));
        assert!(!is_id(&"a".repeat(41), 40, false));
        assert!(!is_id("../etc", 64, true));
    }

    /// The server refuses to say to a client it does not recognise, and that
    /// refusal is an unknown — not an account with no resets.
    #[test]
    fn a_refusal_to_say_is_not_an_answer_of_none() {
        let resets = |reason: &str| {
            parse_resets(Some(&json!({
                "eligible": false, "ineligible_reason": reason, "grants": [], "next_grant_id": null
            })))
        };
        for reason in [
            "surface",
            "cli_version",
            "unavailable",
            "config_off",
            "something_new",
        ] {
            assert_eq!(resets(reason), None, "{reason}");
        }
        for reason in ["no_grant", "tier", "seat", "tenure"] {
            assert_eq!(resets(reason), Some(Resets::none()), "{reason}");
        }
        assert_eq!(parse_resets(None), None);
        assert_eq!(parse_resets(Some(&Value::Null)), None);
    }

    /// A provider-written label reaches a terminal row, so it arrives clean.
    #[test]
    fn a_grant_label_cannot_repaint_the_line() {
        let grant = parse_grant(&json!({
            "label": "\u{1b}[31mFree\u{202e} reset", "resets_left": 2, "clears": ["seven_day", "seven_day"]
        }))
        .unwrap();
        assert_eq!(grant.title.as_deref(), Some("[31mFree reset"));
        assert_eq!(grant.refills, [WindowKind::Weekly]);
        assert_eq!(grant.expires_at, None);
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

    /// A Team seat sets `has_claude_max` exactly as a personal Max one does, so
    /// the booleans alone report it as "max" — a plan the person is not on.
    /// Both bodies are the shape measured on one machine, 2026-09-15.
    #[test]
    fn a_team_seat_is_not_reported_as_a_personal_max_one() {
        let seat = |org_type: &str, tier: &str| {
            json!({
                "account": {"uuid": "u", "has_claude_max": true, "has_claude_pro": false},
                "organization": {"uuid": "o", "name": "Example", "organization_type": org_type,
                                 "rate_limit_tier": tier}
            })
        };

        let team = parse_profile(&seat("claude_team", "default_claude_max_5x"));
        assert_eq!(team.plan.as_deref(), Some("team"));
        assert_eq!(team.capacity, Some(5));
        assert_eq!(team.plan_label().unwrap(), "team 5x");

        let personal = parse_profile(&seat("claude_max", "default_claude_max_5x"));
        assert_eq!(personal.plan.as_deref(), Some("max"));
        assert_eq!(personal.capacity, Some(5));
        assert_eq!(personal.plan_label().unwrap(), "max");
    }

    /// Max 20x became Ultra on 2026-10-30, and Max 5x became Max, without the
    /// organization type or the tier changing. The provider's own name for the
    /// plan says which; this is the shape measured on one machine, 2026-11-17.
    #[test]
    fn the_twenty_times_plan_is_ultra() {
        let ultra = json!({
            "account": {"uuid": "u", "has_claude_max": true, "has_claude_pro": false},
            "organization": {"uuid": "o", "organization_type": "claude_max",
                             "rate_limit_tier": "default_claude_max_20x",
                             "plan_display_name": "Ultra", "plan_display_label": "Ultra plan"}
        });
        let identity = parse_profile(&ultra);
        assert_eq!(identity.plan.as_deref(), Some("ultra"));
        assert_eq!(identity.capacity, Some(20));
        // The plan is one size, and its name says which.
        assert_eq!(identity.plan_label().unwrap(), "ultra");

        // Without the name, the tier tells the two apart.
        let mut unnamed = ultra.clone();
        unnamed["organization"]
            .as_object_mut()
            .unwrap()
            .remove("plan_display_name");
        assert_eq!(parse_profile(&unnamed).plan.as_deref(), Some("ultra"));
        unnamed["organization"]["organization_type"] = json!(null);
        assert_eq!(parse_profile(&unnamed).plan.as_deref(), Some("ultra"));

        // A name we do not know is never printed; the type decides instead.
        let mut odd = ultra.clone();
        odd["organization"]["plan_display_name"] = json!("\u{1b}[31mUltra");
        assert_eq!(parse_profile(&odd).plan.as_deref(), Some("ultra"));
        odd["organization"]["organization_type"] = json!("claude_team");
        odd["organization"]["plan_display_name"] = json!("Team Premium");
        assert_eq!(parse_profile(&odd).plan.as_deref(), Some("team"));
    }

    /// A record kept from before the rename names its plan as it is called now,
    /// without asking the provider again.
    #[test]
    fn a_stored_max_twenty_times_seat_is_called_ultra() {
        let stored = |plan: &str, capacity: Option<u32>| {
            let mut identity = Identity {
                plan: Some(plan.into()),
                capacity,
                ..Identity::default()
            };
            rename_retired_plan(&mut identity);
            identity.plan.unwrap()
        };
        assert_eq!(stored("max", Some(20)), "ultra");
        assert_eq!(stored("max", Some(5)), "max");
        assert_eq!(stored("max", None), "max");
        assert_eq!(stored("team", Some(20)), "team");
        assert_eq!(stored("ultra", Some(20)), "ultra");
    }

    /// An organization type we have not seen falls back to the booleans rather
    /// than being printed: the words on screen stay ours.
    #[test]
    fn an_unknown_organization_type_is_never_printed() {
        let identity = parse_profile(&json!({
            "account": {"has_claude_max": false, "has_claude_pro": true},
            "organization": {"organization_type": "claude_something_new"}
        }));
        assert_eq!(identity.plan.as_deref(), Some("pro"));

        // Nothing to fall back on either: no plan rather than a guess.
        let identity = parse_profile(&json!({
            "account": {},
            "organization": {"organization_type": "\u{1b}[31mclaude_evil"}
        }));
        assert_eq!(identity.plan, None);
    }

    #[test]
    fn the_quota_multiplier_is_rebuilt_from_digits_or_dropped() {
        let tier = |value: Value| multiplier(Some(&value));
        assert_eq!(tier(json!("default_claude_max_20x")), Some(20));
        assert_eq!(tier(json!("default_claude_max_5x")), Some(5));

        // Anything that is not a plain <digits>x yields nothing, so a provider
        // string has no route onto the row through this field.
        for junk in [
            "default_claude_max",
            "",
            "x",
            "enterprise_unlimited",
            "9999x",
            "0x",
            "-5x",
            "5X",
            "1\u{202e}x",
        ] {
            assert_eq!(tier(json!(junk)), None, "junk tier: {junk:?}");
        }
        assert_eq!(multiplier(None), None);
        assert_eq!(tier(json!(20)), None);
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
        assert_eq!(captured.credential.refresh_token, "sk-ant-ort01-old");
        // The file says `subscriptionType: max`, and it is not believed: that
        // field drifts from the account's real plan, so the plan is left for
        // the provider to state. See `parse_profile`.
        assert_eq!(captured.identity.plan, None);
        assert_eq!(captured.identity.capacity, None);

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

    /// What Claude Code leaves when the provider rejects its refresh token is
    /// a signed-out home that still names the account, and a switch may write
    /// into it. Any login it does hold must never be taken for one.
    #[test]
    fn a_home_without_a_login_is_signed_out_whatever_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("profile");
        std::fs::create_dir_all(&home).unwrap();
        let write = |file: &str, value: Value| {
            std::fs::write(home.join(file), serde_json::to_vec(&value).unwrap()).unwrap();
        };
        let signed_out = || Claude.signed_out(&home).unwrap();

        // Never set up.
        assert!(signed_out());

        write(CREDENTIALS_FILE, credentials_json());
        write(
            IDENTITY_FILE,
            json!({"oauthAccount": {"accountUuid": "acct-1", "emailAddress": "dev@example.com"}}),
        );
        assert!(!signed_out());

        // The login dropped, the rest kept.
        write(
            CREDENTIALS_FILE,
            json!({"mcpOAuth": {"server": "machine-scoped"}}),
        );
        assert!(signed_out());
        write(
            CREDENTIALS_FILE,
            json!({"claudeAiOauth": {"accessToken": "", "refreshToken": null}}),
        );
        assert!(signed_out());
        write(
            CREDENTIALS_FILE,
            json!({"claudeAiOauth": {"accessToken": "sk-ant-oat01-only"}}),
        );
        assert!(!signed_out(), "a token on its own still works until it expires");

        // A Console login keeps an API key instead, which is no subscription
        // but is somebody's login all the same.
        write(CREDENTIALS_FILE, json!({}));
        write(IDENTITY_FILE, json!({"primaryApiKey": "sk-ant-api03-key"}));
        assert!(!signed_out());
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
