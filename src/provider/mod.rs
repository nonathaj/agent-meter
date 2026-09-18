//! Provider adapters: everything that is specific to one agent CLI.
//!
//! A provider knows four things: where the CLI keeps its credentials, how to
//! read an account out of that location, how to write one back, and how to ask
//! the vendor's API about usage and tokens.

pub mod claude;
pub mod codex;
mod secret_store;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::account::{Account, Captured, Credential, Identity, ProviderKind};
use crate::http;
use crate::usage::Usage;

/// The rate used for a provider that has not stated its own.
///
/// Deliberately the slow one. A provider polled too slowly shows a figure a few
/// minutes old; one polled too fast can be refused for an hour, which is an
/// hour with nothing to switch on.
pub const CAUTIOUS_POLL_INTERVAL: Duration = Duration::from_secs(300);

/// The adapter for one agent CLI.
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// The CLI's live configuration directory, honouring its own environment
    /// override (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`).
    fn config_home(&self) -> Result<PathBuf>;

    /// Reads the account currently signed in to `home`.
    fn capture(&self, home: &Path) -> Result<Captured>;

    /// Writes `account` into `home`, making it the CLI's signed-in account.
    ///
    /// Implementations merge into the existing files so unrelated settings and
    /// machine-scoped secrets survive.
    fn install(&self, home: &Path, account: &Account) -> Result<()>;

    /// Prepares `home` as an isolated configuration directory, then returns the
    /// command that performs an interactive login into it.
    fn login_command(&self, home: &Path, device_code: bool) -> Result<Command>;

    /// Deletes a throwaway configuration home once its account has been
    /// captured, including any credential the CLI stored outside the directory.
    fn discard_home(&self, home: &Path) -> Result<()> {
        crate::fsutil::remove_dir_all_if_exists(home).with_context(|| format!("removing {}", home.display()))
    }

    /// Reads the account's current usage.
    fn fetch_usage(&self, credential: &Credential) -> http::Result<Usage>;

    /// Reads identity details the credential alone does not carry. Providers
    /// that need no extra call return what they can derive.
    fn fetch_identity(&self, credential: &Credential) -> http::Result<Identity>;

    /// Exchanges the refresh token for a fresh credential.
    fn refresh(&self, credential: &Credential) -> http::Result<Credential>;

    /// Whether a running session of this CLI keeps using the old account after
    /// a swap, so the user must restart it.
    fn restarts_sessions(&self) -> bool;

    /// The closest together this provider's accounts may be polled for usage.
    ///
    /// Providers differ in what they tolerate, so this is theirs to answer
    /// rather than one number applied to all of them. The default is the
    /// cautious rate: guessing slow costs a stale figure, while guessing fast
    /// can cost an hour of being refused outright.
    fn poll_interval(&self) -> Duration {
        CAUTIOUS_POLL_INTERVAL
    }
}

/// Returns the adapter for `kind`.
pub fn get(kind: ProviderKind) -> &'static dyn Provider {
    match kind {
        ProviderKind::Claude => &claude::Claude,
        ProviderKind::Codex => &codex::Codex,
    }
}

/// Every adapter.
pub fn all() -> impl Iterator<Item = &'static dyn Provider> {
    ProviderKind::ALL.into_iter().map(get)
}

/// Resolves a CLI's configuration home from `env_var`, falling back to
/// `~/`-relative `default`.
///
/// An override is used verbatim but must be absolute: a relative path would
/// resolve against whatever directory the agent happened to start in, so the
/// CLI and agent-meter could disagree about which account is live.
pub fn config_home_from_env(env_var: &str, default: &str) -> Result<PathBuf> {
    if let Some(value) = std::env::var_os(env_var).filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        anyhow::ensure!(
            path.is_absolute(),
            "{env_var} must be an absolute path, got {}",
            path.display()
        );
        return Ok(path);
    }
    Ok(crate::paths::home_dir()?.join(default))
}

