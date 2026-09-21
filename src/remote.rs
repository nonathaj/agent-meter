//! Keeping another machine's accounts in step with this one, over ssh.
//!
//! The two machines talk rather than copy files. This one runs `agent-meter
//! sync --serve` on the other, hands it accounts on standard input and reads
//! its answer back: the other machine merges what it is sent under its own
//! lock and by its own rules, and it alone decides what its files look like.
//! Copying a store across would mean reaching around its locks and pinning
//! this version's file layout to that version's.
//!
//! What makes this worth doing carefully is that an OAuth refresh token is
//! single-use. Two machines holding one account hold two copies of it, and
//! the moment either refreshes, the other's copy is spent. So neither side
//! ever takes a credential simply because it arrived: both apply the same
//! rule, that of two copies the one whose access token expires later is the
//! one refreshed most recently, and the other is the dead one.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::account::Credential;
use crate::account::{Identity, ProviderKind};
use crate::config::RemoteConfig;
use crate::engine::Engine;

/// Version of the conversation, so two machines on different releases say so
/// rather than misreading each other.
pub const PROTOCOL: u32 = 1;

/// The most we will read from the other side before deciding it is not an
/// agent-meter. Generous for any believable number of accounts.
const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

/// How long to wait for a machine that may be asleep, when nobody asked for
/// this and nobody is watching.
const UNATTENDED_TIMEOUT_SECS: u32 = 10;

/// One account, as the other machine receives it.
///
/// Deliberately not the stored record. The id (`claude-2`) counts up per store
/// and means a different account on each machine, so it travels only to be
/// named in the report. `needs_login` does not travel at all: it is one
/// machine's account of what a provider told it, and a machine that has been
/// handed a newer credential has no reason to inherit the verdict on an older
/// one.
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub provider: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub identity: Identity,
    pub credential: Credential,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub provider_data: Map<String, Value>,
    pub added_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entitlement_checked_at: Option<Timestamp>,
    /// What this account is called where it came from, so a report can name it.
    pub origin_id: String,
}

/// Never prints a token, in a log, an error or a panic.
impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("origin_id", &self.origin_id)
            .field("provider", &self.provider)
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

/// What one machine asks another to do.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub protocol: u32,
    /// Accounts to merge into the receiver's store.
    #[serde(default)]
    pub accounts: Vec<Record>,
    /// Whether to send back what the receiver holds.
    #[serde(default)]
    pub want: bool,
    /// False asks what would happen without anything being written.
    #[serde(default)]
    pub apply: bool,
}

/// What it did, and what it holds.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub protocol: u32,
    /// The receiver's version, so a mismatch can be described rather than
    /// guessed at.
    pub version: String,
    pub applied: Report,
    #[serde(default)]
    pub accounts: Vec<Record>,
}

/// What merging a set of accounts did, or would do.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Report {
    /// Accounts the receiver did not have.
    pub added: Vec<String>,
    /// Accounts whose credential the receiver took, because ours was newer.
    pub updated: Vec<String>,
    /// Accounts where what the receiver already had was the same or newer.
    pub unchanged: usize,
    /// Accounts not sent or not taken, and why.
    #[serde(default)]
    pub skipped: Vec<String>,
}

impl Report {
    pub fn writes(&self) -> usize {
        self.added.len() + self.updated.len()
    }
}

/// Everything one exchange did, in both directions.
#[derive(Debug)]
pub struct Exchange {
    /// What the other machine did with what we sent it.
    pub sent: Report,
    /// What we did with what it sent back.
    pub received: Report,
    /// How many accounts we offered.
    pub offered: usize,
    pub version: String,
}

/// Which way accounts move, and whether anything is written.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub push: bool,
    pub pull: bool,
    pub apply: bool,
    pub provider: Option<ProviderKind>,
    /// True when nobody is at the keyboard: fail rather than ask ssh for a
    /// password, and do not wait long for a machine that may be switched off.
    pub unattended: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            push: true,
            pull: true,
            apply: false,
            provider: None,
            unattended: false,
        }
    }
}

/// Runs one exchange with `remote`.
pub fn exchange(engine: &Engine, remote: &RemoteConfig, options: Options) -> Result<Exchange> {
    let mut offered = if options.push {
        engine.records()?
    } else {
        Vec::new()
    };
    if let Some(provider) = options.provider {
        offered.retain(|record| record.provider == provider);
    }

    let request = Request {
        protocol: PROTOCOL,
        want: options.pull,
        apply: options.apply,
        accounts: offered.clone(),
    };
    let response = talk(remote, &request, options.unattended)?;

    if response.protocol != PROTOCOL {
        bail!(
            "the agent-meter on that machine speaks version {} of this conversation and this one \
             speaks {PROTOCOL}. Upgrade whichever is older.",
            response.protocol
        );
    }

    let mut incoming = response.accounts;
    if let Some(provider) = options.provider {
        incoming.retain(|record| record.provider == provider);
    }
    let received = engine.absorb(&incoming, options.apply)?;

    Ok(Exchange {
        sent: response.applied,
        received,
        offered: offered.len(),
        version: response.version,
    })
}

