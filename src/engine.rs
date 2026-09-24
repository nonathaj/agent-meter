//! The operations the CLI and TUI both drive.
//!
//! Everything that mutates the store or an agent CLI's configuration goes
//! through here, so the two front ends cannot drift apart.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value};

use crate::account::{Account, Captured, Credential, Match, ProviderKind, SCHEMA_VERSION, same_identity};
use crate::config::Config;
use crate::http;
use crate::policy::{self, Candidate, Decision, Disruption, Rules};
use crate::provider::{self, Provider};
use crate::remote::Report;
use crate::store::{Store, SwitchRecord, UsageCache};
use crate::usage::Usage;

/// Refresh an access token this long before it expires, so a poll never races
/// the expiry.
const REFRESH_LEEWAY: SignedDuration = SignedDuration::from_mins(10);

/// How long a login directory may sit before it is assumed to be the remains of
/// an interrupted login. Longer than any sign-in a person would still be doing.
const LOGIN_HOME_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// How long an account's stated plan and quota size are taken on trust.
///
/// Six hours is far slower than usage is polled, and far faster than anyone
/// changes plan — about four requests a day for the one account in use, against
/// a budget measured in tens per hour.
const ENTITLEMENT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

pub struct Engine {
    store: Store,
    config: Config,
}

/// An account plus everything known about it right now.
#[derive(Debug, Clone)]
pub struct Status {
    pub account: Account,
    pub active: bool,
    /// A native CLI home owns this account; global switching excludes it.
    pub fleet_home: Option<PathBuf>,
    pub usage: Option<Usage>,
    /// Why the last usage poll failed, if it did, and when.
    pub error: Option<String>,
    pub error_at: Option<Timestamp>,
}

impl Status {
    /// Percent of the tightest window used, if known.
    pub fn used(&self, now: Timestamp) -> Option<f64> {
        self.usage.as_ref().map(|usage| usage.worst_at(now))
    }
}

/// What adding an account did.
#[derive(Debug, Clone, PartialEq)]
pub enum AddOutcome {
    /// A new account was stored.
    Added { id: String },
    /// The account was already stored; its tokens were brought up to date.
    Updated { id: String },
}

impl AddOutcome {
    pub fn id(&self) -> &str {
        match self {
            AddOutcome::Added { id } | AddOutcome::Updated { id } => id,
        }
    }
}

/// What a switch did.
#[derive(Debug, Clone)]
pub struct SwitchOutcome {
    pub to: String,
    pub from: Option<String>,
    /// Whether running sessions of this CLI must be restarted to see the change.
    pub restart_required: bool,
}

/// What one watcher tick did for one provider.
#[derive(Debug, Clone)]
pub struct TickOutcome {
    pub provider: ProviderKind,
    pub decision: Decision,
    /// Set when the decision was a switch and the switch was carried out.
    pub switched: Option<SwitchOutcome>,
    /// Set when a switch was wanted but held back.
    pub held: Option<String>,
}

impl Engine {
    pub fn open() -> Result<Self> {
        let store = Store::open_default()?;
        let config = store.config()?;
        Ok(Self { store, config })
    }

