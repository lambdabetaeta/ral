---
generated_at_commit: e4025de9
generated_at_date: 2026-10-17
covers_paths: [core/src/types/, core/src/types.rs]
---

# Map: core / runtime values & shell state

`core/src/types/` defines what the [[map/core/evaluator|evaluator]] manipulates
at runtime. `types.rs` is a re-export façade so the rest of the tree spells
everything `crate::types::*`.

## Values

- `value.rs` — `Value` (the runtime [[design/cbpv|value]] category); beside it
  `handler.rs` (the user handler stack: `HandlerFrame`, `HandlerStack`,
  `FrameHandle`), `builtin.rs` (`BuiltinEntry` — a checker-owned `Decl` plus the body — and
  `BuiltinTable`, which projects each set into the checker's `Manifest`; kept
  separate from the handler stack — a `Returns` row whose declared scheme settles
  at `Unit` has its answer coerced to `Unit` regardless of what the body returns,
  `Decl::settles_at_unit` reading the curry spine to decide and
  debug-asserting the body agrees), and `handle.rs` (the concurrency
  substrate behind `Value::Handle`: `HandleInner`, which carries its
  worker's `WorkerId`, title and clock — a birth `Stamp` and an end `Stamp`,
  each one instant on a monotonic and a wall clock — and one lock over its
  `HandleState` (`Running`, `Settled`, `Claimed`), beside `CompletedHandle`,
  the worker's `Settler` and its `DeferredSurface`).
  Handlers are deep and self-masking, with no
  `resume` ([[design/effects-handlers|effects-handlers]], [[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]]).
  A *handler* or alias arm is always a lambda: `HandlerEntry` carries its
  `HandlerArity` (`Unary` for a per-name arm or alias, `CatchAll` for `within
  [handler: …]`) fixed by the surface form, and `validate_handler_arity` rejects
  any non-lambda or wrong-arity value at every install boundary — the calling
  convention is never inferred from the runtime shape
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
- `value.rs`'s `Value::heads` gives the heads a value may be admitted at; the
  shapes `execve(2)` has no argument for are `RefusedArg` (`ty/exec_arg.rs`),
  one match on a head with one `remedy` each. Rendering an argv inside the shell
  is total (`Value::render_argv`); this is the one gate, declared once so the
  static refusal and the pre-spawn one cannot disagree
  ([[invariants/exec-argv-is-words|exec-argv-is-words]]).
- `list.rs` / `map.rs` — `List` and `Map`, opaque over a private `Repr`
  wrapping `Arc<imbl::Vector<Value>>` / `Arc<imbl::OrdMap<String, Value>>`;
  `get`/`iter` lend a `Cow<'_, Value>` rather than `&Value`, so a caller
  never knows whether an element is shared or freshly formed; the
  `pub(crate)` mutators go through `Arc::make_mut`. `string.rs` / `bytes.rs` — `Str` and
  `Bytes`, newtypes over `Arc<String>` / `Arc<Vec<u8>>`, so an owned buffer
  moves in without a copy. `Value::string` / `Value::bytes` are their one
  construction door, as `Value::list` is a list's. **Cloning a `Value` copies
  no payload**: a string, a byte string, a list or map spine and a closure's
  scope are all shared, and `Env` and variable lookup rely on it.
- `flow.rs` — the whole control-flow surface, four declarations:
  `Settled<T> = Result<T, Break>`, `Break` (`Error` catchable, `Escape`
  propagating), `Escape` (`Exit` alone — ral does not suspend, so there is no
  stop variant to propagate), and `PolicyError`
  ([[decisions/260514_completion-escape-refactor|completion-escape-refactor]],
  [[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]). No `Option`/null appears;
  optionality is open variants ([[invariants/optionality-via-variants|optionality-via-variants]]).
- `error.rs` — `Error` and `Status` (`Raised(i32) | Cancelled(CancelCause) |
  Process(CommandFailure)`, one constructor per fact whichever door reports
  it; `Status::code` delegates to each failure's own `code`).
  `audit.rs` — the
  `Audit` collector, over `observation.rs`'s `Observation` / `Observed` — the
  one vocabulary shared by the trail, the surface rail, `--audit`'s JSON, and
  the wire. `value.rs` — `Value` and, in `value/`, its two bridges: `first_order.rs` (the one walk each way to `FOValue`) and `compare.rs` (`Value::equals` and `Value::compare`, one equality and one order, exact across Int and Float). `closure.rs` — `Closure`, the thunk value `⟨M, ρ|occ(M)⟩`, its
  fields private and `Closure::new` its one constructor, which `restrict`s
  the environment to the names its body mentions
  ([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
  `env.rs` — lexical `Env`, ρ alone, and
  `EnvVars` process-env overrides; a scope entry is
  `Binding { value, scheme: Option<Arc<Scheme>> }`, so the checker's verdict
  rides next to the value
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
  **A shared write clones a node, and a node clones cheaply**: `Env`'s
  private `Entries` is a sorted array of at most `SMALL` (8) bindings, a persistent
  hash map past it, so a `bind` into an environment another holder still
  references copies only the touched node — the whole array up to `SMALL`,
  a path past it — and copying a node clones every `Binding` stored inline in
  it. Each such clone is cheap, whatever the binding holds: `Binding.scheme`
  sits behind an `Arc`, and the value copies no payload (above). An alias's
  `HandlerEntry.scheme` is shared the same way, so `binding_schemes` and
  `alias_schemes` hand the next run's check the very `Arc`s the scope holds;
  only the wire form keeps its own type and converts at its boundary.
  `signature.rs` — `Signature`, Σ: the natives and the frozen prelude, one
  per `Shell`, `Arc`-shared into every fork. `crate::types::lookup(name, env,
  sig)` is the one resolution rule, ρ then Σ's prelude then Σ's natives; nothing
  else in core spells the shadowing order out again.
- `coerce.rs` — the `sig` / `sig_hint` runtime-error constructors and the
  `Value::as_str` / `as_list` / `as_map` / `expect_handle` family of argument
  decoders, which share one wording (`Value::expected`), sitting below both the
  builtin and capability layers so each reaches them through `crate::types::*`.

## Capabilities

`capability.rs` holds the capability *types* the [[map/core/capabilities|grant]]
decision layer interprets: `Capabilities` (one layer), `ExecGrant` over
`Evidence`, `FsPolicy`,
`EditorPolicy`, `ShellPolicy`, `GrantStack` (`Vec<Capabilities>`, a *stack* of
layers rather than a folded aggregate — attenuation is the fold over the
stack's verdicts, not a `Capabilities::meet`
([[decisions/260906_object-not-name|object-not-name]]); a scoped push —
`with_capabilities`, `with_layers`, `Frame::Grant` — removes its own frames by
position (`GrantStack::remove`) rather than popping the top, so a session
frame `push_session_capabilities` lays inside it, as the boot door's
`--capabilities` does inside its run, survives it), `SandboxProjection`,
and `Capabilities::extend`, which adds an `--extend` `Fragment` to a base once (tested in
`capability/lattice_tests.rs`). `Meet` survives only where evidence genuinely narrows by
intersection, not as a whole-`Capabilities` operation.

## Shell

`shell/` partitions the interpreter state into fields by lifetime — the
field name *is* the invariant — joined by `Shell`
([[decisions/260617_turn-local-state|turn-local-state]],
[[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]]):
`env` (ρ, the session environment, extended by every `Define` that lands),
`sig` (Σ — natives and the frozen prelude, `Arc<Signature>`, one per shell,
never checkpointed since nothing but boot and the one prelude bake ever
writes it) and `context` (dynamic context — `cd`, aliases) — three
flat fields, since a step's focus carries its own environment and there is no
ambient `scope` for a bundle to snapshot — plus `Io`, `SessionState`, and
`LocalState` below. The run door checkpoints and rolls back the `(env,
context)` pair around every run (`Shell::run_under`), so a panicking run
reports as a failed run instead of corrupting the store; `sig` needs no
checkpoint, since a run never writes it.
- **`Io`** — the run's *byte streams*, and the only part of the frame the
  `Shell` carries (as the field `io`): stdin / stdout / stderr, the terminal
  snapshot, the launch role ([[map/core/io-process|io-process]]). These
  genuinely change *within* a run — a redirect frame swaps the sinks — so
  they are taken on **loan** and repaid: `run::IoLoan` swaps a fresh `Io` in
  at install and restores the previous one on `Drop`, carrying the two `Copy`
  registers the run's frame also owns for its life (`session.root_file` and
  `local.audit.call_site`) with it, so an unwinding evaluation cannot leave a
  stale register behind. The loan carries the current cancel scope too.
- **`scope`** — the cancel scope the computation in focus polls
  (`Shell::poll`), which also genuinely changes within a run: a thread base
  sets it to its mooring's node, and `Frame::Cleanup` swaps it for a shielded
  child for a `guard` cleanup's extent and restores it, as a capture swaps
  `io.stdout` (`Shell::shield`). It is a `Scope`, read, never written: writes aim at
  `Mooring::home`, a `ScopeNode`
  ([[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]]).
- **`SessionState`** — what survives every run's teardown: the durable cancel
  `root` that a run's loose workers parent under (`Shell::join_session` *shares*
  it, for a second
  `Shell` the engine runs beside that session — an aside hook,
  [[decisions/260726_cancel-is-a-join|cancel-is-a-join]]), the
  `anchor` a run entered through `Shell::run` nests its foreground frame
  under — minted once, from the session's own root, so the scope tree is the
  LIFO extent it claims to be — the `sources`
  registry rendered against after a run returns (append-only for the session,
  so a nested run's spans can never alias an outer run's `FileId`) and the
  `root_file` naming the current run's root source, the `exit_hints` table
  (one `Arc`, shared by every child), the `fixings` store (what each unit's
  `comparable` variables were fixed to: the session's threads share it, a new
  session snapshots it, [[map/core/ty|ty]]), the host-installed
  `builtins` with the session's `library_docs`, the session's
  `terminal_lease`, and
  the `hooks` table (a worker or a stage dispatches none; a hook registered by a run that then panics is not rolled back with the `(env, context)` checkpoint).
- **`LocalState`** — host-local scratch carrying its own flow rules (audit
  trail, REPL scratch, the `workers` registry, the `bindings` ledger, the
  `detach` budget, the `cradle`); the
  residue once run and session state are named. The `cradle`
  (`types/handle/brood.rs`) says where a loose birth hangs and who settles
  it: loose under the root, answering for nothing, for a run and an aside
  (`Shell::construct`, `Shell::join_session`); a worker's own node and its
  `Brood`, the handles of every `spawn`/`watch` born inside it, set first
  thing in the worker closure; a stage shares its parent's, so a `spawn` in a
  stage inside a worker is that worker's
  ([[invariants/a-worker-ends-with-its-children|a-worker-ends-with-its-children]]). The worker registry
  (`shell/workers.rs`) is a per-`Shell` directory of every `spawn`/`watch`ed
  worker, one per agent: a `WorkerEntry` is the `HandleInner` and its
  `Owner` (the session, the worker it was born inside, or a calling
  expression), the id, title and clock being the handle's own; `Shell::spawn_thread` shares it by `Arc` into
  a spawned worker's or a pipeline stage's own shell (so a nested `spawn`
  registers alongside its parent), but a sub-agent fork or an aside starts
  with a fresh, empty one carrying the parent's retention. Beside the entries it keeps the `ReapNotice` ledger the reap policies
  write — one compact record per entry removed by policy, atomic with the
  removal under the registry's one lock — pushed by the engine as
  `` `notice `` surface events at each settled source run's ready boundary
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
  every registered worker's scope through the `Drop` of `Roster`, so a
  session's workers die with its store — a host's `/clear`, which replaces
  the outgoing shell wholesale, needs no cancel call site, and no worker
  outlives its session's store. The
  `Roster::Owned`/`Roster::Shared` split is what keeps that edge honest: a
  `spawn_thread` child holds a `Shared` roster over its *parent's* registry,
  so its own drop must not reap the parent's.

  The binding-lease ledger (`shell/bindings.rs`,
  [[decisions/260629_agent-binding-reaping|agent-binding-reaping]]) sits
  beside the worker registry but needs no lock: it has exactly one writer,
  the thread that owns `&mut Shell` for every run, install, and prune
  (verified in the module's own doc comment). Inert (`BindingLedger::
  default()`) until `Shell::arm_binding_lease` seals every name then visible
  in the scope chain as permanently-exempt baseline and starts the
  committed-run clock. Every persistent top-level scope write funnels
  through one fused chokepoint, `Shell::note_define` (`scope.rs`,
  beside `bind_value`/`set_var`), so "write a scope entry" and "stamp the
  ledger" can never be pulled apart: `Ran::define`'s single per-name
  loop over `pattern::destructure` calls it for every name a
  `Define` phrase installs under `Mode::Session` — a plain `let`, a
  destructuring pattern's `Name`/`...rest` arms, or a `LetRec` group's
  mutually-recursive members alike — and never under `Mode::Local`. Host verbs
  (`bind_value`, `set_var`) stay on the raw `Env` primitive, since every host
  call to them precedes arming. Idleness is *use-observation*, not
  re-installation: `Shell::dispatch`'s `Source` arm ticks the committed-run
  clock, and a lease is renewed by reference at both harvest seams — the
  run's own compiled program, and a runtime-compiled `use` load
  (`check_source` in `core/src/load.rs`) —
  each reading `ir::referenced_names` off what it just compiled
  ([[map/core/ir|ir]]). The same chokepoint runs a second, orthogonal
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
  the owning session's budget. The bound is per session: a sub-agent fork
  gets the same budget unspent, armed by `construct` from its terms, with no recipe. It is
  equally deliberately absent from
  `LocalState`'s `Drop`: the surviving processes are the one thing a teardown
  must leave alone.

Every `Shell` field — `env`, `sig`, and `context` included — is `pub(crate)`: the
partition encodes run safety, capability attenuation, and wire framing, and
is core's invariant to keep, not an API a host may reach past. Hosts drive a
session through the intent verbs gathered in `host.rs`, plus the scope and
context verbs. `Shell::binding_count` sits there too — the lexical scope's
probe figure for a host's `/resources` fold
([[invariants/probe-convention|probe-convention]]): a count, never the
values, and enumeration renews nothing.

### The mooring

What a run *fixes* is not on the `Shell` at all. The eight run-invariant
members are a **`Mooring`**: the `surface` sink, the `deferred` sink (a
worker's completion delivery — `None` outside an agent host), the
`desk` answering the run's enquiries
([[map/core/engine-protocol|engine-protocol]]), the `fork` door
naming how a forked session reaches the desk that adopts it (`None` outside a
host that installs one; like `desk`, never given to a deferred worker), the
owner's node `home` (the base of the run's scope tree: what a heard key, a
wall or a `cancel` strikes, never what is polled), the
`cleanup_grace` a `guard` cleanup's shield gets from the strike (the host's
policy, from `Run::cleanup_grace`), the
`worker_cap` admission bound (`Some(cap)` refuses a spawn while
`cap` workers still run; `None` admits freely), and the run's
`terminal_access`. It is an owned local on the run
door's own Rust stack frame, and reaches every callee as an explicit
`&Mooring`, placed immediately before the `&Shell` / `&mut Shell` in every
signature.

The `fork` door is a two-armed sum, `Fork` (`mooring.rs`), read through
`Mooring::fork`:

- **`Fork::Park(Nursery)`** — in-process. The reentrancy law bars a
  same-process desk handler from holding `&mut Shell`, so it cannot fork a
  session itself: the builtin body parks `Shell::bequeath`'s `Bequest`,
  the bequest waits in the `Nursery` the identity carrier owns and lays on each
  dispatch, and the handler redeems its `NurseryId` through the parent's own
  transport, `IdentityTransport::adopt_parked` — a new transport over a
  `Shell::child_of` the bequest, never a `Shell` in the host's hands
  (`Nursery::adopt` is `pub(crate)`).
- **`Fork::Seal`** — across a wire. The builtin seals the `Bequest` and names
  the bytes in its enquiry; the host makes the child engine and hands it the
  seal in its `Attach` ([[map/core/transport|transport]]). The builtin does no
  I/O and there is no pen.

Both arms take the same `Shell::bequeath`, which is the whole of
[[design/agents|agents]]'s one-snapshot law: `` exarch-agents `start `` means one
thing regardless of seat.

Immutability is what makes the frame free. A value that never moves needs no
putting back: an outer run's mooring is restored by the stack unwinding, and
a `NurseryGuard` beside it empties a `Park` arm's pen on that same unwinding.
In effect terms the split separates a Reader (`&Mooring`, whose `local` is
applying a function under the mooring it is handed) from State (`Io` under a
loan). **Borrow when you can, loan when you must.**

`&Mooring` and `&mut Shell` are disjoint borrows, so a builtin body can surface
an event while holding the shell mutably. `Mooring` is not `Clone`:
`lend_terminal` is the one lawful derivation, so the raise-never-mint rule on
`TerminalAccess` lives in one door rather than in every bulk copy. Outside
core only `home`, `Mooring::surface`, `Mooring::lend_terminal`,
`Mooring::fork`, and `Mooring::adrift` are reachable —
`adrift()` being the mooring a host builds to call a builtin body outside any
run (no surface, no rail, no desk, no fork door, and a scope under a root
nothing else holds, so cancelling it is how such a caller drives the body's poll points).

A worker *rebuilds* rather than sharing (`Mooring::for_worker`): it keeps the
deferred rail and cap, takes its own buffering surface, and gets
neither desk nor fork door — both barred to something that outlives its run's
Report — under a `worker` scope of the durable root, so a SIGTERM reaches it
and a Ctrl-C does not.

### Surface

The `surface` sink (`Mooring::surface`, `Option<SurfaceSink>` where
`SurfaceSink = Arc<dyn EventSink>`) is the value-typed dual of the byte
[[map/core/io-process|Io]] sinks. `EventSink` is a *synchronous* trait taking a
borrowed first-order `FOValue`
([[map/core/engine-protocol|engine-protocol]]); the `Mooring::surface`
method takes a borrowed `Value`, encodes it once at that door, and forwards onto
the installed sink — inert when none is present (a bare REPL).
`Mooring::surface_data` is its `FOValue`-typed sibling, for a caller that
already holds the wire form: `Shell::observe_stamped` (`types/audit/door.rs`) reaches it
with `Observation::to_surface`, tagged `` `observed `` so a host dispatches on
the tag alone without re-encoding a `Value` it never had. Run-scoped, not
a persistent capability — a run door installs it, so a clone of it has no
liveness role and can never decide a run is over. A worker does not receive the live sink: its surface is a
`DeferredSurface` (`types/handle/surface.rs`) that buffers and, through the
worker's `Settler` on every exit, delivers the batch once to the session's
`DeferredSink` as the worker settles, before its handle does; a mooring without that sink gives
its workers no surface
([[decisions/260622_surface-delivers-itself|surface-delivers-itself]]).

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
scope. It is `None` from `Denied` and from `ExplicitLoan`: a loan raises only
a run that holds the terminal, never mints authority, and is never lent twice.

### Method modules

Methods on `Shell` live by concern, one submodule each:

- `construct.rs` — construction (`Shell::construct`, `Shell::root`, `Shell::child_of`). The startup seeding is not here but in `boot::seed_host_env`: the env-var pass into
  `context.env_overrides` ([[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]];
  a default fills only an absent variable, and one the host binds in bytes
  that are not UTF-8 gets no entry, so children inherit it untouched),
  and the machine facts (`OS_NAME` / `OS_ARCH` / `OS_FAMILY`) seeded into the environment (`env`). `construct.rs` also holds the construction's types: `Runtime` (the process
  half: Σ, the builtin table with its base handler layer, library docs, the
  terminal state; from the installer, shared through its `Arc`s), `Image`
  (what a child can inherit: env, context, sources, `Terms`, fixings), `Terms`
  (the session's policy: stack limit, exit hints, lease, retention, detach
  budget — one type in memory and on the wire), and `Bequest` (`Image` and the
  `SpawnGrant`). `Shell::construct(runtime, image, root)` is the one session
  literal, with a root's hooks and terminal lease passed beside the image so a
  bequest cannot express either; `Shell::root(runtime)` and
  `Shell::child_of(runtime, bequest, narrow)` are its two instances;
- `host.rs` — the host-embedding accessor surface;
- `context.rs` — the `Context` dynamic-context verbs;
- `scope.rs` — `within` / `grant` guards realising [[design/scoping|scoping]];
- `checks.rs` — forwarders to the
  [[map/core/capabilities|`capability::check_*(&Context, …)`]] decisions,
  splitting the disjoint context/audit borrow for the audit-bearing checks;
- `cwd.rs` (`Cwd`, the one working-directory cell: `cd` sets it,
  `enter_cwd`/`restore_cwd` save and restore it whole for `within [dir:]`, and
  `seed_cwd` lets an in-process front end whose working directory is not the
  process cwd state it directly), `inherit.rs` (the
  flow matrix, below), `scrub.rs` (`Shell::bequeath` and its `Scrub`,
  the one snapshot law's whole mechanism, below), `modules.rs`, `detached.rs`
  (the `detach` budget), `hooks.rs` (the session-lived hook table of named
  run-entry points — prompt render, startup, plugin hooks — each a block, a
  lambda or a builtin, resolved by the run door's hook-program arm under the hook's
  registered `DefaultPolicy`: terminal authority, capture, and `aside`; a
  fault in a hook run is labelled by `HookName::fault_label`, and a hook run
  is no ready boundary); the table is an `Option<HookTable>`, `Some` only on the booted session (every child holds `None`), and carries the [[map/repl|REPL]]'s plugin ledger beside the hooks.

## The flow matrix

`inherit.rs` centralises *what state crosses a parent→child shell boundary* —
one file rather than a decision scattered across call sites, so no inheritable
datum (the host builtin table among them) can be silently severed by a call site
copying only the fields it happened to remember. There are two regimes.

A **same-thread β-step** — forcing a block or applying a lambda — does not fork:
`force`/`beta` in `evaluator::machine` put the body in focus *in* the
caller's `Shell`, no snapshot or restore around the call ([[map/core/evaluator|evaluator]]).
The `io`, `session`, and `local` state are simply the one `Shell`'s, and the
caller's `&Mooring` is passed along, so the body observes the caller's audit
trail, byte sinks, builtin table, cancel scope, and terminal lease without any
of them being copied — there is no second store to drift from the first.
Block and lambda entry are uniform: an unbracketed store write in either
body (`cd`, `alias`, a hook registration) persists to the caller, no snapshot
standing between the body and the store
([[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]]);
`within [dir:]` is the working directory's local-state handler, undoing a `cd`
in its body, and `within [handlers:]` the scoped form for handlers
([[decisions/260925_within-dir-is-local-state|within-dir-is-local-state]]).

The owned-`Shell` modes *are* genuine runtime forks — a different store — and are
all built by the one exhaustive literal in `Shell::child`, so a field added to
`Shell` cannot reach a child by defaulting. Each holds **no terminal
authority**: no lease on the session, and the mooring the fork is handed
carries `TerminalAccess::Denied` — the safe default for a store that is not
the session's. Each also starts with stdin `Empty`, no hooks and no control
counters; what rides along is the scope, context, Σ, builtin table, source
registry, root file, call site and exit hints. The kinds differ only in their
`Holding` — cancel root and anchor, roster, lease ledger, detach budget,
fixings:

- `spawn_thread` — a spawned worker (`spawn`, `watch`, a `par` or `all`
  child) *and* a ral-written pipeline stage
  ([[map/core/runtime|runtime]]'s `pipeline/thread.rs`) — a pipeline never
  rides a re-exec'd child — on a fresh OS thread that owns its own IO, seeded
  from the spawning shell's session, built on the spawning thread; the body's
  capture rides in its closure, and nothing flows back. A worker's mooring is rebuilt by `Mooring::for_worker` on the
  calling thread (so the door can hand the caller the worker's scope) and moved
  into the thread, which is why it runs under a child of its cradle — the
  durable root in a run, the parent worker's node inside a worker — rather
  than the foreground scope, and a run timeout or Esc does not reach it (a
  `par` or `all` child hangs under the call's current scope instead, so they
  do); a stage thread instead gets `Mooring::for_stage_thread`, a child of the
  *node's own* cancel scope, so a pipeline-wide cancel reaches every stage
  transitively and the pipeline's own surface/deferred rail carries over
  rather than a worker's fresh one. It shares the session's root, roster,
  detach budget and fixings; its lease ledger is unarmed.
- `join_session` — an aside: an independent sibling that clones the parent's
  `context`, source cursor, builtin table, and Σ (`sig`, by `Arc` clone)
  without touching its IO / audit / REPL scratch; no flow-back. It shares the
  parent's cancel root, so code there is interruptible while it runs and older
  interrupts stay out of its reach; the run door runs a hook there when its
  registered `DefaultPolicy` says `aside` (the REPL's buffer-change hooks).
  It shares the detach budget and fixings too, but holds a roster of its own
  and an unarmed ledger.
- `bequeath` / `child_of` — the host session fork (the sub-agent case): a new
  session, holding its parent's policy and none of its state.
  `Shell::bequeath(grant)` (`scrub.rs`) packages the parent's `Image` as a
  `Bequest`; `Shell::child_of(runtime, bequest, narrow)` constructs over it
  through `Shell::construct`. The child's durable root is its own, and its
  roster carries the parent's retention. Its lease ledger runs on the parent's
  terms and seals everything inherited as baseline
  ([[internals/binding-leases|binding-leases]]). It gets an unspent detach
  budget of the same size, the parent's `SourceDb`, and a snapshot of the
  fixings. A sealed child takes the same path: its `EngineSeed` is the
  bequest in wire form ([[map/core/transport|transport]]), and no recipe runs.
  The round-trip test is a codec test and a constructor test.
  `bequeath` is the door every sub-agent fork passes through: one
  `Scrub` over every root the fork holds — the session scope and each
  handler frame's arms (`HandlerFrame::values_mut`). A fork's hook table is
  empty by construction: hooks are session state, never inherited. The scrub replaces each `Value::Handle` it reaches with an
  `` `opaque `` placeholder, the binding's name and scheme surviving.
  - It walks *scopes*, not values: each scope the roots reach, once, by
    `ptr_eq` root identity, dependencies first, on an explicit stack — so a
    chain of closures costs no host stack, and a scope many closures share
    is visited once.
  - Within a scope it recurses through data only — lists, maps, variant
    payloads, a native's `applied` arguments. A closure is never entered: it
    is re-seated over its captured scope's scrubbed replacement.
  - A rebuilt scope or container is its original with only the changed
    entries overwritten, and one that reaches no handle is kept whole: a
    shell reaching no handle forks as itself, its session map `ptr_eq` to
    the parent's.

  See [[map/core/transport|transport]] and [[map/exarch/agent|agent]].

Every genuine fork copies `session.builtins` (the dispatch table), so
dispatch reaches the child; the same-thread β-step shares it by identity. The
guest jail is not session state: it is process-global
([[map/core/io-process|io-process]]).
