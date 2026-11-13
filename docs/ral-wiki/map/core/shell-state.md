---
generated_at_commit: 9290eb03
generated_at_date: 2026-11-13
covers_paths: [core/src/shell/runtime/, core/src/shell/runtime.rs, core/src/ir/value.rs, core/src/ir/value/, core/src/ir/env.rs, core/src/builtin.rs, core/src/handle.rs, core/src/handle/]
---

# Map: core / shell / runtime — runtime values & shell state

**A runtime value is a value closure `⟨V,ρ⟩`; a `Shell` is its state partitioned
by lifetime, and what a run fixes rides beside it as an immutable `Mooring`.**
`core/src/shell/runtime.rs` re-exports the types; the [[map/core/evaluator|evaluator]]
steps over them. The value types themselves live in `core/src/ir/`.

## Values

**`ValClos` (`ir/value.rs`) is `⟨V,ρ⟩` with a non-variable root and every direct
name of `V` bound in ρ** ([[design/cbpv|cbpv]],
[[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]]).

- *Representation.* One form: `ValClos(Val, Env)`. A list or map built at run
  time is syntax over `%itemK` variables bound in ρ; `parts` hands the
  marshaller the syntax and environment, nothing more. `VClosRef(&Val, &Env)`
  is the borrowed form, and every observation is defined on it.
