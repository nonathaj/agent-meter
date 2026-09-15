# agent-meter

Manage several coding-agent accounts from one place, and stop losing an
afternoon to a rate limit you did not see coming.

`agent-meter` stores the accounts your agent CLIs sign in to, shows how much of
each subscription is left, and — if you let it — moves you onto a fresher
account as a limit approaches.

```console
$ agent-meter list
     ID         PROVIDER      ACCOUNT              ORGANIZATION  PLAN     USED  WINDOWS                        RESETS IN
 ──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 *   claude-1   Claude Code   dev@example.com      Example Inc   team 5x  94%   5h 94%  weekly 61%  weekly Opus 4%  2h13m
     claude-2   Claude Code   dev@example.com      -             max 20x  12%   5h 12%  weekly 30%                  3h02m
 *   codex-1    Codex         dev@example.com      -             pro      8%    5h 0%  weekly 8%                    6d2h

$ agent-meter watch
Watching every 5m at a 90% threshold. Press Ctrl-C to stop.
Claude Code: switching to claude-2 (94% used; claude-2 is at 12%)
```

Supported agents: **Claude Code** and **OpenAI Codex CLI**, on Linux, macOS and
Windows.

## Why

Subscription plans meter you in rolling windows — five-hourly and weekly. If you
have more than one account (a personal plan and a work seat, say), the limits are
separate, but nothing tells you when one is nearly spent or makes it easy to move
to the other. `agent-meter` reads the same usage numbers the vendors show, keeps
your accounts side by side, and switches between them.

## Install

```console
cargo install agent-meter
```

Or build from source with `cargo build --release`; the binary lands in
`target/release/`.

## Getting started

```console
# Store the account your agent CLI is already signed in to.
agent-meter import

# Add another one. The login runs in a throwaway configuration directory,
# so an agent you have open right now keeps the credentials it is using.
agent-meter add claude

# See where you stand.
agent-meter list

# Move Claude Code onto another account.
agent-meter use claude-2

# Or let it happen on its own.
agent-meter watch
```

Accounts are named `claude-1`, `codex-2` and so on. Commands that take an
account also accept its email address or a label you gave it with `--label`.

One address can hold more than one account: a personal seat and a seat in a
team share an address, and on Claude a user id too — only the organisation
tells them apart, which is why it has a column of its own. They have separate
limits, so `agent-meter` keeps them as separate accounts, and naming the shared
address asks you which one you meant.

## Commands

| Command | What it does |
| --- | --- |
| `agent-meter list` | Every account with its usage. `--refresh` polls now, `--json` prints machine-readable output. |
| `agent-meter add <provider>` | Logs in to a new account without disturbing a running agent. |
| `agent-meter import [provider]` | Stores the account a CLI is already signed in to. |
| `agent-meter use <account>` | Signs the agent CLI in to a stored account. |
| `agent-meter remove <account>` | Forgets an account. The account itself is untouched. |
| `agent-meter watch` | Polls usage and switches accounts as limits approach. `--once`, `--dry-run`. |
| `agent-meter tui` | The same operations in a full-screen interface. |
| `agent-meter config` | Reads and changes settings. |
| `agent-meter where` | Prints the data directory. |

### The terminal UI

`agent-meter tui` shows the same information with live meters, and puts every
operation on a key: `enter` to switch, `r` to refresh, `a` to add, `i` to
import, `d` to remove, `w` to watch, `?` for the rest.

## How switching decides

On each check, for each provider:

1. If the live account is below the threshold (90% by default), nothing happens.
2. Otherwise the roomiest other account below the threshold wins.
3. If **every** account is past the threshold, it still moves — but only to an
   account with at least 5 points more headroom, so two busy accounts do not
   ping-pong. This is the case that gets the most out of the subscriptions you
   are paying for.
4. If every account is completely spent, it says so and names the one that frees
   up first, rather than churning.

An account is only ever compared on its *tightest* window: an account at 20% of
its five-hour limit but 98% of its weekly one is treated as 98% full, because
that is the limit you will hit.

