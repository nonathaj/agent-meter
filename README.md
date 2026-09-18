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
Watching every 1m at a 90% threshold. Press Ctrl-C to stop.
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

# Or take everything another tool is already holding.
agent-meter import --from cswap     # claude-swap
agent-meter import --from gemctl    # the Gem project launcher / gemctl

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

### Coming from another tool

`agent-meter import --from cswap` and `--from gemctl` read those tools' own
stores — claude-swap's backup directory, and the account store the Gem project
launcher shares with `gemctl` — and take every account they hold. Add `--dir` if
either keeps its files somewhere unusual.

Nothing is written back, so the other tool keeps working and you can run both
while you decide. Importing twice is safe: an account already stored is updated
rather than added again, and that holds across tools too, so the same account
found in both ends up as one.

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
| `agent-meter import [provider]` | Stores the account a CLI is already signed in to, or everything another tool holds with `--from cswap` / `--from gemctl`. |
| `agent-meter use <account>` | Signs the agent CLI in to a stored account. |
| `agent-meter remove <account>` | Forgets an account. The account itself is untouched. |
| `agent-meter watch` | Polls usage and switches accounts as limits approach. `--once`, `--dry-run`. |
| `agent-meter tui` | The same operations in a full-screen interface. |
| `agent-meter config` | Reads and changes settings. |
| `agent-meter where` | Prints the data directory. |

### The terminal UI

`agent-meter tui` gives every account a block rather than a row, because the
worst of an account's limits is not the whole story — one at 5% of its five
hours and 98% of its week is nearly spent, and one the other way round is fine
within the hour. Every limit of every account is on screen at once, so two
accounts can be compared without selecting either.

```text
 agent-meter   10 accounts   switching at 90%
Claude Code (8)   in the order they will be taken
▌1 dev@example.com  [personal]  max 20x  ● in use
    5h            ████████████████████████░░░░   85%  resets 3h43m
    weekly        ██████████████████████░░░░░░   80%  resets 4d19h  (49% ahead of pace)
    weekly Fable  █████░░░░░░░░░░░░░░░░░░░░░░░   19%  resets 4d19h

 2 oncall@example.com  [Example Inc]  team 5x  next
    5h            ███░░░░░░░░░░░░░░░░░░░░░░░░░    9%  resets 43m
    weekly        ███████████████████████████░   97%  resets 1d22h
```

Accounts are grouped by harness, and `p` shows one harness at a time. **With
switching on, the list is the queue**: the account in use is first and says so,
the one that would be taken next says so too, and the rest follow in the order
they would actually be chosen — so the order on screen is the order that will
happen, not an arrangement of its own.

`(ahead of pace)` marks a weekly allowance being spent faster than the clock
that refills it: 60% of a week is unremarkable on day five and a warning on day
two. It is only ever said of a weekly window, because a five-hour one is a rate
that corrects itself.

Every operation is on a key: `enter` to switch, `r` to refresh, `p` to filter by
harness, `a` to add, `i` to import, `d` to remove, `w` to switch automatically,
`?` for the rest.

## How switching decides

On each check, for each provider:

1. If the live account is below the threshold (90% by default), nothing happens
   — except on Claude, where an expiring week can still move it; see below.
2. Otherwise the best other account below the threshold wins.
3. If **every** account is past the threshold, what happens depends on what a
   switch costs. This is the case that gets the most out of the subscriptions
   you are paying for.
4. If every account is completely spent, it says so and names the one that frees
   up first, rather than churning.

An account is compared on its *tightest* window, so one at 20% of its five-hour
limit but 98% of its weekly one counts as 98% full. But only the account's own
limits count: `weekly Opus` at 100% costs one model, while `weekly` at 100%
stops the account, so a spent per-model window never makes an account look
unusable.

### The five-hour window gates; the week ranks

The two limits are not the same kind of thing, and ranking on whichever happens
to be worse gets it wrong:

- A five-hour window is a **rate**. At 90% it costs a few hours of waiting, and
  it refills on its own.
- A weekly window is a **budget**. At 90% it costs days — and whatever is left
  in it when it resets is **thrown away**.

So the threshold keeps throttled accounts out, and what ranks the rest is when
they recover: **soonest weekly reset first**, then soonest five-hour reset, then
how much work each can still do, then the name. Spending the soonest-expiring
allowance first is earliest-deadline-first on a perishable resource — better or
neutral, never worse.

### What a switch costs decides how eager it is

Claude Code re-reads its credential between messages, so a switch interrupts
nothing. Codex reads its once at startup, so a switch means restarting whatever
you have open. `agent-meter` takes that from the provider itself rather than
naming them in the rules:

- **Claude** moves whenever another usable account's week expires sooner, without
  waiting for the threshold — that allowance is being wasted while it waits, and
  taking it costs nothing. The trigger is a strictly sooner weekly reset and
  nothing else, which is what stops it oscillating: a weekly reset is fixed for
  the life of its window, so whichever account wins stays the winner until its
  week actually turns over.
- **Codex** stays put once everything is past the threshold. Picking the least
  busy of several busy accounts is an optimisation, and paying for one by
  restarting your sessions is the wrong trade — unless staying means staying on
  an account that can do nothing at all, which is a restart you would have to
  take anyway.

### Quota size, not just percentage

A percentage says how full an account is, never how big it is — and two
accounts on the same plan can differ several-fold. Claude reports the size as a
multiplier, shown in the plan column: a `max 20x` seat holds four times what a
`team 5x` one does, so **40% left on the 20x seat is twice the work that 90%
left on the 5x seat is**.

Where the provider states the size of every account in play, `agent-meter`
compares how much work each can still do rather than the fraction it has left.
If the size of any one of them is unknown, it compares percentages instead — an
account is never ranked last over a fact the provider simply did not state.

The same is true of the plan name itself: a Team seat reports `has_claude_max`
exactly as a personal Max seat does, so the plan is read from the account's
organization type, and both the plan and the multiplier come from the provider
rather than from the copies in the local credential file, which drift.

## Settings

```console
agent-meter config show
agent-meter config set watch.threshold 85
agent-meter config set provider.codex.enabled false
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `watch.threshold` | `90` | Usage percent at which to look for a better account. |
| `watch.poll-secs` | `60` | Seconds between checks. Each provider is still polled no faster than it allows. |
| `watch.margin` | `5` | Extra headroom needed to switch when every account is busy. |
| `watch.cooldown-secs` | `300` | Minimum gap between automatic switches. |
| `provider.<name>.threshold` | — | Per-provider threshold. |
| `provider.<name>.enabled` | `true` | Whether `watch` manages this provider. |

### How often anything is actually read

`watch.poll-secs` is how often the watcher **wakes**, not how often an account is
asked. Each provider states the closest together its own endpoint may be polled,
and the slower of the two wins — so raising this slows everything down, while
lowering it cannot speed any provider past its own rate.

| | Polled | Why |
| --- | --- | --- |
| **Codex** | every minute | Its own CLI reads that endpoint about once a minute per running session, so this is traffic your machine already makes. |
| **Claude** | every 5 minutes | Anthropic tolerates roughly 30 requests an hour per account, and going past it does not cost one skipped reading — the endpoint stays saturated for about an hour, which is an hour with nothing to switch on. |
| a new provider | the slow rate | Guessing slow costs a stale figure; guessing fast can cost that hour. |

Plan and quota size are on a slower clock again: they change when you change plan
or move organisation, not as you work, so they are re-read every six hours and
only for the account actually in use.

A reading's age is only ever shown when it is a surprise — older than the
interval that should already have replaced it, which usually means no watcher is
running. Saying "just now" on every row would spend a column on the ordinary
case.

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