- *Formation.* `ValClos::close` (`shell/runtime/close.rs`) restricts ρ to the
  value's occurrences and pulls each missing direct name from Σ with its
  scheme. `ValClos::thunk` forms `⟨thunk M, ρ↾occ(M)⟩`. `VClosRef::sub` reads a
  component: a variable is its binding in one lookup, anything else shares ρ
  without restricting again
  ([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
  `ValClos::closure` rebuilds a marshalled closure and refuses an unbound
  direct name.
- *Reading.* `VClosRef::view` reads the head constructor only; a
  `View::Variant`'s payload is a `VClosRef`, and a bare tag's is `()`.
- *Classification.* **`VClosRef::shape` is the one classification.** A builtin
  is classified as its η-expansion: arity 0 is a `Block`, otherwise a
  `Function`. `type_name`, `found` and `heads` all derive from it.
- *Builtins.* `ValClos::builtin` is `⟨Val::Builtin(entry), ∅⟩`. Forcing or
  applying it enters `BuiltinEntry::eta`, a λ-chain ending in δ. A partial
  application is that chain's residual thunk; `residual` recovers the entry
  and the arguments still awaited. `block` is a thunk the checker may read,
  never a residual. Display is `<native NAME>` or `<native NAME, k of n
  arguments>`.
- *Tags.* `ValClos::variant` binds a builtin, handle or residual payload to a
  fresh name, so a tag spine holds only syntax.
- *Exec arguments.* `heads` gives the heads a value is admitted at; the shapes
  `execve(2)` cannot take are `RefusedArg` (`ty/exec_arg.rs`), each with one
  `remedy`. `render_argv` is total
  ([[invariants/exec-argv-is-words|exec-argv-is-words]]).
- *Size.* `shallow_size` sums string and byte lengths, recurses lists, maps
  and tag labels, and charges 32 for a thunk, builtin or handle, never
  descending a capture.
- *Drop is trampolined:* a dying value queues each environment and list,
  record or map node it alone owns, bounding the host stack.
- Compile-time bounds: `ValClos ≤ 48`, `Env ≤ 16` and machine `Frame ≤ 128`
  bytes.

Beside it:

- `ir/value/list.rs` / `map.rs` — list and map nodes are `imbl::Vector` /
  `imbl::OrdMap` of syntax. `ListRef` / `MapRef` read them over ρ; `ListRef`
  also holds the one definition of cons, snoc, concat and tail, and
  `ValClos::map` builds a map
  ([[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]]).
- `ir/env.rs` — `Env` is ρ alone, and `Env::place` the one placement rule. A scope entry is `Binding { value, scheme:
  Option<Closed> }`. Entries are `Empty`, a sorted `ThinArc` array of at most
  8, or a persistent hash map, so a shared write copies only the touched node.
  Session bindings always carry a scheme; block-local ones may not
  ([[internals/type-inference|type-inference]]). `Env::from_bindings` refuses
  duplicate names.
- `signature.rs` — **Σ, the natives and the frozen prelude, stored as typed
  `Binding`s**, one `Signature` per shell, `Arc`-shared into every child. A
  native builtin carries its declared scheme (none for a boundary row).
  `lookup` is the one resolution rule: ρ, then Σ's prelude, then Σ's natives.
- `flow.rs` — `Settled<T> = Result<T, Break>`; `Break` is `Error`
  (catchable) or `Escape` (`Exit`, `Stopped`). A process `Cancellation`
  becomes a non-catchable `Stopped`
  ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
- `error.rs` — rendering of runtime `Error` and `outcome_value`, whose `err`
  arm is the plain record `ValClos::from_datum(fact.record())`; the type is `core/src/error.rs` ([[map/core/diagnostics|diagnostics]]).
- `audit.rs` — the `Audit` collector over `fact/observation.rs`.
- `compare.rs` — `equals` and `compare` on `VClosRef`, exact across `Int` and
  `Float`. `ir/value/first_order.rs` converts to and from `FOValue`.
- `coerce.rs` — argument decoders on `VClosRef` (`as_str`, `as_list`,
  `as_map`, `expect_handle`); `expect_thunk` on `ValClos`; shape refusals go
  through `expected`. `expect_thunk` accepts a builtin through its η.
  `expect_handle` refuses a withheld handle as `withheld-handle`.
- `walk.rs` — `Shell::admitted_walk` hands `path::tree::Walk` the live grant
  and cancel scope, skipping unadmitted entries.

`core/src/builtin.rs`:

- `BuiltinEntry` is a checker-read `Decl` plus a body; `BuiltinTable`
  projects installed sets into the checker's `Manifest`.
- A row that settles at `Unit` has its answer coerced to `Unit`; debug builds
  assert the body agrees (`Decl::settles_at_unit`).

`core/src/handle.rs`:

- `Handle` is `Live(HandleInner)` or `Withheld(title)`. A marshalled parent's
  handles arrive withheld; `Handle::live` refuses observing one.
- `HandleInner` carries `WorkerId`, title, birth `Stamp` (monotonic and wall),
  `home` node and one lock over its state, sampled as `Sample::{Running,
  Stopping, Ready, Consumed}`. The sole `CompletionPermit` publishes the end.
- **`pins_live_work` walks lists, maps, tag spines and a partial builtin's
  supplied arguments, never a block's capture**
  ([[internals/binding-leases|binding-leases]]).
- `handle/brood.rs` — `Cradle`, `Brood`, `BroodGuard`; `handle/surface.rs` —
  `DeferredSurface`.

## Handlers

**Handlers are deep and self-masking, with no `resume`**
([[design/effects-handlers|effects-handlers]],
[[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]]).
`handler.rs` holds `HandlerStack`, `HandlerFrame` and `FrameHandle`.

- `HandlerEntry` carries `HandlerArity`, fixed by the surface form: `Unary`
  for a named arm, `CatchAll` for `within [handler: …]`. The calling
  convention is never inferred from the value
  ([[decisions/260619_handlers-are-lambdas|handlers-are-lambdas]]).
- A named arm has `U([String] → Command)`; a catch-all
  `U(String → [String] → Command)`. Base-frame builtins (`Convention::Argv`)
  have that type by construction.
- Persistent installation (`Shell::install_handler` = `vet_handler` +
  `commit_handler`) vets each arm once:
  - `arm_closure` refuses anything but a block the checker can read; a bare
    builtin is named, with the block that wraps it
    (`Expected::BlockNotBuiltin`);
  - `arm_stands_in` checks the block against its slot, inferred over its own
    capture by `Shell::schemes_over`, refusing as one `IllTyped` fact.
- Scoped arms are checked statically and never re-vetted. No statement or
  builtin changes persistent frames; only the host installs them between units
  ([[decisions/260925_within-dir-is-local-state|within-dir-is-local-state]]).

## Shell

**`Shell` (`state/mod.rs`) partitions interpreter state by lifetime; the field
name is the invariant**
([[decisions/260617_turn-local-state|turn-local-state]]). Every field is
`pub(crate)`: hosts use the verbs in `host.rs`, `scope.rs`, `context.rs`.

- `env` — ρ of the session, extended by every session `Define`.
- `sig` — `Arc<Signature>`, never written after boot, so never checkpointed.
- `context` — `Context`: env overrides, cwd, `GrantStack`, handler stack,
  args. Clones wholesale into every child.
- `io` — the three byte streams, swapped within a run by redirect frames.
- `scope` — the cancel scope the focus polls (`Shell::poll`). A thread base sets
  it to its mooring's node; `Shell::shield` swaps it for a `guard`'s
  acquisition or release. It is read, never written: strikes aim at
  `Mooring::home`
  ([[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]]).
- `session: SessionState` — survives every run: durable cancel `root` and
  `anchor`, the append-only `sources` and `root_file`, `exit_hints`, `scratch`,
  `fixings` ([[map/core/ty|ty]]), `runtime` (`Arc<Runtime>`: Σ, builtin
  table, base handler layer, library docs, terminal state), `terminal_lease`,
  `stack_limit`, `child_limits`, `interactive`, `stage`, and `hooks`
  (`Some` only on the booted session).
- `local: LocalState` — host-local, each member with its own rule: `audit`,
  the `workers` roster, the `cradle`, the `bindings` ledger, `machine_depth`.

**A run checkpoints `(env, context)` and restores it on panic**
(`Shell::run_under`). The hooks table and `local.audit` are outside the
checkpoint. Within the run, `IoLoan` (`shell/run.rs`) swaps in `io`, `scope`,
`root_file` and the audit call site, restoring them on `Drop`, so an unwinding
run leaves nothing stale.

### The mooring

**What a run fixes is a `Mooring` (`mooring.rs`), an owned local on the run's
stack frame passed as `&Mooring` beside `&mut Shell`.** Being immutable it needs
no restoring; the two borrows are disjoint.

- Fields: `surface`, `deferred`, `desk`, `home`, `cleanup_grace`,
  `worker_cap`, `worker_title` (`TitlePolicy`), `terminal_access`.
- `worker_cap` refuses a spawn while live plus reserved seats reach it.
- `Mooring::for_thread` rebuilds one for a worker or stage: same deferred
  sink, grace, cap and title policy; its own `home` and `surface`; no desk;
  `TerminalAccess::Denied`.
- `Mooring::adrift` runs a builtin body outside any run: nothing installed,
  `home` under a root nothing holds.
- `has_desk` gates enquiries; `no_desk` is the shared refusal.

**Surface.** `Mooring::surface` encodes a `ValClos` to `FOValue` once and
forwards to the `SurfaceSink`; `surface_data` takes `FOValue` directly. Both are
inert without a sink. A session worker gets a `DeferredSurface`, delivered once
through its `CompletionPermit` to the `DeferredSink`; a gather child or stage
gets the parent's live sink
([[decisions/260622_surface-delivers-itself|surface-delivers-itself]]).

**Terminal.** Authority is a permit, never a resource getter
([[map/core/io-process|io-process]]).

- `SessionState::terminal_lease` is minted once at startup when ral owns the
  foreground; hosts request `RequestedTerminalAccess` and cannot mint one.
- `TerminalAccess` is `Denied`, `Leased` or `ExplicitLoan`. `reach` yields a
  `TerminalPermit` only with the lease, and for `Leased` only when stdout is
  the terminal.
- `Mooring::lend_terminal` derives a new mooring raising `Leased` to
  `ExplicitLoan`, and is `None` otherwise: authority is raised, never minted,
  and never lent twice.
- `Shell::group_owner` decides once: a stage answers its `Membership`; a
  session gathers its permit and interactivity.

### Workers and the cradle

- **`Roster`** (`state/workers.rs`) is a per-session `WorkerRegistry` of
  `WorkerEntry` (handle, `Owner`, `settled_epoch`). Threads share it
  (`Roster::Shared`); a session owns it (`Roster::Owned`), and dropping an
  owned roster cancels every worker (`cancel_all`), waiting up to
  `DEFAULT_CLEANUP_GRACE + TEARDOWN_GRACE`.
- **Retention.** `Shell::arm_worker_retention` arms it; each source dispatch
  ticks the epoch; `sweep_retention` stamps a `Ready` entry and expires it
  after the retention, leaving entries of a live parent worker or a gather
  alone. Expiry queues a `ReapNotice`. Unarmed (the REPL), `Ready` entries
  stay.
- **`Cradle`** says where a loose birth hangs: under the durable root for a
  run or aside (`Cradle::loose`), or inside a gather with its `Brood`
  (`Cradle::owning`). A stage keeps its parent's
  ([[invariants/a-worker-ends-with-its-children|a-worker-ends-with-its-children]]).

### Binding lease

**`BindingLedger` (`state/bindings.rs`) has one writer, the thread owning
`&mut Shell`, so it takes no lock**
([[internals/binding-leases|binding-leases]]).

- Inert until `Shell::arm_binding_lease` seals every visible name as baseline.
- `Shell::note_define` is the one write path for session names, called only
  from `Ran::define` under `Mode::Session`. It renews the lease and queues a
  large-binding notice when `shallow_size` meets `large_binding_bytes`.
- `Shell::set_var` binds a host value with an explicit `Closed` scheme and
  bypasses the ledger.

**Ready boundary.** `emit_ready_boundary_notices` (`shell/run.rs`) runs after
each settled source run:

1. `sweep_retention`, always;
2. large-binding notices to stderr, always;
3. only with a surface sink: reap notices and `BindingLedger::prune`, each as
   a `` `notice `` surface event. Without a sink both ledgers wait.

## Method modules (`state/`)

- `construct.rs` — `Runtime`, `Image` (env, handler frames, sources, `Terms`,
  fixings), `Terms` (stack and child limits, exit hints, scratch, lease,
  retention; one type in memory and on the wire). `Shell::construct` is the
  one session literal; `Shell::root` and `Shell::child_of` are its two uses.
  `Shell::marshal(&grant)` writes the image and `SpawnGrant`.
- `inherit.rs` — `Shell::child`, the one literal for thread children.
- `host.rs` — host accessors, probe counts (`binding_count`,
  `leased_binding_count`), `group_owner`.
- `scope.rs` — handler install/vet/remove, `set_var`, `note_define`,
  `schemes_over`.
- `checks.rs` — the [[map/core/capabilities|authority adapter]]: judged path
  lookups, `with_layer` (pushes, then removes its own layers by position with
  `GrantStack::remove`), `push_session_capabilities`, sandbox projection.
- `cwd.rs` — the cwd cell: `enter_cwd` / `restore_cwd` for `within [dir:]`,
  `seed_cwd` for an in-process front end.
- `hooks.rs` — the hook table: named run roots (block, lambda or builtin),
  `vet_hook` / `register_hook`, plugin ledger, dispatched by `Program::Hook`
  under each hook's `DefaultPolicy`.
- `context.rs`, `json.rs`, `notice.rs`, `workers.rs`, `bindings.rs`.

Startup seeding is `shell::boot::seed_host_env`: env overrides
([[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]) and
`OS_NAME` / `OS_ARCH` / `OS_FAMILY`.

## Children

**Thread children share the session's memory and are built by `Shell::child`;
a new session crosses `marshal` / `child_of`.** Exhaustive literals keep a new
field from reaching either by default.

- **Same-thread β-step.** Forcing a block or applying a lambda runs the body in
  the caller's `Shell` under the caller's `&Mooring`; nothing is copied, so an
  unbracketed `cd` persists. `within [dir:]` restores the cwd
  ([[map/core/evaluator|evaluator]]).
- **Thread children** inherit env, context, Σ, runtime, sources, root file,
  call site, exit hints, scratch and fixings, and hold no hooks, no terminal
  lease and an unarmed ledger. They differ in their `Holding`:
  - `spawn_worker` — empty stdin, not interactive, no group; shared root and
    roster; the body replaces the inherited cradle with its own;
  - `spawn_stage` — the pipeline's `Io` and group; shared root, roster and
    cradle. Both go through `spawn_thread`, with `home` minted by the caller
    via `Mooring::for_thread`;
  - `join_session` — an aside: shares root and anchor, writes where the parent
    does, reads nothing, and holds a fresh roster with the parent's retention.
- **New session.** `Shell::child_of(runtime, &parent, narrow)` reads the
  marshalled image, constructs through `Shell::construct` with no root recipe,
  then pushes the grant layer against the child's cwd.
  - It inherits bindings with schemes, handlers, context, sources, terms and a
    fixings snapshot; it owns a fresh durable root and roster, no hooks, no
    terminal lease, and seals every inherited name as lease baseline.
  - Handles cross as titles and arrive withheld; held syntax, sharing,
    residual arguments survive. A non-Unicode cwd
    refuses writing; a builtin the receiver lacks refuses reading
    ([[map/core/transport|transport]], [[map/exarch/agent|agent]]).
