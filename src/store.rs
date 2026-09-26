//! Persistence for accounts, cached usage and watcher state.
//!
//! Layout under the data directory:
//! ```text
//! config.toml            user settings
//! accounts/<id>.json     one file per account (contains OAuth tokens)
//! usage.json             cached usage readings
//! state.json             last-switch bookkeeping
//! store.lock             advisory lock held across mutations
//! ```
//! Account files are written with owner-only permissions, but they are not
//! encrypted: the agent CLIs keep the same tokens in plain files next door, so
//! encrypting here would add a key-management problem without raising the bar.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::account::{Account, ProviderKind};
use crate::fsutil::{self, Mode};
use crate::lock::FileLock;
use crate::usage::Usage;

/// How long to wait for another agent-meter process to finish a mutation.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// Guards against a runaway file; real records are a few kilobytes.
const MAX_ACCOUNT_BYTES: u64 = 256 * 1024;

/// Handle on agent-meter's data directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Opens the store at `dir`, creating the directory if needed.
    pub fn open(dir: PathBuf) -> Result<Self> {
        fsutil::create_private_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// Opens the store at the default location.
    pub fn open_default() -> Result<Self> {
        Self::open(crate::paths::data_dir()?)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn accounts_dir(&self) -> PathBuf {
        self.dir.join("accounts")
    }

    fn account_path(&self, id: &str) -> PathBuf {
        self.accounts_dir().join(format!("{id}.json"))
    }

    /// Takes the store-wide lock. Hold it around any read-modify-write cycle so
    /// two agent-meter processes cannot interleave.
    ///
    /// Never hold it across a network call: the other processes on the machine
    /// wait behind it.
    pub fn lock(&self) -> Result<FileLock> {
        FileLock::acquire(&self.dir.join("store.lock"), LOCK_TIMEOUT)
    }

    /// Takes a lock covering one account, for work that must not be done twice
    /// at once but should not stop unrelated accounts from being read.
    pub fn lock_account(&self, id: &str) -> Result<FileLock> {
        validate_id(id)?;
        FileLock::acquire(&self.accounts_dir().join(format!("{id}.lock")), LOCK_TIMEOUT)
    }

    /// Loads every account, sorted by provider then id.
    pub fn accounts(&self) -> Result<Vec<Account>> {
        let dir = self.accounts_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };

        let mut accounts = Vec::new();
        for entry in entries {
            let path = entry
                .with_context(|| format!("reading {}", dir.display()))?
                .path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            accounts.push(self.read_account_file(&path)?);
        }
        accounts.sort_by(|a, b| {
            a.provider
                .cmp(&b.provider)
                .then_with(|| natural_cmp(&a.id, &b.id))
        });
        Ok(accounts)
    }

    /// Loads one account by id.
    pub fn account(&self, id: &str) -> Result<Option<Account>> {
        let path = self.account_path(id);
        if !path.exists() {
            return Ok(None);
        }
        self.read_account_file(&path).map(Some)
    }

    fn read_account_file(&self, path: &Path) -> Result<Account> {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_ACCOUNT_BYTES {
            bail!(
                "{} is {size} bytes, which is too large to be an account record",
                path.display()
            );
        }
        let bytes = fsutil::read_optional(path)
            .with_context(|| format!("reading {}", path.display()))?
            .with_context(|| format!("{} disappeared while being read", path.display()))?;
        let account: Account =
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;

        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        if account.id != stem {
            bail!(
                "{} holds account id {:?}; rename the file or fix the id",
                path.display(),
                account.id
            );
        }
        if account.schema_version > crate::account::SCHEMA_VERSION {
            bail!(
                "{} was written by a newer agent-meter (schema {}); upgrade agent-meter to read it",
                path.display(),
                account.schema_version
            );
        }
        Ok(account)
    }

    /// Writes an account, replacing any record with the same id.
    pub fn put_account(&self, account: &Account) -> Result<()> {
        validate_id(&account.id)?;
        let dir = self.accounts_dir();
        fsutil::create_private_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = self.account_path(&account.id);
        let mut json = serde_json::to_vec_pretty(account).context("serializing the account")?;
        json.push(b'\n');
        fsutil::write_atomic(&path, &json, Mode::Private)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Deletes an account record. Returns whether it existed.
    pub fn remove_account(&self, id: &str) -> Result<bool> {
        validate_id(id)?;
        let path = self.account_path(id);
        let existed = path.exists();
        fsutil::remove_file_if_exists(&path).with_context(|| format!("removing {}", path.display()))?;
        // The per-account lock file outlives the account otherwise. A lock held
        // right now keeps the file open, so treat failure as nothing to do.
        let _ = fsutil::remove_file_if_exists(&self.accounts_dir().join(format!("{id}.lock")));
        if existed {
            let mut usage = self.usage_cache()?;
            if usage.entries.remove(id).is_some() {
                self.put_usage_cache(&usage)?;
            }
        }
        Ok(existed)
    }

    /// Allocates the next free id for `provider`, e.g. `claude-3`.
    ///
    /// Ids are never reused, so an id in a script or shell history cannot come
    /// to mean a different account later.
    pub fn next_id(&self, provider: ProviderKind) -> Result<String> {
        let prefix = format!("{provider}-");
        let on_disk = self
            .accounts()?
            .iter()
            .filter_map(|a| a.id.strip_prefix(&prefix)?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        // The accounts that exist are the floor, not the answer: the highest
        // of them may have been removed, and its number must not come back.
        let mut state = self.state()?;
        let number = on_disk.max(state.issued(provider)) + 1;
        state.record_issued(provider, number);
        self.put_state(&state)?;
        Ok(format!("{prefix}{number}"))
    }

    /// The cached usage readings.
    pub fn usage_cache(&self) -> Result<UsageCache> {
        read_json_or_default(&self.dir.join("usage.json"))
    }

    pub fn put_usage_cache(&self, cache: &UsageCache) -> Result<()> {
        write_json(&self.dir.join("usage.json"), cache)
    }

    /// Watcher bookkeeping.
    pub fn state(&self) -> Result<State> {
        read_json_or_default(&self.dir.join("state.json"))
    }

    pub fn put_state(&self, state: &State) -> Result<()> {
        write_json(&self.dir.join("state.json"), state)
    }

    pub fn config(&self) -> Result<crate::config::Config> {
        crate::config::Config::load(&self.dir)
    }
}

/// Cached usage readings, keyed by account id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageCache {
    pub entries: BTreeMap<String, UsageEntry>,
}

/// What the last poll of one account produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct UsageEntry {
    /// The most recent successful reading, kept even when a later poll failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Message from the most recent failed poll.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When that failure happened, so the interface can say how old it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<Timestamp>,
    /// Do not poll this account again before this instant (rate-limit backoff).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<Timestamp>,
    /// Consecutive failed polls, used to back off progressively.
    pub failures: u32,
}