    pub fn with_store(store: Store) -> Result<Self> {
        let config = store.config()?;
        Ok(Self { store, config })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Applies command-line overrides to this run's settings, without writing
    /// them to the configuration file.
    pub fn override_watch(&mut self, threshold: Option<f64>, poll_secs: Option<u64>) -> Result<()> {
        let mut config = self.config.clone();
        if let Some(threshold) = threshold {
            config.set("watch.threshold", &threshold.to_string())?;
        }
        if let Some(poll_secs) = poll_secs {
            config.set("watch.poll-secs", &poll_secs.to_string())?;
        }
        self.config = config;
        Ok(())
    }

    /// Every stored account, with cached usage and which one is live.
    ///
    /// This also adopts any credential an agent CLI rotated on its own, so the
    /// stored copy never falls behind the CLI's.
    pub fn status(&self) -> Result<Vec<Status>> {
        let _lock = self.store.lock()?;
        let fleet = crate::fleet::Fleet::load(&self.store)?;
        let accounts = self.store.accounts()?;
        let mut active_ids = Vec::new();
        for kind in ProviderKind::ALL {
            if accounts.iter().any(|a| a.provider == kind)
                && let Some(id) = self.sync_active(kind, &accounts)?
            {
                active_ids.push(id);
            }
        }

        // Re-read: syncing may have rewritten credentials.
        let accounts = self.store.accounts()?;
        let cache = self.store.usage_cache()?;
        Ok(accounts
            .into_iter()
            .map(|account| {
                let entry = cache.get(&account.id);
                Status {
                    active: active_ids.contains(&account.id),
                    fleet_home: fleet.homes.get(&account.id).cloned(),
                    usage: entry.and_then(|e| e.usage.clone()),
                    error: entry.and_then(|e| e.error.clone()),
                    error_at: entry.and_then(|e| e.failed_at),
                    account,
                }
            })
            .collect())
    }

    /// The order this provider's accounts would be taken in, best next first.
    ///
    /// The same ranking the watcher switches on, so a list showing this order
    /// is showing what will happen rather than an arrangement of its own.
    pub fn switch_order(&self, statuses: &[Status], kind: ProviderKind) -> Vec<String> {
        let mine: Vec<&Status> = statuses.iter().filter(|s| s.account.provider == kind).collect();
        let candidates: Vec<Candidate<'_>> = mine.iter().map(|s| self.candidate(s)).collect();
        policy::queue(&candidates, &self.rules_for(kind), Timestamp::now())
            .into_iter()
            .map(ToString::to_string)
            .collect()
    }

    /// How one account looks to the policy.
    fn candidate<'a>(&self, status: &'a Status) -> Candidate<'a> {
        Candidate {
            id: &status.account.id,
            usage: status.usage.as_ref(),
            active: status.active,
            usable: status.account.needs_login.is_none() && status.fleet_home.is_none(),
            capacity: status.account.identity.capacity,
        }
    }

    /// The rules this provider is judged by.
    pub fn rules_for(&self, kind: ProviderKind) -> Rules {
        Rules {
            threshold: self.config.threshold_for(kind),
            margin: self.config.watch.margin,
            // Taken from the provider's own answer about restarts, so a new
            // provider inherits the behaviour instead of being named in the
            // policy.
            disruption: if provider::get(kind).restarts_sessions() {
                Disruption::RestartsSessions
            } else {
                Disruption::Seamless
            },
            ..Rules::default()
        }
    }

    /// Resolves a user-supplied account reference: an id, an email, or a label.
    pub fn resolve(&self, reference: &str) -> Result<Account> {
        let accounts = self.store.accounts()?;
        if let Some(account) = accounts.iter().find(|a| a.id == reference) {
            return Ok(account.clone());
        }
        let matches: Vec<_> = accounts
            .iter()
            .filter(|a| {
                a.label
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(reference))
                    || a.identity
                        .email
                        .as_deref()
                        .is_some_and(|e| e.eq_ignore_ascii_case(reference))
            })
            .collect();
        match matches.as_slice() {
            [account] => Ok((*account).clone()),
            [] => bail!("no account matches {reference:?}. Run `agent-meter list` to see them."),
            many => {
                let ids: Vec<_> = many.iter().map(|a| a.id.as_str()).collect();
                bail!(
                    "{reference:?} matches several accounts ({}); use the id instead",
                    ids.join(", ")
                )
            }
        }
    }

    /// Stores the account an agent CLI is currently signed in to.
    pub fn import(&self, kind: ProviderKind, label: Option<String>) -> Result<AddOutcome> {
        let provider = provider::get(kind);
        let home = provider.config_home()?;
        let mut captured = provider.capture(&home)?;
        name_account(kind, &mut captured);
        let _lock = self.store.lock()?;
        self.store_captured(kind, captured, label)
    }

    /// Stores every account another tool is holding.
    ///
    /// The other tool's files are only read, so its own setup is left working;
    /// an account it shares with one already stored here updates that one
    /// rather than arriving twice.
    pub fn import_from(
        &self,
        source: crate::foreign::Source,
        dir: Option<&std::path::Path>,
        only: Option<ProviderKind>,
    ) -> Result<Vec<(String, AddOutcome)>> {
        let mut found = crate::foreign::read(source, dir)?;
        found.retain(|f| only.is_none_or(|kind| f.provider == kind));
        if found.is_empty() {
            bail!(
                "{} is not holding any account agent-meter can manage",
                source.display_name()
            );
        }

        // Named before the lock is taken: naming asks the provider who an
        // account belongs to, and that is a network call.
        for account in &mut found {
            name_account(account.provider, &mut account.captured);
        }

        let _lock = self.store.lock()?;
        let mut outcomes = Vec::new();
        for account in found {
            let outcome = self.store_captured(account.provider, account.captured, account.label)?;
            outcomes.push((account.origin, outcome));
        }
        Ok(outcomes)
    }

    /// Logs in to a new account inside a throwaway configuration directory, so
    /// the agent CLI the user is running right now keeps its own credentials.
    ///
    /// `run` is handed the prepared command and must run it to completion; the
    /// caller owns the terminal, which an interactive OAuth flow needs.
    pub fn login(
        &self,
        kind: ProviderKind,
        label: Option<String>,
        device_code: bool,
        run: impl FnOnce(&mut std::process::Command) -> Result<std::process::ExitStatus>,
    ) -> Result<AddOutcome> {
        let provider = provider::get(kind);
        let home = self.login_home(kind)?;

        let result = (|| {
            let mut command = provider.login_command(&home, device_code)?;
            let status = run(&mut command).with_context(|| format!("running the {kind} login"))?;
            if !status.success() {
                bail!(
                    "the {kind} login exited with {status}. Nothing was added; \
                     the CLI you already had signed in is untouched."
                );
            }
            provider.capture(&home).with_context(|| {
                format!("the {kind} login finished but left no credential agent-meter could read")
            })
        })();

        // The throwaway home holds a live credential; remove it whether or not
        // the login worked.
        let discarded = provider.discard_home(&home);
        let mut captured = result?;
        discarded?;

        name_account(kind, &mut captured);
        let _lock = self.store.lock()?;
        self.store_captured(kind, captured, label)
    }

    /// Where a login's isolated configuration directory lives.
    ///
    /// It sits in agent-meter's own data directory rather than the system temp
    /// directory: Codex refuses to create its helper binaries under temp.
    fn login_home(&self, kind: ProviderKind) -> Result<PathBuf> {
        self.sweep_login_homes(LOGIN_HOME_MAX_AGE);
        let unique = format!(
            "{kind}-{}-{:x}",
            std::process::id(),
            Timestamp::now().as_nanosecond() as u64
        );
        let home = self.store.dir().join("logins").join(unique);
        crate::fsutil::create_private_dir(&home).with_context(|| format!("creating {}", home.display()))?;
        Ok(home)
    }

    /// Deletes login directories left behind by an interrupted login.
    ///
    /// A login that finishes cleans up after itself, but one killed part-way
    /// through leaves a directory that may hold a real credential. Anything
    /// older than `older_than` cannot belong to a login still in progress.
    fn sweep_login_homes(&self, older_than: std::time::Duration) {
        let Ok(entries) = std::fs::read_dir(self.store.dir().join("logins")) else {
            return;
        };
        let Some(cutoff) = std::time::SystemTime::now().checked_sub(older_than) else {
            return;
        };
        for entry in entries.flatten() {
            let is_stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|modified| modified < cutoff);
            if is_stale {
                // Best effort: a directory another process is using right now
                // simply stays until next time.
                let _ = crate::fsutil::remove_dir_all_if_exists(&entry.path());
            }
        }
    }

    /// Writes a captured account into the store, merging it into an existing
    /// record when it is the same account signing in again.
    fn store_captured(
        &self,
        kind: ProviderKind,
        captured: Captured,
        label: Option<String>,
    ) -> Result<AddOutcome> {
        let existing = self.store.accounts()?.into_iter().find(|a| {
            a.provider == kind
                && (a.credential.refresh_token == captured.credential.refresh_token
                    || same_identity(&a.identity, &captured.identity) == Match::Same)
        });

        // A plan here came from the provider a moment ago, in `name_account`
        // for Claude or from the signed token claims for Codex, so it starts
        // the clock rather than leaving the next poll to ask again.
        let checked_at = captured.identity.plan.is_some().then(Timestamp::now);

        if let Some(mut account) = existing {
            // Two tools can be holding the same account with different
            // credentials, and only one of them still works: refreshing
            // rotates a single-use refresh token, so whichever copy was
            // refreshed last is live and the other was spent at that moment.
            // A store nobody has opened in a week holds the spent one, and
            // taking it would sign the account out.
            //
            // What sits beside the credential — the scopes it was issued with,
            // the CLI's own record of the account — describes that credential,
            // so it travels with it or not at all.
            if supersedes(&captured.credential, &account.credential) {
                account.credential = captured.credential;
                account.needs_login = None;
                overlay_provider_data(&mut account.provider_data, captured.provider_data);
            }
            account.identity.update_from(&captured.identity);
            if checked_at.is_some() {
                account.entitlement_checked_at = checked_at;
            }
            if label.is_some() {
                account.label = label;
            }
            self.store.put_account(&account)?;
            return Ok(AddOutcome::Updated { id: account.id });
        }

        let account = Account {
            schema_version: SCHEMA_VERSION,
            id: self.store.next_id(kind)?,
            provider: kind,
            label,
            identity: captured.identity,
            credential: captured.credential,
            provider_data: captured.provider_data,
            added_at: Timestamp::now(),
            entitlement_checked_at: checked_at,
            needs_login: None,
        };
        self.store.put_account(&account)?;
        Ok(AddOutcome::Added { id: account.id })
    }

    /// Every account, as another machine receives it.
    ///
    /// Accounts the provider has already rejected here are left out. Their
    /// credential is known not to work, and the only thing sending it could
    /// achieve is replacing a working copy somewhere else.
    pub fn records(&self) -> Result<Vec<crate::remote::Record>> {
        let _lock = self.store.lock()?;
        // A CLI may have rotated its credential since anything last looked,
        // and the copy stored from before that is spent. Sending it would
        // hand the other machine an account that is already signed out.
        let accounts = self.store.accounts()?;
        for kind in ProviderKind::ALL {
            if accounts.iter().any(|a| a.provider == kind) {
                self.sync_active(kind, &accounts)?;
            }
        }
        let fleet = crate::fleet::Fleet::load(&self.store)?;
        Ok(self
            .store
            .accounts()?
            .into_iter()
            .filter(|account| account.needs_login.is_none() && !fleet.homes.contains_key(&account.id))
            .map(|account| crate::remote::Record {
                provider: account.provider,
                label: account.label,
                identity: account.identity,
                credential: account.credential,
                provider_data: account.provider_data,
                added_at: account.added_at,
                entitlement_checked_at: account.entitlement_checked_at,
                origin_id: account.id,
            })
            .collect())
    }

    /// Takes accounts from another machine.
    ///
    /// The ids they carry are the other machine's, so matching is by identity
    /// — the address and the workspace — exactly as importing from another
    /// tool is. A credential is taken only when it supersedes the one held,
    /// because a refresh token is single-use and the older of two copies is
    /// the one that was spent.
    ///
    /// With `apply` false nothing is written and the report says what would
    /// have been.
    pub fn absorb(&self, records: &[crate::remote::Record], apply: bool) -> Result<Report> {
        if records.is_empty() {
            return Ok(Report::default());
        }
        let _lock = self.store.lock()?;
        let mut held = self.store.accounts()?;
        let mut report = Report::default();

        for record in records {
            let found = held.iter().position(|account| {
                account.provider == record.provider
                    && (account.credential.refresh_token == record.credential.refresh_token
                        || same_identity(&account.identity, &record.identity) == Match::Same)
            });

            let Some(at) = found else {
                report.added.push(record.origin_id.clone());
                if apply {
                    let account = Account {
                        schema_version: SCHEMA_VERSION,
                        id: self.store.next_id(record.provider)?,
                        provider: record.provider,
                        label: record.label.clone(),
                        identity: record.identity.clone(),
                        credential: record.credential.clone(),
                        provider_data: record.provider_data.clone(),
                        added_at: record.added_at,
                        entitlement_checked_at: record.entitlement_checked_at,
                        needs_login: None,
                    };
                    self.store.put_account(&account)?;
                    held.push(account);
                }
                continue;
            };

            // A record is one machine's snapshot of an account, and what
            // dates it is the credential: everything else in it was read at
            // the same moment. So it is taken whole or not at all. The same
            // token is the same generation and carries no news; an older one
            // carries nothing but stale news.
            let mut account = held[at].clone();
            if record.credential.refresh_token == account.credential.refresh_token
                || !supersedes(&record.credential, &account.credential)
            {
                report.unchanged += 1;
                continue;
            }
            account.credential = record.credential.clone();
            // Whatever a provider said about the credential just replaced is
            // a verdict on a credential this account no longer has.
            account.needs_login = None;
            account.identity.update_from(&record.identity);
            overlay_provider_data(&mut account.provider_data, record.provider_data.clone());
            // The name this machine gave it stays: it is what the person
            // sitting at this machine calls the account.
            if account.label.is_none() {
                account.label.clone_from(&record.label);
            }
            if record.entitlement_checked_at.is_some() {
                account.entitlement_checked_at = record.entitlement_checked_at;
            }

            // Machines already in step should be quiet: a report that lists
            // every account every time says nothing at all.
            if account == held[at] {
                report.unchanged += 1;
                continue;
            }
            report.updated.push(account.id.clone());
            if apply {
                self.store.put_account(&account)?;
            }
            held[at] = account;
        }
        Ok(report)
    }

    /// A tag for the credentials held, which changes when any of them does.
    ///
    /// Refresh tokens rotate, so comparing fingerprints answers "has anything
    /// happened worth telling the other machines about" without the tokens
    /// themselves being compared, copied or printed.
    pub fn credential_mark(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .store
            .accounts()?
            .into_iter()
            .map(|account| (account.id, account.credential.fingerprint()))
            .collect())
    }

    /// Forgets an account. The provider is not told, so the credential stays
    /// valid; the user can sign in again at any time.
    pub fn remove(&self, id: &str) -> Result<bool> {
        let _lock = self.store.lock()?;
        anyhow::ensure!(
            !crate::fleet::Fleet::load(&self.store)?.homes.contains_key(id),
            "account is registered for fleet use; removing it would strand pinned sessions"
        );
        self.store.remove_account(id)
    }

    /// Signs the agent CLI in to `id`.
    pub fn switch_to(&self, id: &str) -> Result<SwitchOutcome> {
        let _lock = self.store.lock()?;
        self.switch_locked(id)
    }

    fn switch_locked(&self, id: &str) -> Result<SwitchOutcome> {
        let fleet = crate::fleet::Fleet::load(&self.store)?;
        anyhow::ensure!(
            !fleet.homes.contains_key(id),
            "account is registered for fleet use; use agent-meter run instead of global switching"
        );
        let account = self
            .store
            .account(id)?
            .ok_or_else(|| anyhow!("no account with id {id:?}"))?;
        if let Some(reason) = &account.needs_login {
            bail!(
                "{id} cannot be used: {reason}. Run `agent-meter add {} --label {}` to sign in again.",
                account.provider,
                account.display_name()
            );
        }

        let provider = provider::get(account.provider);
        let home = provider.config_home()?;
        let accounts = self.store.accounts()?;
        fleet.guard_global_home(&home)?;
        // Save whatever the CLI rotated for the outgoing account before its
        // credential is overwritten.
        let from = self.sync_active(account.provider, &accounts)?;
        if from.as_deref() == Some(id) {
            return Ok(SwitchOutcome {
                to: id.to_string(),
                from,
                restart_required: false,
            });
        }

        provider.install(&home, &account)?;
        Ok(SwitchOutcome {
            to: id.to_string(),
            from,
            restart_required: provider.restarts_sessions(),
        })
    }

    /// Finds which stored account an agent CLI is signed in to, adopting any
    /// credential the CLI has rotated since it was stored.
    ///
    /// Adoption matters because refresh tokens are single-use: if the CLI
    /// refreshes and agent-meter keeps the superseded token, the stored account
    /// is one failed refresh away from needing a manual login.
    fn sync_active(&self, kind: ProviderKind, accounts: &[Account]) -> Result<Option<String>> {
        let provider = provider::get(kind);
        let home = match provider.config_home() {
            Ok(home) => home,
            // A missing home just means the CLI is not set up here.
            Err(_) => return Ok(None),
        };
        let Ok(live) = provider.capture(&home) else {
            return Ok(None);
        };

        let matched = accounts.iter().find(|a| {
            a.provider == kind
                && (a.credential.refresh_token == live.credential.refresh_token
                    || same_identity(&a.identity, &live.identity) == Match::Same)
        });
        let Some(account) = matched else {
            return Ok(None);
        };

        // Fleet credentials are read only from their registered home. A global
        // login or another tool may still hold an obsolete copy of this account.
        if crate::fleet::Fleet::load(&self.store)?
            .homes
            .contains_key(&account.id)
        {
            return Ok(None);
        }

        if account.credential == live.credential {
            return Ok(Some(account.id.clone()));
        }
        if refreshed_later(&account.credential, &live.credential) {
            // The stored copy is the newer one: the person signed this account
            // in again through agent-meter, or another machine sent a later
            // copy. The CLI's is the stale one, and adopting it would throw
            // away the login that was meant to fix it. Hand the CLI ours.
            if crate::fleet::Fleet::load(&self.store)?
                .guard_global_home(&home)
                .is_ok()
            {
                provider
                    .install(&home, account)
                    .context("giving the newer credential to the agent CLI")?;
            }
        } else {
            let mut updated = account.clone();
            updated.credential = live.credential;
            updated.identity.update_from(&live.identity);
            updated.needs_login = None;
            self.store.put_account(&updated)?;
        }
        Ok(Some(account.id.clone()))
    }

    /// Polls usage for the given accounts, or for all of them when `ids` is
    /// empty, and caches the results.
    ///
    /// `force` ignores the poll interval and any backoff. The providers rate
    /// limit these endpoints to roughly 30 requests per hour per account, so
    /// unforced polls stay inside that budget.
    pub fn poll(&self, ids: &[String], force: bool) -> Result<Vec<(String, Result<Usage, String>)>> {
        // Work out what to poll under the lock, then let go of it: a poll makes
        // network calls that can take tens of seconds, and holding the store
        // lock through them would block every other agent-meter on the machine.
        let (due, active_ids) = {
            let _lock = self.store.lock()?;
            let accounts = self.store.accounts()?;
            let mut active_ids = Vec::new();
            for kind in ProviderKind::ALL {
                if accounts.iter().any(|a| a.provider == kind)
                    && let Some(id) = self.sync_active(kind, &accounts)?
                {
                    active_ids.push(id);
                }
            }

            let accounts = self.store.accounts()?;
            let cache = self.store.usage_cache()?;
            let now = Timestamp::now();
            let due: Vec<Account> = accounts
                .into_iter()
                .filter(|a| ids.is_empty() || ids.contains(&a.id))
                .filter(|a| force || self.is_poll_due(&cache, a, now))
                .collect();
            (due, active_ids)
        };

        let polled: Vec<_> = due
            .iter()
            .map(|account| {
                let active = active_ids.contains(&account.id);
                let outcome = self.poll_one(account, active);
                if outcome.is_ok() {
                    self.learn_entitlement(account, active);
                }
                (account, outcome)
            })
            .collect();

        let _lock = self.store.lock()?;
        let mut cache = self.store.usage_cache()?;
        let mut results = Vec::new();
        for (account, outcome) in polled {
            match outcome {
                Ok(usage) => {
                    cache.record_success(&account.id, usage.clone());
                    results.push((account.id.clone(), Ok(usage)));
                }
                Err(error) => {
                    let retry_after = backoff_until(&error, cache.get(&account.id).map_or(0, |e| e.failures));
                    let message = error.to_string();
                    cache.record_failure(&account.id, message.clone(), retry_after);
                    results.push((account.id.clone(), Err(message)));
                }
            }
        }
        // Re-read under this lock rather than reusing the list from before the
        // network calls: an account removed while they ran has just had its
        // reading written back, and a reading is a stranger's usage and
        // address sitting under an id somebody else will be given.
        let kept: Vec<String> = self.store.accounts()?.into_iter().map(|a| a.id).collect();
        cache.retain(&kept);
        self.store.put_usage_cache(&cache)?;
        Ok(results)
    }

    /// Keeps an account's plan and quota size current.
    ///
    /// Asked once for any account that has neither — one imported while
    /// offline, say — because without the quota size the switching rules fall
    /// back to comparing percentages.
    ///
    /// After that only the account in use is re-asked, and on a clock measured
    /// in hours rather than minutes. These facts change when somebody changes
    /// plan or moves organisation, not as work is done, and every request spent
    /// here comes out of the same per-account budget as the usage polls that
    /// the switching actually runs on.
    fn learn_entitlement(&self, account: &Account, active: bool) {
        if !entitlement_due(account, active, Timestamp::now()) {
            return;
        }
        let asked = provider::get(account.provider).fetch_identity(&account.credential);
        // An account whose plan the provider will not state — because it is
        // offline, rate-limited, or the organisation forbids these calls — must
        // not be asked again on the very next poll. Recording the attempt puts
        // it on the same slow clock as a successful answer, so one account
        // cannot spend the budget re-asking a question that keeps failing.
        let Ok(_lock) = self.store.lock_account(&account.id) else {
            return;
        };
        // Re-read under the lock: this account's credential may have been
        // refreshed during the poll that just happened, and writing back the
        // copy taken before that would restore a spent refresh token.
        let Ok(Some(mut current)) = self.store.account(&account.id) else {
            return;
        };
        if let Ok(identity) = asked {
            current.identity.update_from(&identity);
        }
        current.entitlement_checked_at = Some(Timestamp::now());
        let _ = self.store.put_account(&current);
    }

    /// Whether `account` is due for a poll.
    fn is_poll_due(&self, cache: &UsageCache, account: &Account, now: Timestamp) -> bool {
        let Some(entry) = cache.get(&account.id) else {
            return true;
        };
        if entry.retry_after.is_some_and(|at| at > now) {
            return false;
        }
        let interval = self.poll_interval(account.provider).as_secs() as i64;
        entry.usage.as_ref().is_none_or(|usage| {
            // A window that has turned over since this reading changes what the
            // account can do, so the decision is worth re-running now rather
            // than up to a whole interval later. It fires once per reset,
            // because the reading that answers carries the next ones.
            let reset_since = usage
                .windows
                .iter()
                .filter_map(|window| window.resets_at)
                .any(|at| at > usage.observed_at && at <= now);
            reset_since || usage.age_secs(now) >= interval
        })
    }

    /// How often one provider's accounts are actually polled.
    ///
    /// Two numbers meet here. The provider states the closest together its own
    /// endpoint may be asked, which differs by provider and is not ours to
    /// choose. The configured interval is how often the watcher wakes, and a
    /// provider cannot be polled more often than that however fast it would
    /// tolerate — so the slower of the two wins, and setting a long interval
    /// slows everything down without letting a short one speed anything past
    /// what its provider allows.
    pub fn poll_interval(&self, kind: ProviderKind) -> std::time::Duration {
        provider::get(kind)
            .poll_interval()
            .max(std::time::Duration::from_secs(self.config.watch.poll_secs))
    }

    /// Reads one account's usage, refreshing its token first if needed and
    /// retrying once if the provider rejects it.
    fn poll_one(&self, account: &Account, active: bool) -> Result<Usage, http::Error> {
        let provider = provider::get(account.provider);
        let fleet = crate::fleet::Fleet::load(&self.store).map_err(http::Error::Transport)?;
        if let Some(home) = fleet.homes.get(&account.id) {
            let credential = self.capture_fleet_credential(account, home)?;
            // A registered native CLI is the sole refresh writer. An expired
            // credential produces a polling error until that CLI refreshes it.
            return provider.fetch_usage(&credential);
        }
        let mut credential = account.credential.clone();

        if expires_within(&credential, REFRESH_LEEWAY) {
            credential = self.refresh_and_store(provider, account, active)?;
        }

        match provider.fetch_usage(&credential) {
            Err(http::Error::Unauthorized { .. }) => {
                // The token was rejected even though it looked current; one
                // refresh is worth trying before declaring the account dead.
                let credential = self.refresh_and_store(provider, account, active)?;
                provider.fetch_usage(&credential)
            }
            other => other,
        }
    }

    fn capture_fleet_credential(
        &self,
        account: &Account,
        home: &std::path::Path,
    ) -> Result<Credential, http::Error> {
        let _lock = self
            .store
            .lock_account(&account.id)
            .map_err(http::Error::Transport)?;
        self.capture_fleet_credential_locked(account, home)
    }

    fn capture_fleet_credential_locked(
        &self,
        account: &Account,
        home: &std::path::Path,
    ) -> Result<Credential, http::Error> {
        let current = self
            .store
            .account(&account.id)
            .map_err(http::Error::Transport)?
            .ok_or_else(|| http::Error::Transport(anyhow!("fleet account disappeared")))?;
        let live = crate::fleet::capture_verified(&current, home).map_err(http::Error::Transport)?;
        let credential = live.credential.clone();
        crate::fleet::adopt(&self.store, &current, live).map_err(http::Error::Transport)?;
        Ok(credential)
    }

    /// Exchanges the refresh token for a fresh one and stores the result.
    ///
    /// Refresh tokens are single-use: the moment this exchange succeeds, every
    /// other copy of the old token is dead. Two things follow, and both are
    /// handled here.
    ///
    /// First, only one refresh may be in flight for an account, so this takes a
    /// per-account lock and re-reads under it — if another agent-meter got
    /// there first, its result is adopted instead of spending a second token.
    /// Second, if the agent CLI is signed in to this account, the new token has
    /// to reach the CLI as well; leaving it holding the spent one would sign
    /// the user out at its next refresh.
    fn refresh_and_store(
        &self,
        provider: &dyn Provider,
        account: &Account,
        active: bool,
    ) -> Result<Credential, http::Error> {
        let transport = |e: anyhow::Error| http::Error::Transport(e);
        let _lock = self.store.lock_account(&account.id).map_err(transport)?;

        // Registration may have completed after poll_one took its snapshot.
        if let Some(home) = crate::fleet::Fleet::load(&self.store)
            .map_err(transport)?
            .homes
            .get(&account.id)
        {
            return self.capture_fleet_credential_locked(account, home);
        }

        // Someone may have refreshed while this process waited for the lock.
        if let Ok(Some(stored)) = self.store.account(&account.id)
            && stored.credential != account.credential
        {
            return Ok(stored.credential);
        }
        // The CLI refreshes its own credential as it works. If it already has,
        // that token is the live one and ours is spent: take theirs.
        if active
            && let Ok(home) = provider.config_home()
            && let Ok(live) = provider.capture(&home)
            && live.credential != account.credential
            && !refreshed_later(&account.credential, &live.credential)
        {
            let mut updated = account.clone();
            updated.credential = live.credential.clone();
            updated.needs_login = None;
            self.store.put_account(&updated).map_err(transport)?;
            return Ok(live.credential);
        }

        match provider.refresh(&account.credential) {
            Ok(credential) => {
                let mut updated = account.clone();
                updated.credential = credential.clone();
                updated.needs_login = None;
                self.store
                    .put_account(&updated)
                    .map_err(|e| transport(e.context("saving the refreshed credential")))?;
                if active && let Ok(home) = provider.config_home() {
                    provider.install(&home, &updated).map_err(|e| {
                        transport(e.context("giving the refreshed credential to the agent CLI"))
                    })?;
                }
                Ok(credential)
            }
            Err(error) => {
                if !error.is_transient() {
                    let mut updated = account.clone();
                    updated.needs_login =
                        Some(format!("the provider rejected its credential: {}", error.brief()));
                    let _ = self.store.put_account(&updated);
                }
                Err(error)
            }
        }
    }

    /// Runs one watcher tick: poll what is due, then act on the policy.
    pub fn tick(&self) -> Result<Vec<TickOutcome>> {
        self.run_tick(true)
    }

    /// Evaluates a tick without switching anything.
    pub fn dry_tick(&self) -> Result<Vec<TickOutcome>> {
        self.run_tick(false)
    }

    fn run_tick(&self, act: bool) -> Result<Vec<TickOutcome>> {
        self.poll(&[], false)?;
        let statuses = self.status()?;
        let now = Timestamp::now();
        let mut outcomes = Vec::new();

        for kind in ProviderKind::ALL {
            if !self.config.is_enabled(kind) {
                continue;
            }
            if crate::fleet::Fleet::load(&self.store)?
                .guard_global_home(&provider::get(kind).config_home()?)
                .is_err()
            {
                continue;
            }
            let provider_statuses: Vec<_> = statuses.iter().filter(|s| s.account.provider == kind).collect();
            if provider_statuses.is_empty() {
                continue;
            }

            let candidates: Vec<Candidate<'_>> =
                provider_statuses.iter().map(|s| self.candidate(s)).collect();
            let decision = policy::decide(&candidates, &self.rules_for(kind), now);

            let (switched, held) = match &decision {
                Decision::Switch { .. } if !act => (None, None),
                Decision::Switch { to, .. } => match self.cooldown_remaining(kind, now)? {
                    Some(remaining) => (
                        None,
                        Some(format!(
                            "waiting {} before switching again",
                            crate::timefmt::duration(remaining.as_secs())
                        )),
                    ),
                    None => {
                        let outcome = self.switch_to(to)?;
                        let mut state = self.store.state()?;
                        state.record_switch(
                            kind,
                            SwitchRecord {
                                at: now,
                                to: outcome.to.clone(),
                                from: outcome.from.clone(),
                            },
                        );
                        self.store.put_state(&state)?;
                        (Some(outcome), None)
                    }
                },
                _ => (None, None),
            };
            outcomes.push(TickOutcome {
                provider: kind,
                decision,
                switched,
                held,
            });
        }
        Ok(outcomes)
    }

    /// How long remains of the cooldown after the last automatic switch.
    fn cooldown_remaining(&self, kind: ProviderKind, now: Timestamp) -> Result<Option<std::time::Duration>> {
        let cooldown = self.config.watch.cooldown_secs as i64;
        let Some(record) = self.store.state()?.last_switch(kind).cloned() else {
            return Ok(None);
        };
        let elapsed = now.as_second() - record.at.as_second();
        Ok((elapsed < cooldown).then(|| std::time::Duration::from_secs((cooldown - elapsed) as u64)))
    }
}

