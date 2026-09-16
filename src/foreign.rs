//! Importing accounts from other tools that manage the same credentials.
//!
//! These read another tool's store and hand back accounts in agent-meter's own
//! shape. Nothing is written back: the other tool's files are opened read-only,
//! so importing cannot disturb a setup somebody is still using.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::account::{Captured, Credential, Identity, ProviderKind};
use crate::fsutil;

/// Where an account can be imported from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    /// The agent CLIs on this machine, as they are signed in right now.
    Live,
    /// claude-swap's backup directory.
    Cswap,
    /// The account store the Gem project launcher and `gemctl` share.
    Gemctl,
}

impl Source {
    pub fn display_name(self) -> &'static str {
        match self {
            Source::Live => "the signed-in agent CLIs",
            Source::Cswap => "claude-swap",
            Source::Gemctl => "gemctl",
        }
    }
}

/// One account found in another tool's store.
#[derive(Debug)]
pub struct Found {
    pub provider: ProviderKind,
    pub captured: Captured,
    /// A name the other tool gave it, when that says more than the address.
    pub label: Option<String>,
    /// Where it came from, for the line reporting the import.
    pub origin: String,
}

/// Reads every account another tool holds.
///
/// `dir` overrides where to look; otherwise the tool's usual location is used.
pub fn read(source: Source, dir: Option<&Path>) -> Result<Vec<Found>> {
    match source {
        Source::Live => bail!("the signed-in CLIs are read by `import` itself, not from a store"),
        Source::Cswap => cswap::read(dir),
        Source::Gemctl => gemctl::read(dir),
    }
}

/// Reads a JSON file, naming it if it will not parse.
fn read_json(path: &Path) -> Result<Option<Value>> {
    let Some(bytes) = fsutil::read_optional(path).with_context(|| format!("reading {}", path.display()))?
    else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("parsing {}", path.display()))
}

mod cswap {
    //! claude-swap keeps one credential per numbered slot, base64-encoded (the
    //! `.enc` suffix is historical; there is no encryption), with the accounts
    //! themselves listed in `sequence.json`.

    use super::*;

    /// Reads claude-swap's backups.
    pub(super) fn read(dir: Option<&Path>) -> Result<Vec<Found>> {
        let dir = match dir {
            Some(dir) => dir.to_path_buf(),
            None => default_dir()?,
        };
        let sequence = dir.join("sequence.json");
        let Some(roster) = super::read_json(&sequence)? else {
            bail!(
                "no claude-swap backup found at {}. Pass --dir if it keeps its files elsewhere.",
                dir.display()
            );
        };
        let accounts = roster
            .get("accounts")
            .and_then(Value::as_object)
            .with_context(|| format!("{} lists no accounts", sequence.display()))?;

        let mut found = Vec::new();
        for (slot, entry) in accounts {
            // An API-key slot has no quota to meter, and a disabled one is a
            // slot its owner has already set aside.
            if entry.get("kind").and_then(Value::as_str) == Some("api_key")
                || entry.get("disabled").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            match read_slot(&dir, slot, entry) {
                Ok(Some(account)) => found.push(account),
                Ok(None) => {}
                // One unreadable slot must not cost the others: report it and
                // carry on, since a partial import is still worth having.
                Err(error) => eprintln!("Skipped claude-swap slot {slot}: {error:#}"),
            }
        }
        found.sort_by(|a, b| a.origin.cmp(&b.origin));
        Ok(found)
    }