impl UsageCache {
    pub fn get(&self, id: &str) -> Option<&UsageEntry> {
        self.entries.get(id)
    }

    /// Records a successful reading, clearing any failure state.
    /// Drops readings no stored account claims.
    ///
    /// A poll lets go of the store lock while it is on the network, so an
    /// account removed meanwhile has its reading written back after it is
    /// gone. Left there, it is a stranger's usage — and their address, inside
    /// the reading — waiting under an id for whoever is given it next.
    pub fn retain(&mut self, ids: &[String]) {
        self.entries.retain(|id, _| ids.iter().any(|kept| kept == id));
    }

    pub fn record_success(&mut self, id: &str, usage: Usage) {
        let entry = self.entries.entry(id.to_string()).or_default();
        entry.usage = Some(usage);
        entry.error = None;
        entry.failed_at = None;
        entry.retry_after = None;
        entry.failures = 0;
    }

    /// Forgets that polling this account has been failing, keeping its last
    /// reading. For when the credential those polls failed with is replaced:
    /// the failures say nothing about the new one, and their backoff would
    /// hold it out of the next poll.
    pub fn clear_failure(&mut self, id: &str) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.error = None;
            entry.failed_at = None;
            entry.retry_after = None;
            entry.failures = 0;
        }
    }

    /// Records a failed poll. The previous reading is kept so the UI can still
    /// show something, marked stale by its own timestamp.
    pub fn record_failure(&mut self, id: &str, error: String, retry_after: Option<Timestamp>) {
        let entry = self.entries.entry(id.to_string()).or_default();
        entry.error = Some(error);
        entry.failed_at = Some(Timestamp::now());
        entry.retry_after = retry_after;
        entry.failures = entry.failures.saturating_add(1);
    }
}

