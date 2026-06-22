---
generated_at_commit: f91ec75
generated_at_date: 2026-06-19
covers_paths: [exarch/src/shell_eval.rs, exarch/src/sandbox_diag.rs, exarch/src/sandbox_diag/, exarch/src/agent_builtins.rs, exarch/data/agent.ral]
---

# Map: exarch / shell eval

`shell_eval.rs` runs one tool call as a ral top-level turn against the
persistent [[map/core/shell-state|`Shell`]]. `run_shell` is a *request
supplier*: it builds a `TurnRequest` and calls the one host entry,
`Shell::run_turn` ([[internals/a-turn-end-to-end|a turn, end to end]];
[[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). Core owns the turn
machinery — compile, frame install, capture, the wall — so `run_shell` is now
just the request it assembles and the flat `TurnReport` it renders:

- **source + `script_name: "<tool>"`.** Core's `compile_turn` runs
  `compile_and_typecheck` seeded from the live session (`shell.session_schemes()`,
  the one name→scheme seed —
  [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]); the
  prelude's schemes ride scope[0], installed when the annotated prelude was
  evaluated at boot. The check is strict — any type error is fatal — over the
  single mode-inference engine every evaluated path shares
  ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]).
  Parse/type errors come back as `TurnReport::Static`, which `run_shell`
  formats to ariadne text (`Outcome::Static`); on success the *annotated* comp
  runs;
- **`caps`** — the session `Capabilities`, pushed for the eval's dynamic extent.
  **This is the sandbox**: the boundary is the pushed [[design/grant|grant]]
  frame plus the [[map/core/evaluator|top-level contract]], not a source-level
  `grant { … }` the model could escape. External commands route through the same
  OS sandbox as ral ([[map/core/capabilities|capabilities]]). The post-run
  `Mobile` installs onto the shell, so `let`, `cd`, and env persist across tool
  calls (the in-module tests pin this);
- **`io: TurnIo::Capture`** — core mints the stdout/stderr buffers and returns
  them in `TurnReport::Ran`'s `captured`: the full, model-visible and logged
  text. Nothing echoes live; the [[map/exarch/frontend|rail]] surfaces tool
  summaries, patches, writes, and tasks instead, and the
  [[map/exarch/session|digest]] caps shape only the model's history view;
- **`turn_limit`** — the per-tool wall. Core arms it on the turn's foreground
  scope *before compiling*, so it bounds the whole turn; only
  `CancelCause::Deadline` reports `timed_out`, which `run_shell` turns into the
  timeout-124 message ("spawn it, let the turn return, `poll`/`await` later"),
  while Esc stays an interrupt. A grant body evaluates locally — no sandbox-IPC
  round trip to interrupt — so cancellation reaches any spawned child through the
  ordinary process-group / cancel-scope path;
- **`detached_limit`** — `DETACHED_WORKER_CEILING` (1 h), the lifetime ceiling
  for workers the turn detaches at the durable root; a backstop, no longer
  load-bearing for turn exit;
- **`surface`** — the `AgentSink` (below). `audit` still forces a fresh audit
  subtree when requested.

Completion is `run_turn` returning. A detached `spawn`ed worker — a server, a
watch — holds bounded deferred surface storage in core, never a clone of the bus
[[map/exarch/frontend|`Emitter`]], so it cannot keep the tool turn from ending
([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). Frame teardown —
restoring the prior `TurnState`, its byte sinks, and its surface — is core's
`TurnGuard`, which self-heals on a caught worker panic as well as on the normal
return ([[decisions/260612_exarch-panic-recovery|panic-recovery]]); exarch
installs no per-call `IoGuard` of its own. The dynamic-context half of the
contract lives in [[map/exarch/session|session]].

**Surface host sink.** `run_shell` passes `surface: Some(Arc::new(AgentSink(emit)))`
in the request; core installs it as the turn-local
[[map/core/shell-state|`SurfaceSink`]] for the extent of the turn — and only the
turn, since it is no longer stored on the persistent session. `AgentSink`
decodes kit output into rail events in three steps:

- a ral kit hands a `` `card `` render document — an ordered stack of Bertin
  marks — to the core `surface` builtin;
- the sink runs `value_to_card` (`exarch/src/card.rs`) to decode it into the
  closed `Card`/`Mark` model and emits one `Kind::Card` — or, for an `io`-keyed
  value core emits at a redirect/exec door, the sibling `value_to_io`/`io_card`
  produce a `Kind::Io { event, card }` ([[map/exarch/io-surface|io-surface]]);
- it emits the `Kind` on the [[map/exarch/frontend|bus]] through a clone of the
  call's `Emitter`, where one generic `render_card` interpreter binds the marks
  to visual variables.

The producer is a direct `surface` call at each kit site, with no cross-language
sentinel constant. Same-thread children inherit the sink; detached workers do
not — core buffers their `surface` calls and replays them once on `await`, so a
bus `Emitter` clone can never outlive the tool turn. Across the OS-sandbox
boundary the events are buffered in the confined child and replayed through the
parent's sink ([[map/core/capabilities|carried on the IPC response]]), so they
are batched rather than live under the sandbox.

`agent_builtins.rs` registers exarch's resident host atoms — the line/window
witnesses and the `grep-files` search and hash-addressed `edit` that moved below
the ral line ([[map/exarch/io-surface|io-surface]]) — and sources the now-smaller
embedded `data/agent.ral` helper library (`view-text`) into the shell at boot
([[map/exarch/builtins|builtins]]). The Rust atoms — but not the sourced
library — also dress the [[map/core/capabilities|sandbox-IPC child]]'s fresh
shell, installed through `set_child_shell_extension`.

`sandbox_diag.rs` harvests kernel-reported sandbox denials (Seatbelt on macOS,
the seccomp filter inside bwrap on Linux) over a failed call's wall window,
keeping only lines attributed to the call's descendant PID tree
(`DescendantTracker`), and appends them to stderr. No-op when the policy engages
no OS sandbox.