/// Whether `candidate` is a later copy of an account's credential than `held`.
///
/// Refreshing an OAuth credential rotates its refresh token and mints an
/// access token with a fresh expiry, so of two copies of one account the one
/// whose access token expires later is the one refreshed most recently — and
/// the other was spent at that moment. When neither states an expiry there is
/// nothing to compare, and the copy being offered is the most recent thing
/// anybody has said about the account.
fn supersedes(candidate: &Credential, held: &Credential) -> bool {
    if candidate.refresh_token == held.refresh_token {
        return true;
    }
    match (candidate.expires_at, held.expires_at) {
        (Some(offered), Some(stored)) => offered > stored,
        _ => true,
    }
}

/// Whether `a` is known to have been refreshed after `b`.
///
/// Stricter than `supersedes`, which lets an offered copy win whenever nothing
/// says otherwise. This is for the other direction: keeping the stored copy
/// over the one an agent CLI holds. The CLI's copy is the one in use, so it
/// loses only to evidence — both expiries stated, and ours later.
pub(crate) fn refreshed_later(a: &Credential, b: &Credential) -> bool {
    a.refresh_token != b.refresh_token && matches!((a.expires_at, b.expires_at), (Some(a), Some(b)) if a > b)
}

/// Lays what a newer copy of an account carries over what is held.
///
/// Sources differ in how much they keep: Claude Code's own files hold the
/// scopes and a full account record, while gemctl keeps the tokens and four
/// fields of identity. A key a source never kept is not a key it removed, so
/// what it did not say is left as it was — one level deep, so a thin
/// `oauthAccount` refreshes the fields it has without erasing the ones it
/// lacks. Replacing wholesale is what left accounts without the scopes Claude
/// Code needs to consider itself signed in.
fn overlay_provider_data(held: &mut Map<String, Value>, offered: Map<String, Value>) {
    for (key, value) in offered {
        match (held.get_mut(&key), value) {
            (Some(Value::Object(held)), Value::Object(offered)) => held.extend(offered),
            (_, value) => {
                held.insert(key, value);
            }
        }
    }
}

