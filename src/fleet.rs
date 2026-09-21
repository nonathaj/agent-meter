//! Local account homes and durable conversation affinity.
//!
//! Native CLIs own the credentials in registered homes. We only capture them;
//! copying or refreshing them here would introduce a second refresh writer.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::account::{Account, Captured, Match, ProviderKind, same_identity};
use crate::fsutil::{self, Mode};
use crate::provider;
use crate::store::Store;

/// Machine-local state. Never exported with account credentials.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fleet {
    version: u32,
    pub homes: BTreeMap<String, PathBuf>,
    pub bindings: Vec<Binding>,
}

impl Default for Fleet {
    fn default() -> Self {
        Self {
            version: 1,
            homes: BTreeMap::new(),
            bindings: Vec::new(),
        }
    }
}

/// An immutable assignment, independent of the OS process running it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub scope: String,
    pub provider: ProviderKind,
    pub session: String,
    pub account: String,
    pub created_at: Timestamp,
}

/// Credential-free launch metadata, also suitable for orchestrator discovery.
#[derive(Debug, Serialize)]
pub struct Launch {
    pub scope: String,
    pub provider: ProviderKind,
    pub session: String,
    pub account: String,
    pub home: PathBuf,
    pub transcript_roots: Vec<PathBuf>,
    pub environment: BTreeMap<String, String>,
}

impl Fleet {
    /// Reads authoritative affinity state. Corruption must never reassign work.
    pub fn load(store: &Store) -> Result<Self> {
        let path = store.dir().join("fleet.json");
        let Some(bytes) = fsutil::read_optional(&path)? else {
            return Ok(Self::default());
        };
        let fleet: Self =
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
        ensure!(
            fleet.version == 1,
            "unsupported fleet.json version {}",
            fleet.version
        );
        for home in fleet.homes.values() {
            ensure!(home.is_absolute(), "fleet.json home must be absolute");
        }
        for (index, binding) in fleet.bindings.iter().enumerate() {
            ensure!(
                fleet.homes.contains_key(&binding.account),
                "fleet.json binding has no registered home"
            );
            ensure!(
                !fleet.bindings[..index].iter().any(|b| b.scope == binding.scope
                    && b.provider == binding.provider
                    && b.session == binding.session),
                "duplicate session binding in fleet.json"
            );
        }
        Ok(fleet)
    }

    fn save(&self, store: &Store) -> Result<()> {
        fsutil::write_atomic(
            &store.dir().join("fleet.json"),
            &serde_json::to_vec_pretty(self)?,
            Mode::Private,
        )
        .context("saving fleet.json")
    }

    /// Registers an existing, independently authenticated home without writing it.
    pub fn register(store: &Store, id: &str, home: &Path) -> Result<PathBuf> {
        ensure!(home.is_absolute(), "fleet home must be an absolute path");
        let home = home.canonicalize().context("opening fleet home")?;
        ensure!(home.is_dir(), "fleet home must be a directory");
        let _lock = store.lock()?;
        let mut fleet = Self::load(store)?;
        let account = store.account(id)?.context("account does not exist")?;
        if let Some(existing) = fleet.homes.get(id) {
            ensure!(
                *existing == home,
                "{id} already has a fleet home; moving it would break session affinity"
            );
        }
        ensure!(
            !fleet
                .homes
                .iter()
                .any(|(other, path)| other != id && *path == home),
            "fleet home is already registered to another account"
        );
        let native = provider::get(account.provider);
        // The default home remains controlled by use/watch/cswap. A fleet home
        // must not be that directory, including when a symlink aliases it.
        let default = crate::paths::home_dir()?.join(match account.provider {
            ProviderKind::Claude => ".claude",
            ProviderKind::Codex => ".codex",
        });
        let configured = native.config_home()?;
        for global in [default, configured] {
            let global = global.canonicalize().unwrap_or(global);
            ensure!(
                global != home,
                "the global CLI home cannot be registered for fleet use"
            );
            if let Ok(live) = native.capture(&global) {
                let candidate = native.capture(&home)?;
                ensure!(
                    live.credential.refresh_token != candidate.credential.refresh_token,
                    "fleet home shares a refresh credential with the global CLI; use an independent login or move an inactive home, not a credential copy"
                );
            }
        }
        let captured = capture_verified(&account, &home)?;
        let _account_lock = store.lock_account(id)?;
        adopt(store, &account, captured)?;
        fleet.homes.insert(id.to_owned(), home.clone());
        fleet.save(store)?;
        Ok(home)
    }