/// Environment variables that force an agent CLI to use a fixed key or token.
/// They are cleared for logins and captures so the CLI performs a real OAuth
/// sign-in instead of silently adopting an ambient credential.
pub const AUTH_OVERRIDE_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
];

/// Variables an agent sets for its own child processes. Leaving them set makes
/// the CLI think it is a nested session and change its behaviour.
pub const SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_EXECPATH",
    "CODEX_THREAD_ID",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "AI_AGENT",
];

/// Names the agent session this process is running inside, if it is.
///
/// The same markers a CLI sets for its children are what identify a session
/// from within one, so this reads exactly the list that is scrubbed for logins.
pub fn inside_agent_session() -> Option<&'static str> {
    SESSION_VARS
        .iter()
        .find(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty()))
        .copied()
}

/// Builds a command for `program`, resolved through `PATH` (which on Windows
/// means finding `claude.cmd` or `codex.cmd`), with inherited agent state
/// stripped and the CLI pointed at `home`.
pub fn cli_command(program: &str, home_var: &str, home: &Path) -> Result<Command> {
    let path = which::which(program).map_err(|_| {
        anyhow::anyhow!(
            "could not find `{program}` on PATH. Install the {program} CLI, or run \
             `agent-meter import` instead to pick up an account it already saved."
        )
    })?;

    let mut command = new_command(&path);
    for var in AUTH_OVERRIDE_VARS.iter().chain(SESSION_VARS) {
        command.env_remove(var);
    }
    command.env(home_var, home);
    Ok(command)
}

/// Creates a command for `path`, routing Windows batch shims through `cmd /c`
/// because `CreateProcess` cannot execute a `.cmd` file directly.
fn new_command(path: &Path) -> Command {
    let is_shim = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    if cfg!(windows) && is_shim {
        let mut command = Command::new("cmd");
        command.arg("/c").arg(path);
        command
    } else {
        Command::new(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_an_adapter() {
        for kind in ProviderKind::ALL {
            assert_eq!(get(kind).kind(), kind);
        }
        assert_eq!(all().count(), ProviderKind::ALL.len());
    }

    /// The point of a second configuration directory is that the agent CLI the
    /// user has open keeps its own credentials, so a login must be pointed at
    /// the throwaway home and stripped of anything that would short-circuit it.
    #[test]
    fn a_login_is_isolated_from_the_running_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("login-home");

        for (kind, var) in [
            (ProviderKind::Claude, claude::CONFIG_HOME_ENV),
            (ProviderKind::Codex, codex::CONFIG_HOME_ENV),
        ] {
            let provider = get(kind);
            // The CLI may not be installed on the machine running the tests;
            // that failure is about PATH, not about isolation.
            let Ok(command) = provider.login_command(&home, false) else {
                continue;
            };

            let envs: Vec<_> = command.get_envs().collect();
            let set = |name: &str| {
                envs.iter()
                    .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                    .map(|(_, value)| *value)
            };
            assert_eq!(
                set(var).flatten().map(std::path::PathBuf::from),
                Some(home.clone()),
                "{kind} login must use the throwaway home"
            );
            for var in AUTH_OVERRIDE_VARS.iter().chain(SESSION_VARS) {
                assert_eq!(set(var), Some(None), "{kind} login must clear {var}");
            }

            let args: Vec<_> = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(args.iter().any(|a| a == "login"), "{kind}: {args:?}");
            assert!(home.exists(), "{kind} must prepare the login directory");
        }
    }

    #[test]
    fn env_override_must_be_absolute() {
        // SAFETY: single-threaded test; no other thread reads the environment.
        unsafe { std::env::set_var("AGENT_METER_TEST_HOME", "relative/path") };
        let err = config_home_from_env("AGENT_METER_TEST_HOME", ".claude").unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
        unsafe { std::env::remove_var("AGENT_METER_TEST_HOME") };
        assert!(
            config_home_from_env("AGENT_METER_TEST_HOME", ".claude")
                .unwrap()
                .ends_with(".claude")
        );
    }
}