/// Asks the provider who an account belongs to and what it is entitled to.
///
/// The credential files name the account but do not reliably state its plan or
/// the size of its quota — Claude Code's copies of both drift from what the
/// provider reports — so the provider is asked whenever either is missing.
///
/// Best effort, and deliberately outside the store lock: it is a network call,
/// and an account with an unknown plan is better than a failed import.
fn name_account(kind: ProviderKind, captured: &mut Captured) {
    if captured.identity.email.is_some() && captured.identity.plan.is_some() {
        return;
    }
    if let Ok(identity) = provider::get(kind).fetch_identity(&captured.credential) {
        captured.identity.update_from(&identity);
    }
}

/// Whether to ask the provider what an account is entitled to.
///
/// An account that has never been told its plan asks once, because without the
/// quota size the switching rules are comparing percentages blind. After that
/// only the account in use asks again, and only when the answer is old: these
/// facts change when somebody changes plan, not as work is done, and every
/// request spent here is one the usage polls no longer have.
fn entitlement_due(account: &Account, active: bool, now: Timestamp) -> bool {
    // One clock, whatever the last attempt produced. A provider that will not
    // answer — offline, rate-limited, or an organisation that forbids the call
    // — must not be asked again on the very next poll, or a handful of such
    // accounts would spend the whole budget re-asking a question that keeps
    // failing.
    let asked_recently = account
        .entitlement_checked_at
        .is_some_and(|at| now.as_second() - at.as_second() < ENTITLEMENT_MAX_AGE.as_secs() as i64);
    if asked_recently {
        return false;
    }
    // Never asked: worth one request whoever it is, because without the quota
    // size the ranking compares percentages blind. After that, only the account
    // in use is worth re-asking.
    account.identity.plan.is_none() || active
}

