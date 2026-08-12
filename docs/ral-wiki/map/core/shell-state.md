---
generated_at_commit: 5afa1c81
generated_at_date: 2026-08-12
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
  `Audit` collector, over `observation.rs`'s `Observation` / `Observed` — the
  one vocabulary shared by the trail, the surface rail, `--audit`'s JSON, and
  the wire. `env.rs` — lexical `Env` and
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
/ `Join` lattice operations (tested in `capability/lattice_tests.rs`). `Meet`
attenuates live frames; `Join` widens a base overlay, but Boolean `false`
permissions remain sticky vetoes.

## Shell

`shell/` partitions the interpreter state into **four fields by lifetime — the
field name *is* the invariant** — joined by `Shell`
([[decisions/260617_turn-local-state|turn-local-state]]):

- **`Mobile`** — the persistable computation state (lexical `scope` +
  `ControlState` + dynamic `Context`) that crosses evaluation boundaries and
  thread spawns. `mobile` is the public embedding seam. The run door
  checkpoints and rolls back the `Mobile` around every run, so a
  panicking run reports as a failed run instead of corrupting the store.
- **`Io`** — the run's *byte streams*, and the only part of the frame the
  `Shell` carries (as the field `io`): stdin / stdout / stderr, the terminal
  snapshot, the launch role ([[map/core/io-process|io-process]]). These
  genuinely change *within* a run — a redirect frame swaps the sinks — so
  they are taken on **loan** and repaid: `run::IoLoan` swaps a fresh `Io` in
  at install and restores the previous one on `Drop`, carrying the two `Copy`
  registers the run's frame also owns for its life (`session.root_file` and
  `local.audit.call_site`) with it, so an unwinding evaluation cannot leave a
  stale register behind.
