---
generated_at_commit: df36715
generated_at_date: 2026-06-17
covers_paths: [exarch/src/shell_eval.rs, exarch/src/sandbox_diag.rs, exarch/src/sandbox_diag/, exarch/src/agent_builtins.rs, exarch/data/agent.ral]
---

# Map: exarch / shell eval

`shell_eval.rs` runs one tool call as a ral top-level turn against the
persistent [[map/core/shell-state|`Shell`]]. `run_shell`:

- compiles the model's source through `compile_and_typecheck` seeded from the
  live session (`shell.session_schemes()`, the one name→scheme seed —
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]); the
  prelude's schemes ride scope[0], installed when the annotated prelude was
  evaluated at boot. The check is strict — any type error is fatal — over the
  single mode-inference engine `compile_and_typecheck` every evaluated path shares
  ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]).
  Parse/type errors return as pre-rendered ariadne text (`Outcome::Static`); on
  success the *annotated* comp is what runs;
- pushes the session `Capabilities` with `Shell::with_capabilities` and runs
  `evaluator::eval_top_level`. **This is the sandbox**: the boundary is the
  pushed [[design/grant|grant]] frame plus the [[map/core/evaluator|top-level
  contract]], not a source-level `grant { … }` the model could escape. External
  commands route through the same OS sandbox as ral
  ([[map/core/capabilities|capabilities]]). The post-run `Mobile` installs onto the shell, so
  `let`, `cd`, and env persist across tool calls (the in-module tests pin this);
- captures stdout and stderr into in-memory buffers — the full, model-visible
  and logged text. Nothing echoes live; the [[map/exarch/frontend|rail]] surfaces
  tool summaries, patches, writes, and tasks instead, and the
  [[map/exarch/session|digest]] caps shape only the model's history view;
- arms a wall-clock watchdog over a child `CancelScope`; only
  `CancelCause::Deadline` maps to timeout exit 124, while Esc remains an
  interrupt. A grant body evaluates locally — there is no sandbox-IPC round trip
  to interrupt — so cancellation reaches any spawned child through the ordinary
  process group / cancel-scope path. `audit` still forces a fresh audit subtree
  when requested.

The whole per-call IO frame — the stdout/stderr/stdin tees, the `SurfaceSink`,
the script-context location triple, and the watchdog `CancelScope` — is installed
through an `IoGuard` whose `Drop` restores every field, so the frame self-heals
on a caught worker panic as well as on the normal return
([[decisions/260612_exarch-panic-recovery|panic-recovery]]). The dynamic-context
half of that contract lives in [[map/exarch/session|session]].

**Surface host sink.** `run_shell` installs a
[[map/core/shell-state|`SurfaceSink`]] on `shell.local.surface` for the extent of
the turn, decoding kit output into rail events in three steps:

- a ral kit hands a tagged variant to the core `surface` builtin;
- the sink runs `value_to_kind` to decode it into a typed rail `Kind` (`Patch` /
  `Wrote` / `Task` / `Meter`);
- it emits the `Kind` on the [[map/exarch/frontend|bus]].

The producer is a direct `surface` call at each kit site, with no cross-language
sentinel constant. Across the OS-sandbox boundary the events are buffered in the
confined child and replayed through the parent's sink
([[map/core/capabilities|carried on the IPC response]]), so they are batched
rather than live under the sandbox.

`agent_builtins.rs` registers exarch's resident host atoms (line witnesses, grep
helpers) and sources the embedded `data/agent.ral` helper library into the shell
at boot ([[map/exarch/builtins|builtins]]). The Rust atoms — but not the sourced
library — also dress the [[map/core/capabilities|sandbox-IPC child]]'s fresh
shell, installed through `set_child_shell_extension`.

`sandbox_diag.rs` harvests kernel-reported sandbox denials (Seatbelt on macOS,
the seccomp filter inside bwrap on Linux) over a failed call's wall window,
keeping only lines attributed to the call's descendant PID tree
(`DescendantTracker`), and appends them to stderr. No-op when the policy engages
no OS sandbox.