/// Whether a credential expires within `leeway`.
pub(crate) fn expires_within(credential: &Credential, leeway: SignedDuration) -> bool {
    credential
        .expires_at
        .is_some_and(|at| at - leeway <= Timestamp::now())
}

/// When to retry after a failed poll.
fn backoff_until(error: &http::Error, previous_failures: u32) -> Option<Timestamp> {
    let seconds = match error {
        // Respect the provider's own instruction, with a margin: these budgets
        // are measured per hour, so coming back too early just spends another
        // request on another refusal.
        http::Error::RateLimited { retry_after } => {
            retry_after.map_or(900, |d| d.as_secs().max(60) + 300).min(3600)
        }
        // A rejected credential will not fix itself; wait for the next manual
        // action rather than hammering.
        http::Error::Unauthorized { .. } => 1800,
        _ => (30u64 << previous_failures.min(5)).min(600),
    };
    Timestamp::now()
        .checked_add(SignedDuration::from_secs(seconds as i64))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Identity;

    fn engine() -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        (dir, Engine::with_store(store).unwrap())
    }

    fn captured(email: &str, refresh: &str) -> Captured {
        Captured {
            identity: Identity {
                email: Some(email.into()),
                ..Default::default()
            },
            credential: Credential {
                access_token: "access".into(),
                refresh_token: refresh.into(),
                id_token: None,
                expires_at: None,
                refresh_expires_at: None,
            },
            provider_data: Map::new(),
        }
    }

    #[test]
    fn adding_the_same_account_twice_updates_it_in_place() {
        let (_tmp, engine) = engine();
        let first = engine
            .store_captured(ProviderKind::Claude, captured("a@x.com", "r1"), None)
            .unwrap();
        assert_eq!(
            first,
            AddOutcome::Added {
                id: "claude-1".into()
            }
        );

        // Same email, rotated token: the same account signing in again.
        let second = engine
            .store_captured(
                ProviderKind::Claude,
                captured("a@x.com", "r2"),
                Some("work".into()),
            )
            .unwrap();
        assert_eq!(
            second,
            AddOutcome::Updated {
                id: "claude-1".into()
            }
        );

        let accounts = engine.store().accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].credential.refresh_token, "r2");
        assert_eq!(accounts[0].label.as_deref(), Some("work"));
    }

    #[test]
    fn different_accounts_get_separate_records() {
        let (_tmp, engine) = engine();
        engine
            .store_captured(ProviderKind::Claude, captured("a@x.com", "r1"), None)
            .unwrap();
        let second = engine
            .store_captured(ProviderKind::Claude, captured("b@x.com", "r2"), None)
            .unwrap();
        assert_eq!(
            second,
            AddOutcome::Added {
                id: "claude-2".into()
            }
        );
        assert_eq!(engine.store().accounts().unwrap().len(), 2);
    }

    #[test]
    fn accounts_resolve_by_id_email_or_label() {
        let (_tmp, engine) = engine();
        engine
            .store_captured(
                ProviderKind::Claude,
                captured("a@x.com", "r1"),
                Some("personal".into()),
            )
            .unwrap();
        assert_eq!(engine.resolve("claude-1").unwrap().id, "claude-1");
        assert_eq!(engine.resolve("A@X.com").unwrap().id, "claude-1");
        assert_eq!(engine.resolve("personal").unwrap().id, "claude-1");
        let err = engine.resolve("nope").unwrap_err().to_string();
        assert!(err.contains("agent-meter list"), "{err}");
    }

    /// The plan and quota size are re-asked on a clock measured in hours, and
    /// only for the account in use. Every request here competes with the usage
    /// polls that switching actually runs on.
    /// The property, not the symptom: **after a renewal, the account must not
    /// be due for another one.**
    ///
    /// Anything that renews on a timer and cannot say that burns a single-use
    /// refresh token on every poll, and two rotations colliding ends in
    /// `invalid_grant`, which no retry undoes. Asserted here for every provider
    /// and for a response that states no lifetime at all, because carrying the
    /// spent token's expiry forward is what turns one renewal into a loop.
    #[test]
    fn a_renewed_credential_is_never_immediately_due_again() {
        let spent = Credential {
            access_token: "old".into(),
            refresh_token: "old-refresh".into(),
            id_token: None,
            // Long expired, which is why a renewal happened at all.
            expires_at: Some(Timestamp::now() - SignedDuration::from_hours(9)),
            refresh_expires_at: Some(Timestamp::now() + SignedDuration::from_hours(48)),
        };
        assert!(expires_within(&spent, REFRESH_LEEWAY), "the credential was due");

        let responses = [
            // What the provider normally sends.
            serde_json::json!({"access_token": "new", "refresh_token": "fresh", "expires_in": 28800}),
            // And what it must survive sending: no lifetime at all.
            serde_json::json!({"access_token": "new", "refresh_token": "fresh"}),
            // A rotation-free renewal, which keeps its own refresh clock.
            serde_json::json!({"access_token": "new", "expires_in": 28800}),
        ];
        for response in responses {
            let renewed = crate::provider::claude::parse_token_response(&response, &spent).unwrap();
            assert!(
                !expires_within(&renewed, REFRESH_LEEWAY),
                "renewing again immediately burns a single-use token every poll: {response}"
            );
            assert_ne!(
                renewed.expires_at, spent.expires_at,
                "the spent token's clock must not be carried forward: {response}"
            );
        }
    }

    /// One rate for every provider was Anthropic's rate applied to a provider
    /// that never asked for it. Each states its own now, and the configured
    /// cycle can only slow them down, never speed one past what it allows.
    #[test]
    fn each_provider_is_polled_at_its_own_rate() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        let mut engine = Engine::with_store(store).unwrap();

        // The default cycle is a minute: Codex takes it, Claude does not.
        engine.override_watch(None, Some(60)).unwrap();
        assert_eq!(engine.poll_interval(ProviderKind::Codex).as_secs(), 60);
        assert_eq!(
            engine.poll_interval(ProviderKind::Claude).as_secs(),
            300,
            "Anthropic's ceiling is not something a faster cycle can opt out of"
        );

        // A slower cycle slows everyone, because nothing is read between wakes.
        engine.override_watch(None, Some(900)).unwrap();
        for kind in ProviderKind::ALL {
            assert_eq!(engine.poll_interval(kind).as_secs(), 900, "{kind}");
        }
    }

    #[test]
    fn only_the_account_in_use_re_asks_its_entitlement_and_only_when_it_is_old() {
        let (_tmp, engine) = engine();
        engine
            .store_captured(ProviderKind::Claude, captured("a@x.com", "r1"), None)
            .unwrap();
        let mut account = engine.store().account("claude-1").unwrap().unwrap();
        account.identity.plan = Some("team".into());

        let now = Timestamp::now();
        let checked = |hours: i64| Some(now - jiff::SignedDuration::from_hours(hours));

        account.entitlement_checked_at = checked(1);
        assert!(!entitlement_due(&account, true, now), "asked an hour ago");
        assert!(!entitlement_due(&account, false, now), "not the account in use");

        account.entitlement_checked_at = checked(7);
        assert!(
            entitlement_due(&account, true, now),
            "in use, and the answer is old"
        );
        assert!(
            !entitlement_due(&account, false, now),
            "an idle account's plan is not worth a request"
        );

        // An account that was never told its plan asks once either way: without
        // the quota size the ranking compares percentages blind.
        account.identity.plan = None;
        account.entitlement_checked_at = None;
        assert!(entitlement_due(&account, false, now));

        // But only once. A provider that will not answer — rate-limiting, or an
        // organisation that forbids the call — would otherwise be asked again
        // every poll, and a handful of such accounts would spend the whole
        // budget on it.
        account.entitlement_checked_at = checked(1);
        assert!(
            !entitlement_due(&account, false, now),
            "a failed attempt still puts the question on the slow clock"
        );
    }

    #[test]
    fn stale_login_directories_are_swept_but_fresh_ones_are_kept() {
        let (_tmp, engine) = engine();
        let logins = engine.store().dir().join("logins");
        std::fs::create_dir_all(&logins).unwrap();

        // A login killed part-way through can leave a real credential behind.
        let abandoned = logins.join("claude-1-abandoned");
        std::fs::create_dir(&abandoned).unwrap();
        std::fs::write(abandoned.join(".credentials.json"), b"{}").unwrap();

        // A login starting now must not sweep away one that started a moment ago.
        let fresh = engine.login_home(ProviderKind::Claude).unwrap();
        assert!(fresh.exists());
        assert!(
            abandoned.exists(),
            "an hour-old cutoff must spare a directory created just now"
        );

        // With no grace period, every leftover goes.
        engine.sweep_login_homes(std::time::Duration::ZERO);
        assert!(
            !abandoned.exists(),
            "an abandoned login directory must be removed"
        );
        assert!(!fresh.exists());
    }

    #[test]
    fn backoff_grows_and_respects_retry_after() {
        let now = Timestamp::now();
        let seconds =
            |e: &http::Error, failures| backoff_until(e, failures).unwrap().as_second() - now.as_second();

        let transient = http::Error::Status {
            status: 503,
            url: String::new(),
            body: String::new(),
        };
        let first = seconds(&transient, 0);
        let later = seconds(&transient, 3);
        assert!(later > first, "{later} should exceed {first}");
        assert!(seconds(&transient, 20) <= 600);

        let limited = http::Error::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(60)),
        };
        assert!(seconds(&limited, 0) >= 360);

        let denied = http::Error::Unauthorized {
            message: "no".into(),
            code: None,
        };
        assert!(seconds(&denied, 0) >= 1800);
    }
}
