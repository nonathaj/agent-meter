//! The background worker that runs blocking operations for the TUI.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;

use anyhow::Result;

use crate::account::ProviderKind;
use crate::engine::Engine;
use crate::store::Store;

/// A unit of work the interface asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    /// Poll usage for every account.
    Poll { force: bool },
    /// Read usage for this one account now, whether or not it is due.
    Read(String),
    /// Sign the agent CLI in to this account.
    Switch(String),
    /// Forget this account.
    Remove(String),
    /// Store the account a CLI is already signed in to.
    Import(ProviderKind),
    /// Run one watcher tick, switching if the policy says so.
    Tick,
    /// Turn automatic switching on or off for one harness, and remember it.
    SetSwitching { provider: ProviderKind, on: bool },
}

impl Job {
    /// What to show while the job runs.
    pub fn describe(&self) -> String {
        match self {
            Job::Poll { .. } => "Reading usage…".into(),
            Job::Read(id) => format!("Reading usage for {id}…"),
            Job::Switch(id) => format!("Switching to {id}…"),
            Job::Remove(id) => format!("Removing {id}…"),
            Job::Import(provider) => format!("Importing from {}…", provider.display_name()),
            Job::Tick => "Checking whether to switch…".into(),
            Job::SetSwitching { provider, on } => format!(
                "Turning switching {} for {}…",
                if *on { "on" } else { "off" },
                provider.display_name()
            ),
        }
    }
}

/// The outcome of a job: a line to show the user.
pub type Outcome = Result<String, String>;

/// A worker thread. Dropping it lets the thread finish its current job and exit.
pub struct Worker {
    jobs: Option<Sender<Job>>,
    outcomes: Receiver<Outcome>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Worker {
    /// Starts a worker that operates on the same store as the interface.
    pub fn spawn(store: Store) -> Result<Self> {
        let (job_tx, job_rx) = channel::<Job>();
        let (outcome_tx, outcome_rx) = channel::<Outcome>();
        let handle = thread::Builder::new()
            .name("agent-meter-worker".into())
            .spawn(move || {
                // Each job re-opens the engine so a configuration change made
                // elsewhere takes effect without restarting the interface.
                while let Ok(job) = job_rx.recv() {
                    let outcome = Engine::with_store(store.clone())
                        .and_then(|engine| run_job(&engine, &job))
                        .map_err(|e| format!("{e:#}"));
                    if outcome_tx.send(outcome).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            jobs: Some(job_tx),
            outcomes: outcome_rx,
            handle: Some(handle),
        })
    }

    /// Queues a job. Returns false if the worker has stopped.
    pub fn submit(&self, job: Job) -> bool {
        self.jobs.as_ref().is_some_and(|jobs| jobs.send(job).is_ok())
    }

    /// Takes a finished job's outcome, if one is ready.
    pub fn poll(&self) -> Option<Outcome> {
        match self.outcomes.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("the background worker stopped unexpectedly".into())),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the channel ends the worker's loop once its current job is done.
        self.jobs = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_job(engine: &Engine, job: &Job) -> Result<String> {
    match job {
        Job::Poll { force } => {
            let results = engine.poll(&[], *force)?;
            let failed = results.iter().filter(|(_, r)| r.is_err()).count();
            match (results.len(), failed) {
                (0, _) => Ok("Usage is already up to date".into()),
                (total, 0) => Ok(format!("Read usage for {total} account(s)")),
                // Nothing worked, which is a failure however it is counted.
                (total, failed) if failed == total => {
                    anyhow::bail!("Could not read usage for any of {total} account(s)")
                }
                (total, failed) => Ok(format!("Read usage for {} of {total} account(s)", total - failed)),
            }
        }
        Job::Read(id) => match engine.poll(std::slice::from_ref(id), true)?.pop() {
            Some((_, Err(error))) => anyhow::bail!("Could not read usage for {id}: {error}"),
            _ => Ok(format!("Read usage for {id}")),
        },
        Job::Switch(id) => {
            let outcome = engine.switch_to(id)?;
            let mut note = match &outcome.from {
                Some(from) if from != &outcome.to => format!("Switched {from} -> {}", outcome.to),
                _ => format!("Now using {}", outcome.to),
            };
            if outcome.restart_required {
                note.push_str(" (restart running sessions to pick it up)");
            }
            Ok(note)
        }
        Job::Remove(id) => {
            engine.remove(id)?;
            Ok(format!("Removed {id}"))
        }
        Job::Import(provider) => {
            let outcome = engine.import(*provider, None)?;
            Ok(format!(
                "Imported {} from {}",
                outcome.id(),
                provider.display_name()
            ))
        }
        Job::SetSwitching { provider, on } => {
            // Written to the configuration rather than held for this run, so
            // the watcher and the interface cannot disagree about it.
            let mut config = engine.store().config()?;
            config.set(&format!("provider.{provider}.enabled"), &on.to_string())?;
            config.save(engine.store().dir())?;
            Ok(format!(
                "Switching {} for {}",
                if *on { "on" } else { "off" },
                provider.display_name()
            ))
        }
        Job::Tick => {
            let outcomes = engine.tick()?;
            let notes: Vec<_> = outcomes
                .iter()
                .filter_map(|o| {
                    o.switched
                        .as_ref()
                        .map(|s| format!("{}: now using {}", o.provider, s.to))
                })
                .collect();
            Ok(if notes.is_empty() {
                String::new()
            } else {
                notes.join("; ")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_jobs_and_reports_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).unwrap();
        let worker = Worker::spawn(store).unwrap();

        assert!(worker.submit(Job::Remove("claude-9".into())));
        let outcome = loop {
            if let Some(outcome) = worker.poll() {
                break outcome;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        // Removing an account that was never stored is not an error.
        assert_eq!(outcome.unwrap(), "Removed claude-9");
    }

    #[test]
    fn job_descriptions_name_the_account() {
        assert!(Job::Switch("codex-2".into()).describe().contains("codex-2"));
        assert!(
            Job::Import(ProviderKind::Claude)
                .describe()
                .contains("Claude Code")
        );
    }
}
