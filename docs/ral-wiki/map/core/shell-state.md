---
generated_at_commit: 1baac6d
generated_at_date: 2026-06-22
covers_paths: [core/src/types/, core/src/types.rs]
---

# Map: core / runtime values & shell state

`core/src/types/` defines what the [[map/core/evaluator|evaluator]] manipulates
at runtime. `types.rs` is a re-export façade so the rest of the tree spells
everything `crate::types::*`.

## Values

- `value.rs` — `Value` (the runtime [[design/cbpv|value]] category), plus the
  handler machinery: `HandlerFrame`, `HandlerStack`, `FrameHandle`,
  `BuiltinEntry` / `BuiltinTable`. Handlers are deep and self-masking, with no
  `resume` ([[design/effects-handlers|effects-handlers]], [[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]]).
  A *handler* or alias arm is always a lambda: `HandlerEntry` carries its
  `HandlerArity` (`Unary` for a per-name arm or alias, `CatchAll` for `within
  [handler: …]`) fixed by the surface form, and `validate_handler_arity` rejects
  any non-lambda or wrong-arity value at every install boundary — the calling
  convention is never inferred from the runtime shape
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
- `list.rs` / `map.rs` — `List` and `Map`, opaque newtypes over persistent
  `imbl::Vector` / `imbl::OrdMap`.
- `flow.rs` — the control-flow surface: `Settled`, `Escape`, `Break`, and the
  crate-internal `Control` / `Raw` / `Tail`
  ([[decisions/260514_completion-escape-refactor|completion-escape-refactor]]). No `Option`/null appears;
  optionality is open variants ([[invariants/optionality-via-variants|optionality-via-variants]]).
- `error.rs` — `Error`, `Status`, and the `BodyResult` split. `audit.rs` — the
  `Audit` collector and `ExecNode` execution tree. `env.rs` — lexical `Env` and
  `EnvVars` process-env overrides; a scope entry is `Binding { value, scheme }`,
  so the checker's verdict rides next to the value
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
- `coerce.rs` — the `sig` / `sig_hint` runtime-error constructors and the
  `as_map` family of `Value` → `Map` coercions, sitting below both the builtin
  and capability layers so each reaches them through `crate::types::*`.

## Capabilities

`capability.rs` holds the capability *types* the [[map/core/capabilities|grant]]
decision layer interprets: `Capabilities`, `ExecPolicy`, `FsPolicy`,
`EditorPolicy`, `ShellPolicy`, `GrantStack`, `SandboxProjection`, and the `Meet`
/ `Join` lattice operations (tested in `capability/lattice_tests.rs`).

## Shell

`shell/` partitions the interpreter state into **four fields by lifetime — the
field name *is* the invariant** — joined by `Shell`
([[decisions/260617_turn-local-state|turn-local-state]]):

- **`Mobile`** — the persistable computation state (lexical `scope` +
  `ControlState` + dynamic `Context`) that crosses evaluation boundaries and
  thread spawns. `mobile` is the public embedding seam.
- **`TurnState`** — the dynamic frame a top-level turn installs and restores on
  teardown: the pipeline-stage `Io`, the `surface` sink, the foreground
  `cancel` scope, the source-position `loc` cursor, the detached-worker
  lifetime ceiling, and the turn's `TerminalAccess`.
- **`SessionState`** — what survives every turn's teardown: the durable cancel
  `root` that detached workers parent under, the `sources` registry rendered
  against after a turn returns, the `exit_hints` table, the host-installed
  `builtins`, and the session's `terminal_lease`.
- **`LocalState`** — host-local scratch carrying its own flow rules (audit
  trail, REPL scratch); the residue once turn and session state are named.

`turn` / `session` / `local` are `pub(crate)`: the fields that encode turn
safety are not a public API. Hosts drive a session through the narrow accessors
gathered in `host.rs`, which a host crate reaches while only `mobile` stays the
public embedding seam.

### Surface

The `surface` sink (`TurnState::surface`, `Option<SurfaceSink>` where
`SurfaceSink = Arc<dyn EventSink>`) is the value-typed dual of the byte
[[map/core/io-process|Io]] sinks. `EventSink` is a *synchronous* trait taking a
borrowed `Value`; `Shell::surface` forwards onto the installed sink and is inert
when none is present (a bare REPL). Turn-scoped, not a persistent capability — a
turn door installs it, so a clone of it has no liveness role and can never decide
a turn is over. A *detached* worker does not receive the live sink: its events
buffer into a bounded `SurfaceBuffer`, drained into the `CompletedHandle` and
replayed through the awaiting turn's surface on the first `await` / `race`.

### Terminal handoff

The authority to hand the controlling terminal to a child is an unforgeable
`TerminalLease`, not an inferred predicate
([[decisions/260619_terminal-lease|terminal-lease]]). It splits across two
lifetimes:

- The lease itself lives on `SessionState::terminal_lease`, minted once at
  startup from the `tcgetpgrp == getpgrp` witness — `Some` when ral owns the
  foreground, `None` otherwise. It is *lent*, never moved or cloned.
- A turn's authority to borrow it is the per-turn `TerminalAccess` on
  `TurnState`: `Denied` (the safe default — an exarch tool turn, the boot
  frame), `Leased` (an interactive turn), or `ExplicitLoan` (a within-turn
  elevation raised only by the host loan token). `Shell::terminal_lease` yields
  `&TerminalLease` only when access permits *and* the session owns a lease, so a
  `Denied` turn cannot construct a foreground handoff.

The host-facing `TerminalLoan` (`host.rs`) raises a `Leased` turn to
`ExplicitLoan` for `_ed-tui` and restores the prior access on surrender; it
leaves a `Denied` turn untouched, closing the `Denied → ExplicitLoan` door so a
loan can only raise an authorised turn, never mint authority.

### Method modules

Methods on `Shell` live by concern, one submodule each:

- `init.rs` — construction, the startup env-var seeding pass into
  `context.env_overrides` ([[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]),
  and the machine facts (`OS_NAME` / `OS_ARCH` / `OS_FAMILY`) seeded into `$env`;
- `host.rs` — the host-embedding accessor surface, plus `TerminalLoan`;
- `context.rs` — the `Context` dynamic-context verbs;
- `scope.rs` — `within` / `grant` guards realising [[design/scoping|scoping]];
- `checks.rs` — forwarders to the
  [[map/core/capabilities|`capability::check_*(&Context, …)`]] decisions,
  splitting the disjoint context/audit borrow for the audit-bearing checks;
- `inherit.rs` — parent⇄child state transfer. A same-thread β-step runs the body
  *in* the caller's `Shell` via `with_thunk_body`: only `Mobile` is swapped for
  one rescoped to the closure's capture, while turn, session, and local state are
  shared by identity, so the body sees the caller's audit trail, byte sinks,
  builtin table, cancel root, and terminal lease without any copy
  ([[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]).
  `ThunkBody` fixes the only two places a block and a lambda differ — the entry
  `last_status` and the fold-back set. The owned-`Shell` forks (`spawn_thread`,
  `child_of`, `child_from`) copy `Context` explicitly and start from a freshly
  defaulted `SessionState`, holding no terminal authority;
- `cwd.rs` (`Cwd`), `modules.rs`, `control.rs`, `repl.rs` (`ReplScratch`, owned
  by the [[map/repl|REPL]] layer).
