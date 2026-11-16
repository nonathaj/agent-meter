# agent-meter in herdr: proposal assets

Screenshots and the throwaway prototype behind the herdr plugin proposal
(see the issue that links here). Nothing on this branch is built or shipped.

- `*.png`: real herdr 0.9.2 frames, captured from a sandboxed herdr session
  running the prototype against a demo agent-meter store (made-up accounts,
  real readings). The agents in the panes are reported with
  `herdr pane report-agent` rather than run.
- `prototype/`: the plugin as PowerShell scripts. The proposal replaces them
  with `agent-meter herdr ...` subcommands.
