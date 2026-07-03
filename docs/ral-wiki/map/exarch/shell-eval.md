---
generated_at_commit: f55191c
generated_at_date: 2026-07-03
covers_paths: [exarch/src/shell_eval.rs, exarch/src/agent_builtins.rs, exarch/data/agent.ral]
---

# Map: exarch / shell eval

`shell_eval.rs` runs one tool call as a ral top-level turn against the
persistent [[map/core/shell-state|`Shell`]]. **`run_shell` is a pure *request
supplier*: it assembles a `TurnRequest`, hands it to the one source-text turn
door `Shell::run_source_turn`, and renders the flat `TurnReport` that comes
back** ([[internals/a-turn-end-to-end|a turn, end to end]];
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]). Evaluation can
be entered *only* through a framed turn door — the reduction primitive behind it
is crate-private ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]) —
so core owns all the turn machinery (compile, frame install, capture, the wall),
and `run_shell` owns only the request it builds and the outcome it formats:

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
  runs ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]);
- **`caps`** — the session `Capabilities`, pushed for the eval's dynamic extent.
  **This is the sandbox**: the boundary is the pushed [[design/grant|grant]]
  frame plus the [[map/core/evaluator|top-level contract]], not a source-level
  `grant { … }` the model could escape. External commands route through the same
  OS sandbox as ral ([[map/core/capabilities|capabilities]]). The post-run
  `Mobile` installs onto the shell, so `let`, `cd`, and env persist across tool
  calls (the in-module tests pin this);
- **`io: TurnIo::Capture`** — core mints the stdout/stderr buffers and returns
  them in `TurnReport::Ran`'s `captured`: the full, model-visible and logged
  text. Nothing echoes live; the [[map/exarch/frontend|rail]] surfaces cards
  instead, and the [[map/exarch/agent|digest]] caps shape only the model's
  history view;
- **`terminal: RequestedTerminalAccess::Denied`** — a tool turn holds no
  [[decisions/260619_terminal-lease|terminal lease]], so the foreground handoff
  is *uncallable*: a bare pipeline cannot `tcsetpgrp`, and the SIGTTIN crash of
  an agent stealing the controlling terminal becomes a state the types refuse to
  represent. Paired with `stdin: TurnStdin::Empty`, the turn reads no terminal at
  all — an explicit empty source, not a side effect of foreground denial;
- **`turn_limit`** — the per-tool wall. Core arms it on the turn's foreground
  scope *before compiling*, so it bounds the whole turn; only
  `CancelCause::Deadline` reports `timed_out`, which `run_shell` turns into the
  timeout-124 message ("spawn it, let the turn return, `poll`/`await` later"),
  while Esc stays an interrupt. A grant body evaluates locally — no sandbox-IPC
  round trip to interrupt — so cancellation reaches any spawned child through the
  ordinary process-group / cancel-scope path;
- **`detached_limit`** — `DETACHED_WORKER_CEILING` (1 h), the lifetime ceiling
  for workers the turn detaches at the durable root; a backstop, not load-bearing
  for turn exit;
- **`surface`** — the `AgentSink` (below);
- **`lifecycle: Box::new(())`** — a tool turn installs no per-turn hooks.

Completion is `run_source_turn` returning. A detached `spawn`ed worker — a
server, a watch — holds bounded deferred surface storage in core, never a clone
of the bus [[map/exarch/frontend|`Emitter`]], so it cannot keep the tool turn
from ending ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). Frame
teardown — restoring the prior `TurnState`, its byte sinks, its surface, and its
terminal access — is core's `TurnGuard`, which self-heals on a caught worker
panic as well as on the normal return
([[decisions/260612_exarch-panic-recovery|panic-recovery]]); exarch installs no
per-call `IoGuard` of its own. The dynamic-context half of the contract lives in
[[map/exarch/agent|agent]].

**Surface host sink.** `run_shell` passes `surface: Some(Arc::new(AgentSink(emit)))`
in the request; core installs it as the turn-local
[[map/core/shell-state|`SurfaceSink`]] for the extent of the turn — and only the
turn, since it is no longer stored on the persistent session. A ral kit hands a
`` `card `` *render document* — an ordered stack of Bertin marks composed
entirely in ral — to the core `surface` builtin
([[decisions/260619_surface-carries-documents|surface-carries-documents]]);
`AgentSink::emit` decodes each `Value` it receives and emits a `Kind` on the
[[map/exarch/frontend|bus]] through a clone of the call's `Emitter`. It is a
two-decoder sink, io-first:

- a `` `pin ``/`` `unpin `` wrapper normally decodes to `Kind::Pin` /
  `Kind::Unpin`, but `commitment:*` is protected
  ([[decisions/260703_protected-commitment-pins|protected-commitment-pins]]):
  ordinary `surface` writes or clears to that prefix are rejected with a
  diagnostic before they reach the pin mirror or viewport; accepted pins are
  mirrored as `PinDigest { kind, card }` so the agent can distinguish ordinary
  state from commitment state without parsing rendered text;
- an `io`-keyed `Map` core emits at a redirect / exec door decodes through
  `value_to_io` / `io_card` into a `Kind::Io { event, card }`, carrying the raw
  effect record beside its rendering ([[map/exarch/io-surface|io-surface]]);
- any other value tries `value_to_card` and becomes a `Kind::Card`; the closed
  mark set and the `value_to_card` / `render_card` decode-and-bind path live in
  [[map/exarch/cards|cards]];
- a value that is neither shape is dropped, the same graceful degradation
  `value_to_card` gives an unknown mark.

The producer is a direct `surface` call at each kit site, with no cross-language
sentinel constant. Same-thread children inherit the sink; detached workers do
not — core buffers their `surface` calls and replays them once on `await`, so a
bus `Emitter` clone can never outlive the tool turn. Across the OS-sandbox
boundary the events are buffered in the confined child and replayed through the
parent's sink ([[map/core/capabilities|carried on the IPC response]]), so they
are batched rather than live under the sandbox.

`agent_builtins.rs` registers exarch's resident host atoms — the line/window
witnesses and the `grep-files` search and hash-addressed `edit` whose file I/O
happens in Rust, below the redirect frame ([[map/exarch/io-surface|io-surface]]) —
and sources the small embedded `data/agent.ral` helper library (`view-text`,
`view-text-around`) into the shell at boot ([[map/exarch/builtins|builtins]]). The
Rust atoms — but not the sourced library — also dress the
[[map/core/capabilities|sandbox-IPC child]]'s fresh shell, installed through
`set_child_shell_extension`.

A tool command that fails under an active OS sandbox carries a kernel-denial
diagnostic — the blocked syscall, the exact path to grant, the symlink caveat —
appended to the error's `hint`. That harvesting now lives in core
(`core::sandbox::diag`), driven by the command and pipeline runners over the
failing call's wall window and rendered identically by both the `ral-sh` REPL and
exarch ([[map/core/capabilities|capabilities]]).
