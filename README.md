# agent-meter in herdr: proposal assets

Screenshots and throwaway prototypes behind the herdr proposal (see the issue
that links here). Nothing on this branch is built or shipped.

- `herdr-app/`: the Herdr desktop app (herdr-app 0.1.x on herdr-core 0.1.21).
  `herdr-app shot` frames of a sandboxed app and core (their own
  `HERDR_APP_HOME`, `HC_SESSION` and XDG paths) running the prototype mod in
  `herdr-app/mod/` against a demo agent-meter store: made-up accounts, real
  readings, offline. The agents are terminals that report their status with
  OSC 7501 rather than real Claude Code or Codex sessions.
- `herdr/`: the earlier take for the terminal UI (herdr 0.9), with its plugin
  prototype in `herdr/prototype/`.