    /// Where claude-swap keeps its backups.
    ///
    /// It uses the XDG data directory on Linux and a dot-directory on macOS and
    /// Windows; both are checked, since which one holds the files depends on
    /// where it was installed rather than on where it runs.
    fn default_dir() -> Result<PathBuf> {
        let home = crate::paths::home_dir()?;
        let candidates = [
            home.join(".claude-swap-backup"),
            std::env::var_os("XDG_DATA_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local").join("share"))
                .join("claude-swap"),
        ];
        Ok(candidates
            .iter()
            .find(|dir| dir.join("sequence.json").exists())
            .cloned()
            .unwrap_or_else(|| candidates[0].clone()))
    }

    fn read_slot(dir: &Path, slot: &str, entry: &Value) -> Result<Option<Found>> {
        let Some(path) = credential_path(dir, slot)? else {
            return Ok(None);
        };
        let encoded = fsutil::read_optional(&path)
            .with_context(|| format!("reading {}", path.display()))?
            .unwrap_or_default();
        let decoded = STANDARD
            .decode(String::from_utf8_lossy(&encoded).trim())
            .with_context(|| format!("decoding {}", path.display()))?;
        let blob: Map<String, Value> = serde_json::from_slice(&decoded)
            .with_context(|| format!("parsing the credential in {}", path.display()))?;

        // The identity claude-swap saved alongside the credential is preferred,
        // since it is the one Claude Code itself wrote.
        let config = dir.join("configs").join(format!(
            ".claude-config-{slot}-{}.json",
            string(entry, "email").unwrap_or_default()
        ));
        let account = super::read_json(&config)
            .ok()
            .flatten()
            .and_then(|v| v.get("oauthAccount").cloned())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_else(|| identity_block(entry));

        let captured = crate::provider::claude::captured_from_parts(&blob, Some(&account))?;
        Ok(Some(Found {
            provider: ProviderKind::Claude,
            captured,
            label: string(entry, "alias"),
            origin: format!("claude-swap slot {slot}"),
        }))
    }

    /// The credential file for a slot.
    ///
    /// Found by prefix rather than by composing the address into a name: the
    /// address is part of the file name, and one that does not round-trip
    /// through the filesystem would simply not be found. `.prev` holds the
    /// superseded generation and is never what we want.
    fn credential_path(dir: &Path, slot: &str) -> Result<Option<PathBuf>> {
        let credentials = dir.join("credentials");
        let prefix = format!(".creds-{slot}-");
        let Ok(entries) = std::fs::read_dir(&credentials) else {
            return Ok(None);
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) && name.ends_with(".enc") {
                return Ok(Some(entry.path()));
            }
        }
        Ok(None)
    }

    /// The `oauthAccount` block, rebuilt from the roster when the saved config
    /// is missing.
    fn identity_block(entry: &Value) -> Map<String, Value> {
        let mut block = Map::new();
        for (from, to) in [
            ("uuid", "accountUuid"),
            ("email", "emailAddress"),
            ("organizationUuid", "organizationUuid"),
            ("organizationName", "organizationName"),
        ] {
            if let Some(value) = string(entry, from) {
                block.insert(to.into(), value.into());
            }
        }
        block
    }

    fn string(entry: &Value, key: &str) -> Option<String> {
        entry
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
    }
}

mod gemctl {
    //! The Gem project launcher and `gemctl` share one account store: a
    //! directory of JSON records, one per account, covering both providers.

    use super::*;

    /// One of its records. Unknown fields are ignored, so a newer writer does
    /// not stop an import.
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Record {
        agent: String,
        name: String,
        #[serde(default)]
        account_id: Option<String>,
        #[serde(default)]
        user_id: Option<String>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        email: Option<String>,
        #[serde(default)]
        org_id: Option<String>,
        #[serde(default)]
        org_name: Option<String>,
        #[serde(default)]
        tokens: Tokens,
        /// Epoch seconds, unlike Claude Code's own file, which uses millis.
        #[serde(default)]
        expires_at: Option<i64>,
        #[serde(default)]
        refresh_expires_at: Option<i64>,
        #[serde(default)]
        needs_login: Option<String>,
    }

    #[derive(Debug, Default, Deserialize)]
    struct Tokens {
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        id_token: Option<String>,
    }