/// Starts agent-meter on the other machine and holds one conversation with it.
fn talk(remote: &RemoteConfig, request: &Request, unattended: bool) -> Result<Response> {
    let mut command = transport(remote, unattended)?;
    let described = describe(&command);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {described}"))?;

    // On its own line, and stdin closed after it, so the other side knows the
    // question is finished without having to count bytes.
    let mut message = serde_json::to_vec(request).context("preparing what to send")?;
    message.push(b'\n');
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(&message)
        .with_context(|| format!("sending to {described}"))?;

    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for {described}"))?;
    if !output.status.success() {
        let complaint = String::from_utf8_lossy(&output.stderr);
        let complaint = complaint.trim();
        bail!(
            "{described} failed{}{}",
            if complaint.is_empty() { "" } else { ": " },
            complaint
        );
    }

    parse_reply(&output.stdout).with_context(|| {
        format!(
            "{described} answered with something that is not an agent-meter's reply. Check that              agent-meter is installed there and is new enough to know `sync --serve`."
        )
    })
}

/// Finds the reply in what the other machine said.
///
/// Logging in to a machine can print things of its own — a message of the day,
/// a shell greeting — and some of that arrives on standard output ahead of the
/// answer. The reply is one line of JSON, so it is looked for from the end
/// rather than assumed to be the whole of what came back.
fn parse_reply(stdout: &[u8]) -> Result<Response> {
    let text = String::from_utf8_lossy(stdout);
    let mut first_complaint = None;
    for line in text.lines().rev().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<Response>(line) {
            Ok(response) => return Ok(response),
            Err(error) => first_complaint = first_complaint.or(Some(error)),
        }
    }
    match first_complaint {
        Some(error) => Err(error).context("reading the reply"),
        None => bail!("it said nothing at all"),
    }
}

/// How to start agent-meter on the other machine.
fn transport(remote: &RemoteConfig, unattended: bool) -> Result<Command> {
    if let Some((program, arguments)) = remote.exec.split_first() {
        let mut command = Command::new(program);
        command.args(arguments);
        command.envs(&remote.env);
        return Ok(command);
    }
    let Some(destination) = &remote.ssh else {
        bail!("this remote states neither ssh nor exec, so there is no way to reach it");
    };

    let mut command = Command::new("ssh");
    if unattended {
        // Nobody is here to type a passphrase, and a machine that is asleep
        // should be reported rather than waited on.
        command.args(["-o", "BatchMode=yes"]);
        command.args(["-o", &format!("ConnectTimeout={UNATTENDED_TIMEOUT_SECS}")]);
    }
    command.arg(destination);
    command.arg(remote.command.as_deref().unwrap_or("agent-meter"));
    command.args(["sync", "--serve"]);
    Ok(command)
}