### Quota size, not just percentage

A percentage says how full an account is, never how big it is — and two
accounts on the same plan can differ several-fold. Claude reports the size as a
multiplier, shown in the plan column: a `max 20x` seat holds four times what a
`team 5x` one does, so **40% left on the 20x seat is twice the work that 90%
left on the 5x seat is**.

Where the provider states the size of every account in play, `agent-meter` ranks
by how much work each can still do rather than by the fraction it has left. If
the size of any one of them is unknown, it compares percentages instead — an
account is never ranked last over a fact the provider simply did not state.

The same is true of the plan name itself: a Team seat reports `has_claude_max`
exactly as a personal Max seat does, so the plan is read from the account's
organization type, and both the plan and the multiplier come from the provider
rather than from the copies in the local credential file, which drift.

Claude Code re-reads its credential between messages, so a switch takes effect
in a session you already have open. Codex reads its credential once at startup,
so `agent-meter` tells you when a restart is needed.

## Settings

```console
agent-meter config show
agent-meter config set watch.threshold 85
agent-meter config set provider.codex.enabled false
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `watch.threshold` | `90` | Usage percent at which to look for a better account. |
| `watch.poll-secs` | `300` | Seconds between usage polls. |
| `watch.margin` | `5` | Extra headroom needed to switch when every account is busy. |
| `watch.cooldown-secs` | `300` | Minimum gap between automatic switches. |
| `provider.<name>.threshold` | — | Per-provider threshold. |
| `provider.<name>.enabled` | `true` | Whether `watch` manages this provider. |

Polling more often than every few minutes is counterproductive: the vendors rate
limit their usage endpoints to roughly 30 requests an hour per account.

## Where things are kept

| | |
| --- | --- |
| Linux | `~/.local/share/agent-meter` |
| macOS | `~/Library/Application Support/agent-meter` |
| Windows | `%LOCALAPPDATA%\agent-meter` |

Set `AGENT_METER_DIR` to put them somewhere else. Set `AGENT_METER_OFFLINE=1` to
stop `agent-meter` contacting the providers at all.

Account records hold OAuth tokens, and are written so that only your user
account can read them (mode `0600` on Unix). They are **not** encrypted: your
agent CLIs keep the same tokens in plain files next door, so encrypting here
would add a key to manage without raising the bar for anyone who can already
read your home directory. Treat the data directory the way you treat `~/.ssh`.

## How it works

`agent-meter` does not proxy your agent or hold a token on its behalf. It reads
and writes exactly the files the CLIs already use:

- **Claude Code** — `~/.claude/.credentials.json` (the macOS login Keychain,
  where that is where Claude Code put it) and the `oauthAccount` block of
  `~/.claude.json`. Writes are merges taken under Claude Code's own lock files,
  so a running session is never caught mid-swap, and machine-scoped secrets such
  as MCP tokens stay behind when the account changes.
- **Codex** — `~/.codex/auth.json`, merged so unrelated settings survive.

Usage comes from the vendors' own endpoints, the ones behind `/usage` in Claude
Code and `/status` in Codex.

Refresh tokens are single-use. If a CLI refreshes its own token, `agent-meter`
notices and adopts the new one rather than keeping a spent copy — otherwise the
stored account would be one failed refresh away from needing a manual login.

## Limitations

- API-key sign-ins are refused: usage is billed per token, so there is no quota
  to meter.
- Only subscription (OAuth) accounts are managed.
- `watch` runs in the foreground. Use your platform's service manager if you want
  it running all the time.

## Contributing

Bug reports and pull requests are welcome. Run `cargo test` and
`cargo clippy --all-targets` before sending a change; both are clean and are
expected to stay that way. Tests never touch the network or your real agent
configuration.

## Licence

Dual licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your
option.

`agent-meter` is an independent project. It is not affiliated with, endorsed by,
or supported by Anthropic or OpenAI.