/// Watcher bookkeeping that must survive restarts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Last automatic switch per provider, keyed by provider name.
    pub last_switch: BTreeMap<String, SwitchRecord>,
    /// Highest account number ever issued per provider, keyed by provider name.
    ///
    /// Counting from the accounts that exist would hand the number back the
    /// moment the highest one is removed, and then an id somebody wrote down
    /// means a different account. Absent — a store written before this was
    /// kept — reads as zero, and the accounts on disk carry the count.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub issued: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchRecord {
    pub at: Timestamp,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

impl State {
    pub fn last_switch(&self, provider: ProviderKind) -> Option<&SwitchRecord> {
        self.last_switch.get(provider.as_str())
    }

    pub fn record_switch(&mut self, provider: ProviderKind, record: SwitchRecord) {
        self.last_switch.insert(provider.as_str().to_string(), record);
    }

    fn issued(&self, provider: ProviderKind) -> u32 {
        self.issued.get(provider.as_str()).copied().unwrap_or(0)
    }

    fn record_issued(&mut self, provider: ProviderKind, number: u32) {
        self.issued.insert(provider.as_str().to_string(), number);
    }
}

fn read_json_or_default<T: Default + serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let Some(bytes) = fsutil::read_optional(path).with_context(|| format!("reading {}", path.display()))?
    else {
        return Ok(T::default());
    };
    // A cache or state file is derived data: if it is corrupt, rebuilding it is
    // better than refusing to run.
    Ok(serde_json::from_slice(&bytes).unwrap_or_default())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut json = serde_json::to_vec_pretty(value).context("serializing")?;
    json.push(b'\n');
    fsutil::write_atomic(path, &json, Mode::Private).with_context(|| format!("writing {}", path.display()))
}

/// Account ids become file names, so they must not escape the accounts dir.
fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !id.starts_with('-');
    if !ok {
        bail!("invalid account id {id:?}: use letters, digits, '-' and '_'");
    }
    Ok(())
}