/// The command as a person would write it, for an error message.
fn describe(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().into_owned()];
    parts.extend(command.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

/// The other side of the conversation: reads one request, answers it.
///
/// Nothing else may be written to `output` while this runs — it is the whole
/// reply, and a stray line of ours would make it unreadable.
pub fn serve(engine: &Engine, input: impl Read, mut output: impl Write) -> Result<()> {
    let mut line = String::new();
    BufReader::new(input.take(MAX_MESSAGE_BYTES))
        .read_line(&mut line)
        .context("reading the request")?;
    if line.trim().is_empty() {
        bail!("no request arrived on standard input");
    }
    let request: Request = serde_json::from_str(&line).context("reading the request")?;
    if request.protocol != PROTOCOL {
        bail!(
            "that machine speaks version {} of this conversation and this one speaks {PROTOCOL}",
            request.protocol
        );
    }

    let applied = engine.absorb(&request.accounts, request.apply)?;
    let accounts = if request.want {
        engine.records()?
    } else {
        Vec::new()
    };

    let response = Response {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_string(),
        applied,
        accounts,
    };
    let mut message = serde_json::to_vec(&response).context("preparing the reply")?;
    message.push(b'\n');
    output.write_all(&message).context("sending the reply")?;
    output.flush().context("sending the reply")
}

/// The remotes a name refers to.
///
/// A name out of the configuration, or an ssh destination given directly, so
/// a machine can be synced with once without being written down first.
pub fn resolve(
    configured: &BTreeMap<String, RemoteConfig>,
    names: &[String],
) -> Result<Vec<(String, RemoteConfig)>> {
    if names.is_empty() {
        if configured.is_empty() {
            bail!(
                "no other machines are configured. Add one with `agent-meter remote add <name> \
                 <user@host>`, or name an ssh destination here."
            );
        }
        return Ok(configured.iter().map(|(n, r)| (n.clone(), r.clone())).collect());
    }

    let mut chosen = Vec::new();
    for name in names {
        match configured.get(name) {
            Some(remote) => chosen.push((name.clone(), remote.clone())),
            // Not a name we know, so it is a destination: `user@host`, or a
            // host out of the user's own ssh configuration.
            None => chosen.push((
                name.clone(),
                RemoteConfig {
                    ssh: Some(name.clone()),
                    ..RemoteConfig::default()
                },
            )),
        }
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(exec: &[&str]) -> RemoteConfig {
        RemoteConfig {
            exec: exec.iter().map(|s| (*s).to_string()).collect(),
            ..RemoteConfig::default()
        }
    }

    #[test]
    fn ssh_is_asked_to_run_agent_meter_serving() {
        let configured = RemoteConfig {
            ssh: Some("jon@laptop".into()),
            ..RemoteConfig::default()
        };
        assert_eq!(
            describe(&transport(&configured, false).unwrap()),
            "ssh jon@laptop agent-meter sync --serve"
        );

        // Unattended, it must fail rather than sit waiting for a passphrase
        // or for a machine that is switched off.
        let unattended = describe(&transport(&configured, true).unwrap());
        assert!(unattended.contains("BatchMode=yes"), "{unattended}");
        assert!(unattended.contains("ConnectTimeout="), "{unattended}");

        // Somewhere agent-meter is not on the PATH.
        let elsewhere = RemoteConfig {
            command: Some("/opt/bin/agent-meter".into()),
            ..configured
        };
        assert_eq!(
            describe(&transport(&elsewhere, false).unwrap()),
            "ssh jon@laptop /opt/bin/agent-meter sync --serve"
        );
    }

    #[test]
    fn exec_replaces_ssh_entirely() {
        assert_eq!(
            describe(&transport(&remote(&["wsl", "agent-meter", "sync", "--serve"]), false).unwrap()),
            "wsl agent-meter sync --serve"
        );
        assert!(transport(&RemoteConfig::default(), false).is_err());
    }

    /// A name that is not configured is a destination, so a machine can be
    /// synced with once without being written down first.
    #[test]
    fn an_unknown_name_is_taken_as_an_ssh_destination() {
        let mut configured = BTreeMap::new();
        configured.insert("laptop".to_string(), remote(&["true"]));

        let chosen = resolve(&configured, &["laptop".into(), "jon@desktop".into()]).unwrap();
        assert_eq!(chosen[0].1.exec, ["true"]);
        assert_eq!(chosen[1].1.ssh.as_deref(), Some("jon@desktop"));

        // Naming none of them means all of them.
        assert_eq!(resolve(&configured, &[]).unwrap().len(), 1);
        assert!(resolve(&BTreeMap::new(), &[]).is_err());
    }

    /// Logging in to a machine can print a greeting of its own, and some of it
    /// arrives on standard output ahead of the answer.
    #[test]
    fn the_reply_is_found_under_whatever_the_login_printed() {
        let reply = serde_json::to_string(&Response {
            protocol: PROTOCOL,
            version: "9.9.9".into(),
            applied: Report::default(),
            accounts: Vec::new(),
        })
        .unwrap();

        let greeted = format!("Welcome to laptop.\nLast login: yesterday\n{reply}\n");
        assert_eq!(parse_reply(greeted.as_bytes()).unwrap().version, "9.9.9");
        assert_eq!(parse_reply(reply.as_bytes()).unwrap().version, "9.9.9");

        // Anything that holds no reply is an error to read, not a panic.
        assert!(parse_reply(b"").is_err());
        assert!(parse_reply(b"command not found: agent-meter\n").is_err());
    }

    /// Tokens must not reach a log, an error message or a panic.
    #[test]
    fn a_record_never_prints_its_tokens() {
        let record = Record {
            provider: ProviderKind::Claude,
            label: None,
            identity: Identity::default(),
            credential: Credential {
                access_token: "sk-ant-oat01-secret".into(),
                refresh_token: "sk-ant-ort01-secret".into(),
                id_token: None,
                expires_at: None,
                refresh_expires_at: None,
            },
            provider_data: Map::new(),
            added_at: Timestamp::UNIX_EPOCH,
            entitlement_checked_at: None,
            origin_id: "claude-1".into(),
        };
        let printed = format!("{record:?}");
        assert!(!printed.contains("secret"), "{printed}");
        assert!(printed.contains("claude-1"), "{printed}");
    }
}