    /// A label worth keeping, or nothing.
    ///
    /// gemctl's label is usually the address with a note of its own appended —
    /// `dev@example.com (claimed)`, where "claimed" means it had not yet
    /// confirmed the address with the provider. That is its bookkeeping, not a
    /// name for the account, and carrying it over would put it in the column
    /// where a person's own name for the account belongs.
    pub(super) fn useful_label(label: Option<String>, email: Option<&str>, name: &str) -> Option<String> {
        let label = label?;
        let bare = label
            .split_once(" (")
            .map_or(label.as_str(), |(before, _)| before)
            .trim();
        let echoes_something_shown =
            bare.eq_ignore_ascii_case(name) || email.is_some_and(|email| bare.eq_ignore_ascii_case(email));
        (!bare.is_empty() && !echoes_something_shown).then(|| label.clone())
    }

    pub(super) fn read(dir: Option<&Path>) -> Result<Vec<Found>> {
        let dir = match dir {
            Some(dir) => dir.to_path_buf(),
            None => default_dir()?,
        };
        let entries = std::fs::read_dir(&dir).with_context(|| {
            format!(
                "no gemctl account store at {}. Pass --dir if it keeps its files elsewhere.",
                dir.display()
            )
        })?;

        let mut found = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            match read_record(&path) {
                Ok(Some(account)) => found.push(account),
                Ok(None) => {}
                Err(error) => eprintln!("Skipped {}: {error:#}", path.display()),
            }
        }
        found.sort_by(|a, b| a.origin.cmp(&b.origin));
        Ok(found)
    }

    /// Where that store lives.
    fn default_dir() -> Result<PathBuf> {
        if let Some(dir) = std::env::var_os("GEM_AGENT_ACCOUNTS_DIR").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        let base = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .context("could not determine the local data directory")?;
        Ok(base.join("gem-agent-accounts"))
    }

    fn read_record(path: &Path) -> Result<Option<Found>> {
        let Some(value) = super::read_json(path)? else {
            return Ok(None);
        };
        let record: Record =
            serde_json::from_value(value).with_context(|| format!("reading {}", path.display()))?;

        let Some(provider) = ProviderKind::parse(&record.agent) else {
            // A provider agent-meter does not manage; not an error, just not
            // ours to take.
            return Ok(None);
        };
        let (Some(access_token), Some(refresh_token)) =
            (record.tokens.access_token, record.tokens.refresh_token)
        else {
            bail!("it holds no usable token pair");
        };
        if let Some(reason) = &record.needs_login {
            eprintln!("Importing {} even though gemctl marked it: {reason}", record.name);
        }

        let seconds = |value: Option<i64>| value.and_then(|s| Timestamp::from_second(s).ok());
        let credential = Credential {
            access_token,
            refresh_token,
            id_token: record.tokens.id_token,
            expires_at: seconds(record.expires_at),
            refresh_expires_at: seconds(record.refresh_expires_at),
        };

        // The two providers put different things in `accountId`: for Claude it
        // is the account's own uuid, for Codex the ChatGPT workspace the seat
        // belongs to.
        let identity = match provider {
            ProviderKind::Claude => Identity {
                user_id: record.user_id.or_else(|| record.account_id.clone()),
                email: record.email.clone(),
                workspace_id: record.org_id.clone(),
                workspace_name: record.org_name.clone(),
                // Deliberately not imported: the plan and the quota size are
                // the provider's to state, and a copy that has sat in another
                // tool's store is exactly the kind that drifts.
                plan: None,
                capacity: None,
            },
            ProviderKind::Codex => Identity {
                user_id: record.user_id,
                email: record.email.clone(),
                workspace_id: record.account_id.clone(),
                workspace_name: None,
                plan: None,
                capacity: None,
            },
        };

        // Claude Code needs the identity block written back beside the tokens;
        // Codex builds what it needs from the tokens themselves.
        let mut provider_data = Map::new();
        if provider == ProviderKind::Claude {
            let mut block = Map::new();
            for (key, value) in [
                ("accountUuid", record.account_id.as_ref()),
                ("emailAddress", record.email.as_ref()),
                ("organizationUuid", record.org_id.as_ref()),
                ("organizationName", record.org_name.as_ref()),
            ] {
                if let Some(value) = value {
                    block.insert(key.into(), json!(value));
                }
            }
            if !block.is_empty() {
                provider_data.insert("oauthAccount".into(), Value::Object(block));
            }
        }

        let label = useful_label(record.label, record.email.as_deref(), &record.name);

        Ok(Some(Found {
            provider,
            captured: Captured {
                identity,
                credential,
                provider_data,
            },
            label,
            origin: format!("gemctl {}", record.name),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A claude-swap backup holding two seats under one address, which is the
    /// shape that matters: they differ only in the organisation.
    fn cswap_backup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("credentials")).unwrap();
        std::fs::create_dir_all(root.join("configs")).unwrap();

        std::fs::write(
            root.join("sequence.json"),
            serde_json::to_vec(&json!({
                "activeAccountNumber": 2,
                "sequence": [1, 2, 3],
                "accounts": {
                    "1": {"email": "dev@example.com", "uuid": "person",
                          "organizationUuid": "personal-org",
                          "organizationName": "dev@example.com's Organization"},
                    "2": {"email": "dev@example.com", "uuid": "person",
                          "organizationUuid": "team-org", "organizationName": "Example Inc",
                          "alias": "work"},
                    "3": {"email": "key@example.com", "kind": "api_key"}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        for (slot, refresh) in [("1", "personal"), ("2", "team")] {
            let blob = json!({
                "claudeAiOauth": {
                    "accessToken": format!("sk-ant-oat01-{refresh}"),
                    "refreshToken": format!("sk-ant-ort01-{refresh}"),
                    "expiresAt": 1_900_000_000_000i64,
                    "subscriptionType": "max"
                }
            });
            std::fs::write(
                root.join("credentials")
                    .join(format!(".creds-{slot}-dev@example.com.enc")),
                STANDARD.encode(serde_json::to_vec(&blob).unwrap()),
            )
            .unwrap();
            // The superseded generation must never be the one picked up.
            std::fs::write(
                root.join("credentials")
                    .join(format!(".creds-{slot}-dev@example.com.enc.prev")),
                STANDARD.encode(b"{\"claudeAiOauth\":{\"accessToken\":\"stale\"}}"),
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn reads_claude_swap_slots_including_two_seats_under_one_address() {
        let backup = cswap_backup();
        let found = read(Source::Cswap, Some(backup.path())).unwrap();

        assert_eq!(found.len(), 2, "the API-key slot is not metered: {found:#?}");
        assert_eq!(found[0].origin, "claude-swap slot 1");
        assert_eq!(found[1].origin, "claude-swap slot 2");

        // Same address, different organisations: two accounts, not one.
        assert_eq!(
            found[0].captured.identity.email.as_deref(),
            Some("dev@example.com")
        );
        assert_eq!(
            found[1].captured.identity.email.as_deref(),
            Some("dev@example.com")
        );
        assert_eq!(
            found[0].captured.identity.workspace_id.as_deref(),
            Some("personal-org")
        );
        assert_eq!(
            found[1].captured.identity.workspace_id.as_deref(),
            Some("team-org")
        );

        assert_eq!(
            found[0].captured.credential.refresh_token,
            "sk-ant-ort01-personal"
        );
        assert_eq!(found[1].captured.credential.refresh_token, "sk-ant-ort01-team");
        assert_eq!(found[1].label.as_deref(), Some("work"));
        // Expiries are milliseconds in this blob, as in Claude Code's own file.
        assert_eq!(
            found[0].captured.credential.expires_at.unwrap().as_millisecond(),
            1_900_000_000_000
        );
        // The plan is the provider's to state, not another tool's to pass on.
        assert!(found[0].captured.identity.plan.is_none());
    }

    #[test]
    fn a_missing_claude_swap_backup_says_where_it_looked() {
        let dir = tempfile::tempdir().unwrap();
        let error = read(Source::Cswap, Some(dir.path())).unwrap_err().to_string();
        assert!(error.contains("no claude-swap backup"), "{error}");
        assert!(error.contains("--dir"), "{error}");
    }

    fn gemctl_store() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("claude-1.json"),
            serde_json::to_vec(&json!({
                "schemaVersion": 1, "agent": "claude", "name": "claude-1",
                "accountId": "acct-uuid", "label": "claude-1", "email": "dev@example.com",
                "plan": "max", "verified": true,
                "tokens": {"access_token": "a", "refresh_token": "r"},
                "expiresAt": 1_789_510_555i64, "refreshExpiresAt": 1_791_855_901i64,
                "orgId": "org-1", "orgName": "Example Inc"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("codex-1.json"),
            serde_json::to_vec(&json!({
                "schemaVersion": 1, "agent": "codex", "name": "codex-1",
                "accountId": "chatgpt-workspace", "userId": "chatgpt-user",
                "label": "dev@openai.example", "email": "dev@openai.example",
                "tokens": {"access_token": "a", "refresh_token": "r", "id_token": "i"}
            }))
            .unwrap(),
        )
        .unwrap();
        // A provider agent-meter does not manage, and a record with no tokens.
        std::fs::write(
            dir.path().join("gemini-1.json"),
            br#"{"schemaVersion":1,"agent":"gemini","name":"gemini-1","tokens":{}}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn reads_the_gemctl_store_for_both_providers() {
        let store = gemctl_store();
        let found = read(Source::Gemctl, Some(store.path())).unwrap();

        assert_eq!(found.len(), 2, "an agent we do not manage is skipped: {found:#?}");

        let claude = &found[0];
        assert_eq!(claude.provider, ProviderKind::Claude);
        assert_eq!(claude.captured.identity.user_id.as_deref(), Some("acct-uuid"));
        assert_eq!(claude.captured.identity.workspace_id.as_deref(), Some("org-1"));
        // Seconds there, and the label repeats the record's own name.
        assert_eq!(
            claude.captured.credential.expires_at.unwrap().as_second(),
            1_789_510_555
        );
        assert_eq!(claude.label, None);
        // Claude Code needs this written back beside the tokens.
        assert_eq!(
            claude.captured.provider_data["oauthAccount"]["accountUuid"],
            "acct-uuid"
        );

        let codex = &found[1];
        assert_eq!(codex.provider, ProviderKind::Codex);
        // For Codex this field is the workspace, not the person.
        assert_eq!(
            codex.captured.identity.workspace_id.as_deref(),
            Some("chatgpt-workspace")
        );
        assert_eq!(codex.captured.identity.user_id.as_deref(), Some("chatgpt-user"));
        assert_eq!(codex.captured.credential.id_token.as_deref(), Some("i"));
        // The label only repeats the address, so it is not carried over.
        assert_eq!(codex.label, None);
    }

    /// gemctl labels accounts `<address> (claimed)`, where "claimed" is its own
    /// note about whether it had confirmed the address. That is bookkeeping,
    /// not a name somebody chose, and it must not land in the name column.
    #[test]
    fn gemctl_bookkeeping_does_not_become_an_account_name() {
        let label =
            |label: &str| gemctl::useful_label(Some(label.into()), Some("dev@example.com"), "claude-1");
        assert_eq!(label("dev@example.com (claimed)"), None);
        assert_eq!(label("dev@example.com"), None);
        assert_eq!(label("claude-1"), None);
        assert_eq!(label("DEV@EXAMPLE.COM (claimed)"), None);
        // A name somebody actually chose is kept, parenthetical and all.
        assert_eq!(label("work (main)").as_deref(), Some("work (main)"));
        assert_eq!(label("personal").as_deref(), Some("personal"));
        assert_eq!(gemctl::useful_label(None, None, "claude-1"), None);
    }
}