- **`SessionState`** — what survives every run's teardown: the durable cancel
  `root` that detached workers parent under (minted deaf to the ambient
  causes; `Shell::face_signals` re-mints it facing, for the host that owns the
  process's signals, and `Shell::join_session` *shares* it, for a second
  `Shell` the host runs beside that session,
  [[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]), the
  `anchor` a *top-level* run nests its foreground frame under — re-minted
  wherever the root is and never afterwards, so the scope tree is the LIFO
  extent it claims to be — the `sources`
  registry rendered against after a run returns (append-only for the session,
  so a nested run's spans can never alias an outer run's `FileId`) and the
  `root_file` naming the current run's root source, the `exit_hints` table,
  the host-installed
  `builtins` with the session's `library_docs`, the session's
  `terminal_lease`, and the `guest_jail` (`Some` only inside a VM guest —
  the spawn jail's shared uid counter, [[map/core/io-process|io-process]]).
- **`LocalState`** — host-local scratch carrying its own flow rules (audit
  trail, REPL scratch, the `workers` registry, the `bindings` ledger, the
  `detach` budget); the
  residue once run and session state are named. The worker registry
  (`shell/workers.rs`) is a per-`Shell` directory of every `spawn`/`watch`ed
  `HandleInner`, one per agent; `Shell::spawn_thread` shares it by `Arc` into
  a spawned worker's own shell (so a nested `spawn` registers alongside its
  parent), but a sub-agent fork or pipeline stage starts with a fresh, empty
  one. Beside the entries it keeps the `ReapNotice` ledger the reap policies
  write — one compact record per entry removed by policy, atomic with the
  removal under the registry's one lock — pushed by the engine as
  `` `notice `` surface events at each settled run's ready boundary
  (`emit_ready_boundary_notices`; `take_worker_reap_notices` is
  crate-private, that push its one caller). Which entries belong to which
  dispatch is not the registry's question to answer — a `spawn` observes its
  birth into the dispatch's own trail ([[map/exarch/shell-eval|shell-eval]]),
  and a reader joins that trail against the registry (or the `` `workers ``
  probe, whose rows carry the same `id`) to ask what became of it. A settled
  entry carries a `settled_epoch` stamp too: retention is armed once at boot
  (`Shell::arm_worker_retention`, beside the binding lease), the
  registry's own clock ticks once per source dispatch (`tick_epoch`), and
  the ready-boundary sweep (`sweep_retention`) stamps an entry at the
  first sweep that observes it settled and expires it (a `Retention`
  notice) once its unclaimed result has sat a full retention of ral calls
  — a host that never arms (the REPL) retains settled entries
  indefinitely. Worker teardown is structural: dropping the shell cancels
  every registered worker's scope through `LocalState`'s `Drop`, so a
  session's workers die with its store — a host's `/clear`, which replaces
  the outgoing shell wholesale, needs no cancel call site, and explicit
  destruction outranks every lease, the durable class included. The
  `workers_owned` flag is what keeps that edge honest: a `spawn_thread`
  child shares its *parent's* registry by `Arc`, so its own drop must not
  reap the parent's roster.
  `WorkerEntry` also implements the small `Resident` signature
  (`types/resident.rs`, [[design/residency|residency]]) — designator,
  population, capability kind, lease row, state label, cancel — so the
  REPL's `jobs` listing and its exit-time survivor warning read a worker's
  facets through it instead of hand-formatting per population.

  The binding-lease ledger (`shell/bindings.rs`,
  [[decisions/260629_agent-binding-reaping|agent-binding-reaping]]) sits
  beside the worker registry but needs no lock: it has exactly one writer,
  the thread that owns `&mut Shell` for every run, install, and prune
  (verified in the module's own doc comment). Inert (`BindingLedger::
  default()`) until `Shell::arm_binding_lease` seals every name then visible
  in the scope chain as permanently-exempt baseline and starts the
  committed-run clock. Every persistent top-level scope write funnels
  through one fused chokepoint, `Shell::install_scope_binding` (`scope.rs`,
  beside `bind_value`/`set_var`): it classifies the write by
  `Env::at_session_scope()` and stamps the ledger only when true, so "write a
  scope entry" and "stamp the ledger" can never be pulled apart at a call
  site — the evaluator's four writers (`assign_pattern`'s `Name`/`...rest`
  arms, `eval_letrec`'s two installs) all route here. Host verbs
  (`bind_value`, `set_var`) stay on the raw `Env` primitive, since every host
  call to them precedes arming. Idleness is *use-observation*, not
  re-installation: `Shell::dispatch`'s source arm ticks the committed-run
  clock, and a lease is renewed by reference — the compiled program's
  `ir::referenced_names` at each successful compile ([[map/core/ir|ir]]), plus
  the resolved name at an `Env`-arm command dispatch. The same chokepoint runs a second, orthogonal
  check: `BindingLease` also carries `large_binding_bytes`, and an install
  whose value's `Value::shallow_size` (a structural estimate — `String`/
  `Bytes` byte lengths, `List`/`Map`/`Variant` recursing into elements,
  `Lambda`/`Block`/`Handle` a small fixed constant, never descended) meets it
  queues a `LargeBindingNotice` regardless of baseline status or idle age —
  residency and lifetime are independent axes.
  `Shell::leased_binding_count` rounds out the accessor surface — a probe
  figure, like the `` `largest-binding-bytes `` probe's
  `largest_binding_shallow_size`; large-binding notices ride the same
  ready-boundary `` `notice `` push as the reap ledger's
  (`take_large_binding_notices` is crate-private).

  The `detach` budget (`shell/detached.rs`) is the one member meant to
  outlive the session: `None` until `Shell::arm_detach`, which a host calls
  in the same act that installs `detach`'s base frame
  ([[map/core/builtins|builtins]]), so the verb and the budget it spends
  cannot drift apart. `DetachPolicy::admit` counts *births*, not occupancy —
  a survivor's death is unobservable from here, so a release would be a
  number nobody can compute — and the policy is `Arc`-shared into a spawned
  worker's shell like the registry, so a `detach` inside a `spawn { }` spends
  the owning session's budget. It is equally deliberately absent from
  `LocalState`'s `Drop`: the surviving processes are the one thing a teardown
  must leave alone.

`io` / `session` / `local` are `pub(crate)`: the fields that encode run
safety are not a public API. Hosts drive a session through the narrow accessors
gathered in `host.rs`, which a host crate reaches while only `mobile` stays the
public embedding seam. `Shell::binding_count` sits there too — the lexical
scope's probe figure for a host's `/resources` fold
([[invariants/probe-convention|probe-convention]]): a count, never the
values, and enumeration renews nothing.

### The mooring

What a run *fixes* is not on the `Shell` at all. The eight run-invariant
members are a **`Mooring`**: the `surface` sink, the `deferred` sink (a
detached worker's completion delivery — `None` outside an agent host), the
`desk` answering the run's enquiries
([[decisions/260706_enquiry-channel|enquiry-channel]]), the `nursery` holding
engine-side session forks a desk handler adopts (`None` outside a host that
installs one; like `desk`, never given to a deferred worker), the foreground
`cancel` scope, the deferred-worker `WorkerLease` (`deferred_lease` — the idle
bound and absolute backstop travel as one value; `None` never reaps), the
`worker_cap` admission bound (`Some(cap)` refuses a spawn of any class while
`cap` workers still run; `None` admits freely), and the run's
`terminal_access`. It is an owned local on the run
door's own Rust stack frame, and reaches every callee as an explicit
`&Mooring`, placed immediately before the `&Shell` / `&mut Shell` in every
signature.

Immutability is what makes the frame free. A value that never moves needs no
putting back: an outer run's mooring is restored by the stack unwinding, and
a `NurseryGuard` beside it empties its nursery on that same unwinding. In
effect terms
the split separates a Reader (`&Mooring`, with `Shell::run_nested` as its
`local` — a nested run's frame is a child of the mooring it is handed) from
State (`Io` under a loan). **Borrow when you can, loan when you must.**

`&Mooring` and `&mut Shell` are disjoint borrows, so a builtin body can surface
an event while holding the shell mutably. `Mooring` is not `Clone`:
`lend_terminal` is the one lawful derivation, so the raise-never-mint rule on
`TerminalAccess` lives in one door rather than in every bulk copy. Outside
core only `cancel`, `Mooring::surface`, `Mooring::lend_terminal` /
`in_terminal_loan`, and `Mooring::adrift` are reachable — `adrift()`
being the mooring a host builds to call a builtin body outside any run (no
surface, no rail, no desk, no nursery, and a scope under a root nothing else
holds, so cancelling it is how such a caller drives the body's poll points).

A worker *rebuilds* rather than sharing (`Mooring::for_worker`): it keeps the
deferred rail, lease, and cap, takes its own buffering surface, and gets
neither desk nor nursery — both barred to something that outlives its run's
Report — under a `worker` scope of the durable root, so a SIGTERM reaches it
and a Ctrl-C does not.

### Surface

The `surface` sink (`Mooring::surface`, `Option<SurfaceSink>` where
`SurfaceSink = Arc<dyn EventSink>`) is the value-typed dual of the byte
[[map/core/io-process|Io]] sinks. `EventSink` is a *synchronous* trait taking a
borrowed first-order `FOValue`
([[decisions/260706_enquiry-channel|enquiry-channel]]); the `Mooring::surface`
method takes a borrowed `Value`, encodes it once at that door, and forwards onto
the installed sink — inert when none is present (a bare REPL). Run-scoped, not
a persistent capability — a run door installs it, so a clone of it has no
liveness role and can never decide a run is over. A *detached* worker does not receive the live sink: its events
buffer into a `SurfaceBuffer` and are delivered exactly once — replayed through
the awaiting run's surface on the first `await` / `race`, or handed to the
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
- A run's authority to borrow it is the `TerminalAccess` on its `Mooring`:
  `Denied` (the safe default — an exarch tool run, the boot
  frame), `Leased` (an interactive run), or `ExplicitLoan` (a within-run
  elevation). `Shell::terminal_lease(mooring)` yields
  `&TerminalLease` only when access permits *and* the session owns a lease, so a
  `Denied` run cannot construct a foreground handoff.

Because the mooring is invariant, a raise is not a mutation but a derivation:
`Mooring::lend_terminal` returns a *new* mooring with `Leased` raised to
`ExplicitLoan` for `_ed-tui`, and the borrow ends when that value goes out of
scope. It leaves a `Denied` run untouched, closing the
`Denied → ExplicitLoan` door so a loan can only raise an authorised run,
never mint authority.

### Method modules

Methods on `Shell` live by concern, one submodule each:

- `init.rs` — construction, the startup env-var seeding pass into
  `context.env_overrides` ([[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]),
  and the machine facts (`OS_NAME` / `OS_ARCH` / `OS_FAMILY`) seeded into `$env`;
- `host.rs` — the host-embedding accessor surface;
- `context.rs` — the `Context` dynamic-context verbs;
- `scope.rs` — `within` / `grant` guards realising [[design/scoping|scoping]];
- `checks.rs` — forwarders to the
  [[map/core/capabilities|`capability::check_*(&Context, …)`]] decisions,
  splitting the disjoint context/audit borrow for the audit-bearing checks;
- `cwd.rs` (`Cwd`; `seed_cwd` lets an in-process front end whose working
  directory is not the process cwd state it directly), `inherit.rs` (the
  flow matrix, below), `modules.rs`, `detached.rs` (the `detach` budget),
  `control.rs`, `hooks.rs` (the session-lived hook table of named run-entry
  points — prompt render, startup, plugin hooks — resolved by the run door's
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
the `io`, `session`, and `local` state are shared *by identity*, and the
caller's `&Mooring` is simply passed along, so the body observes the caller's
audit trail, byte sinks, builtin table, cancel scope, and terminal lease
without any of them being copied. There is no second store to
drift from the first ([[decisions/260620_same-thread-body-shares-the-session|same-thread-body-shares-the-session]]).
The `ThunkBody` kind fixes the only two places a block and a lambda differ: a
block enters with the caller's `$?` and folds only `last_status` back; a lambda
enters with a fresh `$?` and folds `{last_status, cwd}` back, so a `cd` inside a
function, alias, or handler persists like every other shell.

The owned-`Shell` modes *are* genuine runtime forks — a different store — and so
copy state explicitly. Each starts from a freshly-defaulted `SessionState` and so
holds **no terminal authority**: no lease on the session, and the mooring the
fork is handed carries `TerminalAccess::Denied` — the safe
default for a store that is not the session's:

- `spawn_thread` — a spawned worker (`spawn`, `par`, the detached-worker helper)
  on a fresh OS thread that owns its own IO; nothing flows back. Its mooring is
  rebuilt by `Mooring::for_worker` on the calling thread (so the door can hand
  the caller the worker's scope) and moved into the thread, which is why the
  worker runs under a child of the durable root rather than the foreground
  scope, and a run timeout or Esc does not reach it.
- `inherit_from` / `return_to` — the per-substate manifests a cross-process
  pipeline stage (`child_of`, [[decisions/260610_child-eval-unification|child-eval]])
  leans on. Their asymmetry *is* the flow matrix: the dispatch call site
  (`local.audit.call_site`)
  and the `within`-attenuable bits do not flow back, but `context.cwd` does.
- `child_from` — a REPL aside (the hook shell, one call site in the
  [[map/repl|REPL plugin runtime]]): an independent sibling that clones the
  parent's `context`, source cursor, and builtin table without touching its IO /
  audit / REPL scratch; no flow-back. `join_session` is its aside
  specialisation, sharing the parent's cancel root, so plugin code there is
  interruptible while it runs and older interrupts stay out of its reach.
- `fork_session` — the host session fork (the sub-agent case), the session-scoped
  specialisation of `child_from`. See [[map/exarch/agent|agent]].

Every genuine fork copies `session.builtins` (the dispatch table) and shares
`session.guest_jail`, so dispatch reaches the child and a guest's workers,
stages, and forks share one jail counter; the same-thread β-step shares both
by identity.
