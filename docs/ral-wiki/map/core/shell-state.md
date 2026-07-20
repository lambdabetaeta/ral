---
generated_at_commit: 1cff92ae8c6c493aa045926a8977195c7fb16293
generated_at_date: 2026-07-20
covers_paths: [core/src/types/, core/src/types.rs]
---

# Map: core / runtime values & shell state

`core/src/types/` defines what the [[map/core/evaluator|evaluator]] manipulates
at runtime. `types.rs` is a re-export façade so the rest of the tree spells
everything `crate::types::*`.

## Values

- `value.rs` — `Value` (the runtime [[design/cbpv|value]] category); beside it
  `handler.rs` (the user handler stack: `HandlerFrame`, `HandlerStack`,
  `FrameHandle`), `builtin.rs` (`BuiltinEntry` / `BuiltinTable`, kept separate
  from the handler stack), and `handle.rs` (the concurrency substrate behind
  `Value::Handle`: `HandleInner`, `CompletedHandle`, `SurfaceBuffer`).
  Handlers are deep and self-masking, with no
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
  teardown: the pipeline-stage `Io`, the `surface` sink, the `deferred` sink
  (a detached worker's completion delivery — `None` outside an agent host),
  the `desk` answering the turn's enquiries
  ([[decisions/260706_enquiry-channel|enquiry-channel]]), the `nursery`
  holding engine-side session forks a desk handler adopts (`None` outside a
  host that installs one; like `desk`, never given to a deferred worker), the foreground
  `cancel` scope, the source-position `loc` cursor, the deferred-worker
  `WorkerLease` (`deferred_lease` — the idle bound and absolute backstop
  travel as one value; `None` never reaps), the `worker_cap` admission bound
  (`Some(cap)` refuses a spawn of any class while `cap` workers still run;
  `None` admits freely, and both flow into same-thread bodies and spawned
  workers alike), and the turn's `TerminalAccess`.
- **`SessionState`** — what survives every turn's teardown: the durable cancel
  `root` that detached workers parent under (with `publishes_signal_slots`,
  true only for the one signal-facing session per process), the `sources`
  registry rendered against after a turn returns (reset and reseeded at each
  turn start), the `exit_hints` table, the host-installed
  `builtins` with the session's `library_docs`, and the session's
  `terminal_lease`.
- **`LocalState`** — host-local scratch carrying its own flow rules (audit
  trail, REPL scratch, the `workers` registry, the `bindings` ledger); the
  residue once turn and session state are named. The worker registry
  (`shell/workers.rs`) is a per-`Shell` directory of every `spawn`/`watch`ed
  `HandleInner`, one per agent; `Shell::spawn_thread` shares it by `Arc` into
  a spawned worker's own shell (so a nested `spawn` registers alongside its
  parent), but a sub-agent fork or pipeline stage starts with a fresh, empty
  one. Beside the entries it keeps the `ReapNotice` ledger the reap policies
  write — one compact record per entry removed by policy, atomic with the
  removal under the registry's one lock — drained by the host through
  `Shell::take_worker_reap_notices`. A settled entry carries a
  `settled_epoch` stamp: `Shell::advance_worker_epoch`, the host's per-call
  sweep, stamps it at the first advance that observes the entry settled and
  expires it (a `Retention` notice) once its unclaimed result has sat a
  full retention of ral calls — a host that never sweeps (the REPL) retains
  settled entries indefinitely. `Shell::cancel_workers` is the other
  worker operation on `host.rs`'s surface: the host's `/clear` arm, it fires
  every entry's cancel scope and resets both ledgers wholesale — entries and
  pending notices alike — since explicit destruction outranks every lease,
  the durable class included
  ([[decisions/260705_leases-and-budgets|leases-and-budgets]]).
  `WorkerEntry` also implements the small `Resident` signature
  (`types/resident.rs`, [[design/residency|residency]]) — designator,
  population, capability kind, lease row, state label, cancel — so the
  REPL's `jobs` listing and its exit-time survivor warning read a worker's
  facets through it instead of hand-formatting per population.

  The binding-lease ledger (`shell/bindings.rs`,
  [[decisions/260629_agent-binding-reaping|agent-binding-reaping]]) sits
  beside the worker registry but needs no lock: it has exactly one writer,
  the thread that owns `&mut Shell` for every turn, install, and prune
  (verified in the module's own doc comment). Inert (`BindingLedger::
  default()`) until `Shell::arm_binding_lease` seals every name then visible
  in the scope chain as permanently-exempt baseline and starts the
  committed-turn clock. Every persistent top-level scope write funnels
  through one fused chokepoint, `Shell::install_scope_binding` (`scope.rs`,
  beside `bind_value`/`set_var`): it classifies the write by
  `Env::at_session_scope()` and stamps the ledger only when true, so "write a
  scope entry" and "stamp the ledger" can never be pulled apart at a call
  site — the evaluator's four writers (`assign_pattern`'s `Name`/`...rest`
  arms, `eval_letrec`'s two installs) all route here. Host verbs
  (`bind_value`, `set_var`) stay on the raw `Env` primitive, since every host
  call to them precedes arming. Idleness is *use-observation*, not
  re-installation: `Shell::run_turn`'s source arm ticks the committed-turn
  clock, and a lease is renewed by reference — the compiled program's
  `ir::referenced_names` at each successful compile ([[map/core/ir|ir]]), plus
  the resolved name at an `Env`-arm command dispatch. The same chokepoint runs a second, orthogonal
  check: `BindingLease` also carries `large_binding_bytes`, and an install
  whose value's `Value::shallow_size` (a structural estimate — `String`/
  `Bytes` byte lengths, `List`/`Map`/`Variant` recursing into elements,
  `Lambda`/`Block`/`Handle` a small fixed constant, never descended) meets it
  queues a `LargeBindingNotice` regardless of baseline status or idle age —
  residency and lifetime are independent axes
  ([[decisions/260705_leases-and-budgets|leases-and-budgets]] §"Shell
  residency is lexical state plus host leases"). `Shell::leased_binding_count`
  and `Shell::take_large_binding_notices` round out the accessor surface, the
  first a probe figure, the second a boundary drain.

`turn` / `session` / `local` are `pub(crate)`: the fields that encode turn
safety are not a public API. Hosts drive a session through the narrow accessors
gathered in `host.rs`, which a host crate reaches while only `mobile` stays the
public embedding seam. `Shell::binding_count` sits there too — the lexical
scope's probe figure for a host's `/resources` fold
([[invariants/probe-convention|probe-convention]]): a count, never the
values, and enumeration renews nothing.

### Surface

The `surface` sink (`TurnState::surface`, `Option<SurfaceSink>` where
`SurfaceSink = Arc<dyn EventSink>`) is the value-typed dual of the byte
[[map/core/io-process|Io]] sinks. `EventSink` is a *synchronous* trait taking a
borrowed `Value`; `Shell::surface` forwards onto the installed sink and is inert
when none is present (a bare REPL). Turn-scoped, not a persistent capability — a
turn door installs it, so a clone of it has no liveness role and can never decide
a turn is over. A *detached* worker does not receive the live sink: its events
buffer into a `SurfaceBuffer` and are delivered exactly once — replayed through
the awaiting turn's surface on the first `await` / `race`, or handed to the
session-lived `deferred` sink at the worker's own completion, whichever renders
first (a shared `joined` latch decides).

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
- `cwd.rs` (`Cwd`), `inherit.rs` (the flow matrix, below), `modules.rs`,
  `control.rs`, `hooks.rs` (the session-lived hook table of named turn-entry
  points — prompt render, startup, plugin hooks — resolved by the turn door's
  hook-program arm), `repl.rs` (`ReplScratch`, owned by the [[map/repl|REPL]]
  layer).

## The flow matrix

`inherit.rs` centralises *what state crosses a parent→child shell boundary* —
one file rather than a decision scattered across call sites, so no inheritable
datum (the host builtin table among them) can be silently severed by a call site
copying only the fields it happened to remember. There are two regimes.

A **same-thread β-step** — forcing a block or applying a lambda — does not fork:
`Shell::with_thunk_body` runs the body *in* the caller's `Shell`. Only the
`Mobile` is swapped, rescoped to the closure's captured `Env` plus a fresh frame;
the `turn`, `session`, and `local` state are shared *by identity*, so the body
observes the caller's audit trail, byte sinks, builtin table, cancel root, and
terminal lease without any of them being copied. There is no second store to
drift from the first ([[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]).
The `ThunkBody` kind fixes the only two places a block and a lambda differ: a
block enters with the caller's `$?` and folds only `last_status` back; a lambda
enters with a fresh `$?` and folds `{last_status, cwd}` back, so a `cd` inside a
function, alias, or handler persists like every other shell.

The owned-`Shell` modes *are* genuine runtime forks — a different store — and so
copy state explicitly. Each starts from a freshly-defaulted `SessionState` and so
holds **no terminal authority** — `TerminalAccess::Denied`, no lease — the safe
default for a store that is not the session's:

- `spawn_thread` — a spawned worker (`spawn`, `par`, the detached-worker helper)
  on a fresh OS thread that owns its own IO; nothing flows back. Runs under a
  child of the durable root, not the foreground scope, so a turn timeout or Esc
  does not reach it.
- `inherit_from` / `return_to` — the per-substate manifests a cross-process
  pipeline stage (`child_of`, [[decisions/260610_child-eval-unification|child-eval]])
  leans on. Their asymmetry *is* the flow matrix: the source cursor (`turn.loc`)
  and the `within`-attenuable bits do not flow back, but `context.cwd` does.
- `child_from` — a REPL aside (the prompt/hook shell, one call site in the
  [[map/repl|REPL plugin runtime]]): an independent sibling that clones the
  parent's `context`, source cursor, and builtin table without touching its IO /
  audit / REPL scratch; no flow-back.
- `fork_session` — the host session fork (the sub-agent case), the session-scoped
  specialisation of `child_from`. See [[map/exarch/agent|agent]].

Every genuine fork copies `session.builtins` (the dispatch table), so dispatch
reaches the child; the same-thread β-step shares it by identity.
