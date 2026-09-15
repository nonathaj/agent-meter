//! The account model shared by every layer.

use std::fmt;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Schema version written into every account record.
pub const SCHEMA_VERSION: u32 = 1;

/// An agent CLI whose accounts agent-meter can manage.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Anthropic Claude Code.
    Claude,
    /// OpenAI Codex CLI.
    Codex,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 2] = [ProviderKind::Claude, ProviderKind::Codex];

    /// Stable identifier used in account ids, config keys and JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Claude => "claude",
            ProviderKind::Codex => "codex",
        }
    }

    /// Human-friendly product name.
    pub fn display_name(self) -> &'static str {
        match self {
            ProviderKind::Claude => "Claude Code",
            ProviderKind::Codex => "Codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str().eq_ignore_ascii_case(s))
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who an account belongs to, as far as the provider tells us.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Provider-scoped user id (Claude `accountUuid`, Codex `chatgpt_user_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Billing container the user acts in (Claude organization, ChatGPT workspace).
    /// The same user in two workspaces is two accounts with separate limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    /// Subscription plan, e.g. `max`, `pro`, `plus`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

impl Identity {
    /// Fills fields that are missing here from `other`, and overwrites fields
    /// `other` knows. Used when the provider reports fresher identity details.
    pub fn update_from(&mut self, other: &Identity) {
        fn take(dst: &mut Option<String>, src: &Option<String>) {
            if src.is_some() {
                dst.clone_from(src);
            }
        }
        take(&mut self.user_id, &other.user_id);
        take(&mut self.email, &other.email);
        take(&mut self.workspace_id, &other.workspace_id);
        take(&mut self.workspace_name, &other.workspace_name);
        take(&mut self.plan, &other.plan);
    }
}

/// Whether two identities describe the same account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    Same,
    Different,
    /// Not enough information on one side to tell.
    Unknown,
}

/// Compares two identities. A conflicting user or workspace id always means a
/// different account; otherwise a matching user id or email means the same one.
pub fn same_identity(a: &Identity, b: &Identity) -> Match {
    fn conflict(x: &Option<String>, y: &Option<String>) -> bool {
        matches!((x, y), (Some(x), Some(y)) if x != y)
    }
    if conflict(&a.user_id, &b.user_id) || conflict(&a.workspace_id, &b.workspace_id) {
        return Match::Different;
    }
    if matches!((&a.user_id, &b.user_id), (Some(x), Some(y)) if x == y) {
        return Match::Same;
    }
    match (&a.email, &b.email) {
        (Some(x), Some(y)) if x.eq_ignore_ascii_case(y) => Match::Same,
        (Some(_), Some(_)) => Match::Different,
        _ => Match::Unknown,
    }
}

/// OAuth tokens for an account.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_expires_at: Option<Timestamp>,
}

impl Credential {
    /// A short, non-reversible tag for the refresh token. Refresh tokens rotate
    /// on every use, so equal fingerprints mean "the same generation".
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.refresh_token.as_bytes());
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("fingerprint", &self.fingerprint())
            .field("expires_at", &self.expires_at)
            .field("refresh_expires_at", &self.refresh_expires_at)
            .finish_non_exhaustive()
    }
}

/// A stored account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub schema_version: u32,
    /// Stable id such as `claude-2`. Never reused after removal.
    pub id: String,
    pub provider: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub identity: Identity,
    pub credential: Credential,
    /// Provider-specific data captured alongside the tokens and restored when
    /// the account is installed (e.g. Claude's `oauthAccount` block).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub provider_data: Map<String, Value>,
    pub added_at: Timestamp,
    /// Set when the stored tokens were rejected and the account must log in again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_login: Option<String>,
}

impl Account {
    /// The best short name for display: label, then email, then id.
    pub fn display_name(&self) -> &str {
        self.label
            .as_deref()
            .or(self.identity.email.as_deref())
            .unwrap_or(&self.id)
    }
}

/// Credentials read from an agent CLI's live configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Captured {
    pub identity: Identity,
    pub credential: Credential,
    pub provider_data: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(user: Option<&str>, email: Option<&str>, ws: Option<&str>) -> Identity {
        Identity {
            user_id: user.map(Into::into),
            email: email.map(Into::into),
            workspace_id: ws.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn identity_matching() {
        assert_eq!(
            same_identity(&id(Some("u"), None, None), &id(Some("u"), None, None)),
            Match::Same
        );
        assert_eq!(
            same_identity(&id(Some("u"), None, Some("o1")), &id(Some("u"), None, Some("o2"))),
            Match::Different
        );
        assert_eq!(
            same_identity(
                &id(None, Some("A@x.com"), None),
                &id(Some("u"), Some("a@X.com"), None)
            ),
            Match::Same
        );
        assert_eq!(
            same_identity(&id(None, Some("a@x.com"), None), &id(None, Some("b@x.com"), None)),
            Match::Different
        );
        assert_eq!(
            same_identity(&id(None, None, None), &id(Some("u"), None, None)),
            Match::Unknown
        );
    }

    #[test]
    fn credential_debug_never_prints_tokens() {
        let c = Credential {
            access_token: "secret-access".into(),
            refresh_token: "secret-refresh".into(),
            id_token: None,
            expires_at: None,
            refresh_expires_at: None,
        };
        let text = format!("{c:?}");
        assert!(!text.contains("secret"));
        assert_eq!(c.fingerprint().len(), 12);
    }
}