    /// Resolves or records a conversation's assignment under the store lock.
    pub fn launch(
        store: &Store,
        provider: ProviderKind,
        scope: &str,
        session: &str,
        account: Option<&str>,
        commit: bool,
    ) -> Result<Launch> {
        ensure!(
            !scope.trim().is_empty() && !session.trim().is_empty(),
            "scope and session must be nonempty"
        );
        let _lock = store.lock()?;
        let mut fleet = Self::load(store)?;
        let bound = fleet
            .bindings
            .iter()
            .find(|b| b.scope == scope && b.provider == provider && b.session == session);
        let id = match (bound, account) {
            (Some(binding), Some(requested)) => {
                ensure!(
                    binding.account == requested,
                    "session is pinned to {}; refusing account change to {requested}",
                    binding.account
                );
                binding.account.clone()
            }
            (Some(binding), None) => binding.account.clone(),
            (None, Some(requested)) => requested.to_owned(),
            (None, None) => bail!("new session needs --account; automatic allocation is not enabled"),
        };
        let account = store.account(&id)?.context("bound account no longer exists")?;
        ensure!(
            account.provider == provider,
            "account belongs to {}, not {provider}",
            account.provider
        );
        let home = fleet
            .homes
            .get(&id)
            .context("account has no fleet home; run agent-meter fleet register first")?
            .clone();
        // Fail closed if a symlink or login changed after registration.
        ensure!(
            home.canonicalize().context("opening registered fleet home")? == home,
            "registered fleet home moved"
        );
        capture_verified(&account, &home)?;
        if bound.is_none() && commit {
            fleet.bindings.push(Binding {
                scope: scope.to_owned(),
                provider,
                session: session.to_owned(),
                account: id.clone(),
                created_at: Timestamp::now(),
            });
            fleet.save(store)?;
        }
        let variable = match provider {
            ProviderKind::Claude => provider::claude::CONFIG_HOME_ENV,
            ProviderKind::Codex => provider::codex::CONFIG_HOME_ENV,
        };
        let mut environment = BTreeMap::from([
            (
                variable.to_owned(),
                home.to_str().context("fleet home is not UTF-8")?.to_owned(),
            ),
            ("AGENT_METER_ACCOUNT".into(), id.clone()),
            ("AGENT_METER_SESSION".into(), session.to_owned()),
            ("AGENT_METER_SCOPE".into(), scope.to_owned()),
        ]);
        // The wrapper may run within another account's CLI environment.
        environment.insert("AGENT_METER_PROVIDER".into(), provider.to_string());
        let transcript_roots = vec![home.join(match provider {
            ProviderKind::Claude => "projects",
            ProviderKind::Codex => "sessions",
        })];
        Ok(Launch {
            scope: scope.to_owned(),
            provider,
            session: session.to_owned(),
            account: id,
            home,
            transcript_roots,
            environment,
        })
    }

    /// Refuses global activation into a registered home, including aliases.
    pub fn guard_global_home(&self, home: &Path) -> Result<()> {
        let home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
        ensure!(
            !self.homes.values().any(|registered| *registered == home),
            "fleet homes cannot be changed by global use/watch"
        );
        Ok(())
    }
}

/// Reads credentials without exchanging or writing native refresh tokens.
pub fn capture_verified(account: &Account, home: &Path) -> Result<Captured> {
    let captured = provider::get(account.provider).capture(home)?;
    ensure!(
        same_identity(&account.identity, &captured.identity) == Match::Same,
        "fleet home identity does not match {} (including workspace); refusing to launch or poll",
        account.id
    );
    Ok(captured)
}

/// Updates the meter's snapshot from the authoritative native home.
/// Caller holds the account lock. The home itself is never written.
pub fn adopt(store: &Store, account: &Account, captured: Captured) -> Result<()> {
    let mut updated = account.clone();
    updated.credential = captured.credential;
    updated.identity.update_from(&captured.identity);
    updated.provider_data = captured.provider_data;
    updated.needs_login = None;
    store.put_account(&updated)
}

impl Launch {
    /// Replaces the wrapper with the native CLI on Unix, preserving signals,
    /// PTY ownership and exit status. Windows waits on the native process.
    pub fn execute(&self, args: &[OsString]) -> Result<ExitCode> {
        let home_var = match self.provider {
            ProviderKind::Claude => provider::claude::CONFIG_HOME_ENV,
            ProviderKind::Codex => provider::codex::CONFIG_HOME_ENV,
        };
        let mut command = provider::cli_command(self.provider.as_str(), home_var, &self.home)?;
        command.envs(&self.environment);
        if self.provider == ProviderKind::Codex {
            // Agent-meter reads auth.json. Do not let an inherited keyring
            // setting make the CLI authenticate from another credential store.
            command.args(["-c", "cli_auth_credentials_store=\"file\""]);
        }
        command.args(args);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            Err(command.exec()).context("executing pinned agent CLI")
        }
        #[cfg(not(unix))]
        {
            let status = command.status().context("running pinned agent CLI")?;
            Ok(ExitCode::from(status.code().unwrap_or(1).try_into().unwrap_or(1)))
        }
    }
}
