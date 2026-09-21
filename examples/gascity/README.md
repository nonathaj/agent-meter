# Gas City account-affinity pilot

This integration gives each **conversation** a fixed subscription account for
both Claude Code and Codex. It uses Gas City's existing provider overrides,
`GT_ROOT` city namespace and durable `GC_SESSION_ID`. It works at the CLI launch
boundary used by the federation's `herdr` runtime; it is not an ACP adapter.

## Prepare account homes

Build agent-meter and put `agent-meter-claude` and `agent-meter-codex` from this
directory on `PATH`. Keep the real `claude` and `codex` commands on `PATH` too.
`AGENT_METER_BIN` can point the wrappers at a particular agent-meter binary.

A fleet home must already contain a native subscription login. Use a dedicated,
persistent directory for each account. Log in there with the native CLI, for
example (replace these paths):

```sh
CLAUDE_CONFIG_DIR=/absolute/claude-home claude auth login
CODEX_HOME=/absolute/codex-home codex -c 'cli_auth_credentials_store="file"' login
```

These commands are interactive; complete the provider's login in the browser.
Use independent logins, not copies of the global CLI's refresh credentials.
Stop other tools from refreshing the credentials used by these homes. In
particular, don't hand the new fleet credentials back to cswap. Existing global
sessions can continue using their existing homes and login credentials.

Import the native homes into agent-meter, then register them from a shell whose
`CLAUDE_CONFIG_DIR` / `CODEX_HOME` do **not** point at those homes:

```sh
CLAUDE_CONFIG_DIR=/absolute/claude-home agent-meter import claude --label city-claude
CODEX_HOME=/absolute/codex-home agent-meter import codex --label city-codex
agent-meter fleet register city-claude --home /absolute/claude-home
agent-meter fleet register city-codex --home /absolute/codex-home
agent-meter fleet list
```

Do not run a watcher between importing and registering a new home. Registration
adopts its current credentials; after registration only the native CLI refreshes
them. Agent-meter polls usage using credentials captured from that home, and
never exchanges its refresh token. When an idle home's token expires, metering
can be stale until the native CLI runs again. That is preferable to a competing
refresh writer signing out a running session.

Moving config homes also moves user settings, plugins, hooks and transcripts.
Provision the settings and tools needed by the pilot in each new home. Do not
symlink entire account homes together or copy authentication files from the
global home. Codex pinned launches explicitly select file credential storage.

## Wire a small pilot

Adapt `providers.toml` to use the registered account **ids**, canonical absolute
home paths, and wrapper locations. Match the federation's explicit model and
effort settings rather than inheriting a new account's defaults. Merge its
`daemon.observe_paths` entries with any existing roots. Add this as a city config
include, and assign only new pilot agents to `claude-pinned` or `codex-pinned`.
Do not change the default provider for existing sessions. Keep the city's
existing concurrency ceiling.

The provider's environment must include the matching native home. This makes the
home visible to the runtime before the wrapper runs, including herdr's metadata
sidecar. Each wrapper checks it against the persistent assignment through
`--expect-home`. Merely exporting a different home inside a wrapper is not enough
for runtime discovery. The observation roots cover transcript discovery for both
providers. Codex has an explicit wrapped `resume_command`; otherwise a resume
could bypass account selection.

Before launching, validate with `gc config show` and inspect the plan:

```sh
agent-meter run --provider claude --account city-claude \
  --scope /absolute/city --session pilot-conversation --dry-run
```

The JSON contains only account ids, home paths and environment metadata, never
credentials. `--dry-run` does not create a session binding. A real launch records
one before executing the native binary. On Unix, exec preserves PID, terminal,
signals, and native exit status. On Windows, agent-meter waits for the child.

Restart/resume keeps the same `GC_SESSION_ID`, so it selects the same home. The
native `--resume` / `resume` arguments still identify the provider conversation;
agent-meter does not guess or rewrite them. Changing the configured account for
an existing binding fails rather than migrating its conversation. New session
ids may deliberately use another account. Record keeping survives process death.

## Pilot acceptance and measurements

1. Start one new Claude conversation and one new Codex conversation. Confirm
   account identity in the native CLI and compare `agent-meter fleet list` with
   the city's session identities.
2. Restart/resume each, verify its conversation history and home are unchanged,
   and confirm the city discovers its new transcript. Exercise a tool call and
   the usual city hooks to catch missing settings or plugins.
3. Change the global login with the existing tool, and verify the two pilot
   conversations keep their own accounts. Do not change a registered home.
4. Observe at least one native credential refresh. `agent-meter list --refresh
   --json` must adopt it without writing to the native home or requesting a new
   login. Check metering on both providers.
5. Compare cache-read tokens, uncached/cache-write tokens, turn latency, completed
   work and subscription-window consumption over comparable work. The city's
   `.gc/usage.jsonl` and native transcripts supply token facts; `gc costs` is an
   API list-price estimate, **not** subscription billing. Deduplicate Claude
   streaming entries and use Codex cumulative counter deltas if analyzing raw
   transcripts. Record model changes, compaction and idle gaps as confounders.

No cache savings are assumed from account affinity alone. The API cache docs do
not establish the subscription login cache boundary.

## Boundaries of this first milestone

This is explicit account assignment, not an automatic allocator. The caller must
name an account for a new session; resumes may omit it. It does not reserve quota,
count live leases, prevent duplicate native launches of the same conversation,
or migrate an exhausted session. The city still owns concurrency and lifecycle.

Fleet homes and bindings are local-only and stored in `fleet.json`. Global
`use`/`watch` excludes registered accounts; export/sync does not distribute their
credentials. Registered accounts cannot be removed by the normal remove command.
There is deliberately no live rebind, unregister or automatic affinity garbage
collection in this milestone: retain the state for every resumable conversation.
Back up `fleet.json` with the account store. Corrupt state fails closed.

Roll back the pilot by stopping its sessions and removing only the pilot provider
selection/include. Leave its homes and affinity state in place for later resume;
ordinary global sessions do not need to restart.

After live acceptance, add allocation as a separate layer: atomic capacity
reservations, account and requested-model quota checks, weighted placement of
**new** sessions, drain thresholds, crash lease recovery, and an explicit
wait-versus-migrate policy. Existing bindings remain authoritative.
