Yes, `ral` should probably be improved here.

“Modern” should not mean “fragile in multiplexers/recorders”. A modern shell should degrade cleanly across:

- plain tty
- tmux
- ssh
- asciinema
- CI
- dumb terminals

The problem is not that `ral` is advanced. The problem is that its interactive layer appears to assume a cleaner terminal than it actually has.

What to improve in `ral`:

- Detect non-ideal terminals and fall back aggressively.
  If running under `tmux`, `asciinema`, `TERM=dumb`, no true cursor support, or uncertain CPR behavior, use a simple prompt and no cursor-position queries.

- Make advanced line-editing features optional.
  Features that require terminal round-trips should be behind capability detection, not assumed.

- Add a “minimal prompt / dumb mode”.
  Something like:
  ```bash
  RAL_INTERACTIVE_MODE=minimal
  ```
  or automatic fallback when stdout/stderr/tty behavior looks odd.

- Separate shell semantics from shell UI.
  `ral` the language can stay rich; `ral` the prompt editor should be conservative unless the terminal proves it can support the fancy path.

- Handle CPR failure or garbage safely.
  If a cursor-position request returns unexpected bytes, time out, or leaks into input, discard it and downgrade the UI instead of printing raw escape sequences.

- Test under hostile-but-real setups.
  Add regression tests for:
  - tmux
  - tmux inside Docker
  - asciinema
  - ssh
  - non-interactive pty wrappers

Should it be improved?
- Yes, if you want `ral` to be taken seriously as an interactive shell.
- No, if you only care about language semantics and scripts. But then its interactive mode should explicitly admit it is experimental.

My view:
- `ral` should keep the modern semantics.
- Its interactive frontend should become more boring, not more clever.
- The right design is “fancy when safe, plain when uncertain.”

A good concrete target:
- default prompt path: plain and robust
- enhanced prompt path: opt-in or capability-detected
- zero leaked control sequences, ever

If you want, I can help sketch a specific design for `ral`’s terminal capability detection and fallback modes.