/// Orders ids so `claude-9` sorts before `claude-10`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn split(s: &str) -> (&str, u64) {
        match s.rsplit_once('-') {
            Some((head, tail)) => match tail.parse() {
                Ok(n) => (head, n),
                Err(_) => (s, 0),
            },
            None => (s, 0),
        }
    }
    let (a_head, a_num) = split(a);
    let (b_head, b_num) = split(b);
    a_head.cmp(b_head).then(a_num.cmp(&b_num)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Credential, Identity};

    fn account(id: &str, provider: ProviderKind) -> Account {
        Account {
            schema_version: crate::account::SCHEMA_VERSION,
            id: id.to_string(),
            provider,
            label: None,
            identity: Identity {
                email: Some(format!("{id}@example.com")),
                ..Default::default()
            },
            credential: Credential {
                access_token: "access".into(),
                refresh_token: format!("refresh-{id}"),
                id_token: None,
                expires_at: None,
                refresh_expires_at: None,
            },
            provider_data: Default::default(),
            added_at: Timestamp::from_second(1_700_000_000).unwrap(),
            entitlement_checked_at: None,
            needs_login: None,
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        (dir, store)
    }

    #[test]
    fn accounts_round_trip_and_sort_naturally() {
        let (_tmp, store) = store();
        for id in ["claude-10", "claude-2", "codex-1"] {
            let provider = if id.starts_with("codex") {
                ProviderKind::Codex
            } else {
                ProviderKind::Claude
            };
            store.put_account(&account(id, provider)).unwrap();
        }
        let ids: Vec<_> = store.accounts().unwrap().into_iter().map(|a| a.id).collect();
        assert_eq!(ids, ["claude-2", "claude-10", "codex-1"]);
        assert_eq!(
            store
                .account("claude-2")
                .unwrap()
                .unwrap()
                .identity
                .email
                .unwrap(),
            "claude-2@example.com"
        );
        assert!(store.account("nope").unwrap().is_none());
    }

    #[test]
    fn ids_are_allocated_without_reuse() {
        let (_tmp, store) = store();
        assert_eq!(store.next_id(ProviderKind::Claude).unwrap(), "claude-1");
        store
            .put_account(&account("claude-1", ProviderKind::Claude))
            .unwrap();
        store
            .put_account(&account("claude-2", ProviderKind::Claude))
            .unwrap();
        assert!(store.remove_account("claude-2").unwrap());
        assert_eq!(store.next_id(ProviderKind::Claude).unwrap(), "claude-2");
        store
            .put_account(&account("claude-5", ProviderKind::Claude))
            .unwrap();
        assert_eq!(store.next_id(ProviderKind::Claude).unwrap(), "claude-6");
        assert_eq!(store.next_id(ProviderKind::Codex).unwrap(), "codex-1");
    }

    /// An id is a name somebody writes down — in a script, in a shell history,
    /// in a note to themselves. Counting from the accounts that exist hands
    /// the highest number back the moment that account is removed, and then
    /// the name means somebody else.
    #[test]
    fn an_id_is_never_handed_out_a_second_time() {
        let (_tmp, store) = store();
        for id in ["claude-1", "claude-2", "claude-3"] {
            let minted = store.next_id(ProviderKind::Claude).unwrap();
            assert_eq!(minted, id);
            store
                .put_account(&account(&minted, ProviderKind::Claude))
                .unwrap();
        }

        // Remove the highest, which is the one whose number used to come back.
        assert!(store.remove_account("claude-3").unwrap());
        assert_eq!(store.next_id(ProviderKind::Claude).unwrap(), "claude-4");

        // And removing all of them does not start the count again.
        for id in ["claude-1", "claude-2"] {
            store.remove_account(id).unwrap();
        }
        assert!(store.accounts().unwrap().is_empty());
        assert_eq!(store.next_id(ProviderKind::Claude).unwrap(), "claude-5");

        // A store written before the count was kept carries on from what is
        // on disk rather than from one.
        let (_older_tmp, older) = self::tests::store();
        older
            .put_account(&account("claude-7", ProviderKind::Claude))
            .unwrap();
        assert_eq!(older.next_id(ProviderKind::Claude).unwrap(), "claude-8");
    }

    /// A poll lets go of the store lock while it is on the network, so an
    /// account removed meanwhile has its reading written back after it is
    /// gone. A reading carries a stranger's usage and their address.
    #[test]
    fn a_reading_does_not_outlive_the_account_it_belongs_to() {
        let mut cache = UsageCache::default();
        for id in ["claude-1", "claude-2"] {
            cache.record_failure(id, "went wrong".into(), None);
        }
        cache.retain(&["claude-1".to_string()]);
        assert!(cache.get("claude-1").is_some());
        assert!(
            cache.get("claude-2").is_none(),
            "a removed account kept its reading"
        );

        cache.retain(&[]);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn removing_an_account_drops_its_cached_usage() {
        let (_tmp, store) = store();
        store
            .put_account(&account("claude-1", ProviderKind::Claude))
            .unwrap();
        let mut cache = store.usage_cache().unwrap();
        cache.record_success(
            "claude-1",
            Usage {
                observed_at: Timestamp::from_second(1).unwrap(),
                windows: vec![],
                limit_reached: false,
            },
        );
        store.put_usage_cache(&cache).unwrap();
        store.remove_account("claude-1").unwrap();
        assert!(store.usage_cache().unwrap().get("claude-1").is_none());
        assert!(!store.remove_account("claude-1").unwrap());
    }

    #[test]
    fn rejects_ids_that_would_escape_the_accounts_directory() {
        let (_tmp, store) = store();
        for bad in ["../escape", "a/b", "", "-leading"] {
            let mut a = account("claude-1", ProviderKind::Claude);
            a.id = bad.to_string();
            assert!(store.put_account(&a).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rejects_records_from_a_newer_schema() {
        let (_tmp, store) = store();
        let mut a = account("claude-1", ProviderKind::Claude);
        a.schema_version = crate::account::SCHEMA_VERSION + 1;
        store.put_account(&a).unwrap();
        let err = store.accounts().unwrap_err().to_string();
        assert!(err.contains("newer agent-meter"), "{err}");
    }

    #[test]
    fn corrupt_cache_falls_back_to_empty() {
        let (_tmp, store) = store();
        std::fs::write(store.dir().join("usage.json"), b"{ not json").unwrap();
        assert!(store.usage_cache().unwrap().entries.is_empty());
    }
}
