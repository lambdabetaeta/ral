---
generated_at_commit: 446e3123
generated_at_date: 2026-10-05
covers_paths: [core/src/builtins/, core/src/builtins.rs, core/src/uutils.rs]
---

# Map: core / builtins

`core/src/builtins/` are the primitives implemented in Rust that run inside the
shell process. `builtins.rs` holds the `builtin_registry!` macro: each entry
binds its facets at once — `names`, [[map/core/typecheck|type rule]] (`ty`),
`doc` line, and runtime body (`call`) — into the `CORE_BUILTINS` static
(`&[BuiltinEntry]`), so the facets cannot drift apart. A `BuiltinEntry` is a
`Decl` (everything the checker may know: name, `Convention`, doc, type rule,
diagnostic, a `boundary` flag) plus the body only the runtime runs; its
constructors set `boundary` from the body they are handed, so the flag and the
body cannot disagree. Arity is no facet: `Decl::fixed_arity` derives it from the
type rule and caches it with the settle-at-`Unit` fact in one `Spine`, a `usize`
for every entry in the table. What a row does with stdout is
in its scheme: a writing row (the encoders, `echo`, `help`, `explain`, `clear`,
`reset`, `ints-to-bytes`) ends in `Command`, `F^w Unit`, and there is no
separate declaration
([[decisions/260930_graded-f|graded-f]]). Settling at `Unit` is enforced at the
same door as arity:
`Decl::settles_at_unit` reads the declared result off that curry spine,
and a row whose scheme says `Unit` answers unit from `call_body`
whatever its Rust body computed, so no builtin
can hand a value of another type to a name the checker believes is `Unit`.
The manifest is *authored as two*, and
that authoring — not the arity — is the classification: a table entry seeds a
`Value::Native` in the base scope, a base-frame row seeds a base handler frame
(`native_value`, `seed_natives_and_base` in `types/shell/host.rs`;
[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]],
[[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).
A body is a `BuiltinBody` — a `Static` `fn(&[Value], &Mooring, &mut Shell) ->
Settled<Value>`, or a `Captured` closure of the same shape for a host frontend
with state to carry — so the run's mooring arrives beside the shell
([[map/core/shell-state|shell-state]]): it is how a body surfaces an event,
enquires, or starts a nested run parented under the run that called it. The
type-rule facet is a `BuiltinTypeRule`, which is a scheme factory —
`fn(&mut Unifier) -> Scheme` — and nothing else. The streaming reducer
`fold-lines` is an ordinary one whose factory (`scheme::fold_lines`) is a
plain scheme ([[map/core/typecheck|typecheck]]);
there is no separate reducer arm. Beside it sits an optional diagnostic facet,
which is not a typing rule: it carries what a misuse this verb has a name for
earns — a decoder handed an argument, `fail` handed a literal zero status
([[internals/builtins-registry|builtins-registry]]). An entry's first-class
form is its scheme, so it has one by construction, every entry in the table
having declared its arguments ([[invariants/fixed-arity|fixed-arity]]).
Builtins are *shell-scoped*: each shell's session carries a `BuiltinTable`
([[map/core/shell-state|shell-state]]) seeded from `CORE_SETS`
(the core rows, the boundaries and the base frames, one list), and a host's
extra sets ride a `HostSurface` into `HostSurface::shell` and `boot::boot_shell`
(`core/src/boot.rs`), so the checker's rule table, the base
scope, and the base frames all come from the manifest the shell was booted with
— there is no process-global registry, and every path that builds or hydrates a
shell must seed through `install_builtins` or re-link a native by name.
`register` clones the baked prelude's bindings into each fresh environment.
`BOUNDARY_BUILTINS` (`from-json`, `from-jsonl`, `from-json-at`, `use`) also sits
outside it, for a different reason: each row is a `BuiltinEntry::boundary`, whose
body takes the `Site` the checker solved at the call and admits the value it
lets in against it ([[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]]).
Four entries sit *outside* the macro, implemented in core but installed by a
host. Two are a pair with the hosts swapped: the public `WATCH_BUILTIN`
(`&[BuiltinEntry]`) wraps the still-private `concurrency::builtin_watch` /
`scheme::watch` — a watched worker's lines leave as `` `watch [label, line] ``
surfaces through the session's deferred sink (`Sink::Watch`), which outlives
the run — so only a host with a deferred sink installs it, and elsewhere
`watch` is an unknown-name diagnostic rather than a runtime refusal
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]); its mirror
`SERVICE_BUILTIN` wraps `concurrency::builtin_service` / `scheme::service` so
the agent host (exarch), whose lease frame reaps ordinary workers, installs
the durable-birth verb while the ral hosts — which grant no lease, so every
spawn of theirs already lives until cancel or exit — omit it. The third,
`DETACH_BUILTIN` (`cfg(unix)`, over `concurrency::builtin_detach`), is the
base-frame manifest's second row — typed `List String -> F Any`, no arity to it
— and is carried by a host that arms a detach policy: installing the verb and
arming the budget (`Shell::arm_detach`) are one act, so absence is an
unknown-name diagnostic rather than a veto, while
whether a given call may spend it is the live grant stack's question
(`GrantStack::permits(Flag::Detach)`). The fourth, `SURFACE_BUILTIN`, wraps
`misc::builtin_surface` / `scheme::surface_op` under the bare name `surface`;
exarch declares its own entry over the same body under `exarch-surface`, with
its own doc, so no host's name enters core's vocabulary. `builtin_surface` is
`pub` for exactly this — the one core body two hosts each name their own way.

Bodies are grouped by concern, one submodule each:

- `strings.rs` — string and regex primitives, including `lines` (`String ->
  [String]`), the same split as `from-lines` run over a string's bytes rather
  than the channel, sharing `util::read_lines`/`lossy_line_list`. `dedent`
  owns the raw-block framing rule: blank lines around a multiline block fall
  away before the common margin is stripped, while content-line whitespace is
  preserved;
- `collections.rs`, `predicates.rs` (`keys`, `has`: ordering and equality are
  `Value::compare` / `Value::equals`, and `equal`, `lt`, `gt`, `is-empty`,
  `sort-list`, `identity` are prelude one-liners over `==`, `<`, `>`, `length`
  and `sort-list-by`), `fs.rs`, `codecs.rs` — the last is also
  home to `builtin_echo`, `to-line`'s neighbour by nature: every argument
  rendered through the total `to-string` — `Value`'s `Display`, mapped over the
  argv — single-space intercalation, a newline to stdout.
  `write_encoded` (`codecs.rs`) writes its bytes to stdout and returns
  `Value::Unit`, so `to-csv`, `to-bytes`, `ints-to-bytes`, `to-string`,
  `to-lines`, `to-json`, and `to-jsonl` are writers: each types
  `A → Command` at its own operand type, and its encoded bytes are what it
  writes. `to-bytes` takes `Bytes` and `ints-to-bytes` takes `[Int]` —
  two names, no union in the table. `builtin_from_jsonl` reads through
  `util::stdin_lines` and parses each line alone, so an error names the input's
  line. Each JSON direction has one typed refusal, worded by each codec in its
  own terms: `json_to_value`'s `OutOfRange`, `value_to_json`'s
  `Unrepresentable`, which `to-jsonl` prefixes with the element's index;
- `shell.rs` — `cd`, `alias` / `unalias`;
- `concurrency.rs` — `spawn` / `watch` / `service` / `detach` and the handle
  verbs
  `await` / `poll` / `race` / `cancel`, split by concern: the root keeps the
  verbs of birth, `concurrency/birth.rs` the `Birth` door (`spawn_child`),
  `concurrency/eliminate.rs` the eliminators, `concurrency/tests.rs` the
  tests; the worker's deferred surface and its `FlushGuard` live in
  `types/handle/surface.rs`, and the `joined` deliver-once cell is a
  `Latch` whose `claim` one side wins (builtins under their bare names; `par`
  and the `is-done` predicate are prelude code over them, not builtins). All
  but the host-installed three seed through `CORE_BUILTINS`; those live here
  too but reach a session via `WATCH_BUILTIN` / `SERVICE_BUILTIN` /
  `DETACH_BUILTIN`, not core. `builtin_detach` is the surface discipline
  alone — the birth itself is the ordinary external-command machinery down to
  the double-fork in `runtime/command/detach.rs`
  ([[map/core/runtime|runtime]]), and it yields a `{pid, desc}` receipt, not a
  `Handle`, so none of the eliminators below apply to it. On completion a
  block's buffers drain *once* into a cached `CompletedHandle { stdout, stderr,
  outcome }` ([[map/core/shell-state|types/value.rs]]); the eliminators project that
  one settle. `HandleInner::try_settle` is the shared non-blocking sample (cached
  outcome, else a `try_recv` drained into the cache; a `Disconnected` receiver — a
  panicked worker — settles as a failure naming the worker rather than whichever
  eliminator found it, so `poll`/`race` see a finished block rather than
  spinning).  A handle's `state` is its transition lock, and `types/handle.rs`
  seals it: `StateCell` hands out no guard, so the worker's exit mark
  (`complete`), settling (`try_settle`) and stopping (`detach`, under
  `stop_handle`) are the only transitions, each holding the lock across both
  the test and the transition, taking `result` and `cached` under it and never
  the other way round.  That is what makes the promise good that a finished
  worker's value is never destroyed by a losing `race` or a `cancel` — a worker
  that completes cannot slip between `detach`'s test and its transition,
  because there is no window between them — and what keeps a reader from
  holding `state` across a registry call, the one order the registry's own lock
  documents. `await`/`race` `project_completed` the
  outcome to `{value, stdout, stderr}`, re-raising `` `err ``; `poll` is total,
  wrapping it as `` `settled `` `{stdout, stderr, outcome: `ok/`err}` (the `` `err ``
  payload built through `Error::record`, the record
  `try` hands its handler) or `` `pending `` `{stdout, stderr}` (a *cumulative,
  non-destructive* `CapturedBytes::peek` snapshot of the running worker's output — the
  buffers are left for the one-shot completion `CapturedBytes::take`, so a partial poll
  never steals bytes) — the block's outcome is data, not a status. `await`
  and `poll` gate first on `ensure_live`, the cancelled pre-check
  ([[decisions/260615_poll-total-failed-arm|the settle decision]],
  [[decisions/260702_partial-poll-pending-output|partial-poll-pending-output]]).
  A detached worker hangs under the durable session root, not the run's
  foreground scope, so a foreground cancel never reaps it; `await` shares
  `race`'s cancel-aware wait loop (`wait_first_settled`), so a deadline unwinds
  the wait while the root-scoped worker survives
  ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).
  Under a frame that grants a `WorkerLease`, `spawn` fires a `LeaseChain`
  (`types/shell/workers.rs`) — a self-re-arming `process::deadline` callback
  deciding by `WorkerLease::verdict`, a pure function of the worker's age and
  idleness, so the first delay is already the sooner bound — the idle-observation lease chain: a
  still-running worker unobserved for `idle` is reaped, where every `poll`
  and every `await`/`race` sweep renews the handle's `last_observed` cell,
  under an absolute `backstop` no polling extends; a worker that finished
  ends the chain silently, its entry lingering as an unclaimed result. A
  reap removes the registry entry, records a `ReapNotice` the engine
  pushes at the next settled run's ready boundary as a `` `notice ``
  surface event (`emit_ready_boundary_notices`; exarch decodes it back via
  `card::value_to_notice`), and cancels the worker's scope with
  `Deadline` — never detaching the handle, so a later `poll`/`await` still
  observes the partial output and failure. The class decides the chain at
  the spawn door: `spawn_child` takes a `Birth` — which of `spawn`, `watch`
  and `service` is being served, and so the verb a refusal names, the lease
  class registered and how the child's bytes are wired — and only a `Worker`
  birth arms it — `service` registers `Durable` and arms nothing, so no
  reaper entry ever exists for it; the absent chain *is* the durable
  policy, whose only bounds are the handle's own `cancel`, the host's
  `/clear`, and process exit. A birth's `cmd` — the name every eliminator's
  error, the trail, and the workers listing use — comes from the birth
  itself: a watch's label, a service's description, and for a plain `spawn`
  or a prelude `defer` over it, `shell.call_site()` — `block at <script>,
  line N`.
  The spawn door also enforces the frame's admission cap
  (`Mooring::worker_cap`): a birth of any class *reserves* its seat at
  the door (`WorkerRegistry::reserve`) — refused while `cap` workers are
  running or reserved, with an error naming the verb the caller actually
  wrote and `await`/`cancel` as the remedies, the reservation held across thread spawn and released into the
  registered entry, so a racing sibling birth never sees a filling seat as
  free (there is no `workers` listing — [[map/exarch/builtins|builtins]]); settled
  entries lingering under retention hold no seat. A
  settled entry's own lease is retention, armed once at boot
  (`Shell::arm_worker_retention`, beside the binding lease): the registry
  keeps its own clock — one `tick_epoch` per source dispatch — and the
  engine sweeps at each settled run's ready boundary (`sweep_retention`,
  engine housekeeping), stamping an entry at the first sweep that
  observes it settled and expiring it — a `Retention`-cause `ReapNotice` on
  the same drain — once its unclaimed result has sat stamped a full
  retention of ral calls. An unarmed registry (the REPL) retains settled
  entries indefinitely; the eliminators still remove entries the moment a
  result is claimed, so the sweep only catches what nobody claimed.
  A worker runs its thunk on a fresh
  `std::thread` via `Shell::spawn_thread` ([[map/core/shell-state|shell-state]]),
  seeded from the parent's session; `worker_body` hands the thunk, its capture
  riding in its closure, straight to `machine::force`
  ([[map/core/evaluator|evaluator]]), deliberately
  bypassing the `Toplevel`/`Phrase` boundary, because the worker's own
  `Shell` is the only one its bindings touch and they die with the thread. The
  worker carries the parent's grant stack, so a forced block *inside* the worker
  still meets the standard boundary rule and any external child it spawns is
  confined per-command — a `spawn` under a `grant` cannot escape it. Every
  `spawn_child` also files the freshly-minted handle on `shell.local.workers`
  — a per-shell registry, host-independent and carrying no policy
  ([[map/core/shell-state|shell-state]]); `await`, `race`'s winner and
  its cancelled losers, and a settled `poll` remove the entry from whichever
  shell observes it, an explicit `cancel` removes it too, and a pending
  `poll` or a bare listing never touches the registry. A `Handle` is
  process-local: `Shell::fork_scrubbed` scrubs it from every fork, and a run
  whose result is or holds one ends `Unreturnable` (`core/src/run/report.rs`);
- `modules.rs` — the cacheless `use` loader (a boundary: it builds the export
  record and admits it with `Site::admit_module`, holding each function to the
  scheme the module was checked at). It runs its phrases through
  `load::module_phrases`, the cycle stack and depth bound it shares with the
  host loading door `load::evaluate_source` (rank 10, `core/src/load.rs`,
  which also holds `check_source`, the compile door both share: it checks
  against the live session, registering the source first so the module's
  spans carry its real identity, and hands back a failure
  as an `Error` carrying its `Rejection`
  ([[map/core/diagnostics|diagnostics]])); `use` is a scope-projecting caller
  of that door, running under the session environment rather than the
  caller's own block-local scope. Module loads carry no cache, so the guards
  keep re-evaluation terminating — see
  [[decisions/260606_cacheless-module-loader|cacheless-module-loader]];
- `misc.rs` — including `builtin_surface`, the body `SURFACE_BUILTIN` (above)
  wraps: it forwards a tagged variant to the host's
  [[map/core/shell-state|`SurfaceSink`]] and is the identity under a bare REPL;
- `math.rs` — the Float rounding builtins (`round`, `floor`, `ceil`, `trunc`);
- `help.rs` — `help` (arity-0 command index) and `explain <name>` lookup, both
  reflecting on the session — its scope and the shell's registries, never the
  caller's block. One `Where` answers both halves of an entry: the line naming
  the frame that would run, and the doc ladder that asks *that* registry — a
  session binding owning its name outright rather than inheriting the doc of
  what it shadows. `locate_all` returns the whole resolution chain, so
  `explain` also names what a name shadows, a PATH binary included, which is
  the question `which` answers wrongly for every name ral provides;
- `print.rs` — the value pretty-printer shared by the REPL and exarch's
  tool-result rendering (`PrintParams`; a rendering utility, not a registered
  builtin). One printer, one policy, per-reader numbers: truncation preserves
  identity — the depth limit summarises (keys and heads) and only the floor
  beneath it counts, a string elides unless it *is* the whole value, and a byte
  budget is spent inside each container, which closes with `…N more`. An elision
  must earn its marker: a string prints whole unless cutting it actually
  shortens the rendering, so nothing is mutilated to save a character. The two
  readers differ in window, quote fence, and absorbable bytes — and in whether a
  nested string is capped at all: the REPL cuts at a terminal row, exarch's
  `VALUE` section cuts nothing, since a payload's text is the identity a later
  `edit-hash` matches;
- `util.rs` — shared helpers. `read_lines` is the one line reader
  ([[design/codecs|codecs]]' line rule), generic over its byte source and
  leaving each line undecoded, since decoding is each caller's policy;
  `stdin_lines` runs it over `stdin_reader`, and `lossy_line_list` decodes
  its lines into the `[String]` that `from-lines` and `lines` return.

The capability `Value`-map decoder is *not* a builtin: it lives beside the
authority layer in `guard/decode.rs` (`decode_capability_map`), consumed by
the `grant` control operator (`evaluator/scope.rs`) and the `--capabilities`
ceiling (`load/profile.rs`) — see [[map/core/capabilities|capabilities]],
[[design/grant|grant]].

Why a capability lands in one of these layers rather than another — builtin vs.
coreutil vs. prelude vs. control operator — is [[design/name-resolution|design: name-resolution]];
what a builtin *is* and the shape of the set is [[design/builtins|design: builtins]];
the `from-X`/`to-X` byte↔value typing in `codecs.rs` is [[design/codecs|design: codecs]].

## Bundled coreutils, diffutils, and ripgrep

`core/src/uutils.rs` — a top-level module, since every consumer is exec-side and
the manifest module holds manifest things only — declares the bundled tools as
three feature-gated families and the predicate and dispatch that unify them.

- **coreutils** — `declare_coreutils!` takes two parallel lists: `cross`
  (always on under the `coreutils` feature) and `unix` (additionally under
  `coreutils-unix-only`, `cfg(unix)`-gated). It emits one merged
  `COREUTILS_TOOLS` slice, a `coreutils_invoke` arm, and the
  platform-unconditional `COREUTILS_UNIX_ONLY_TOOLS` list — the one
  authoritative spelling of the `unix` names, so a caller that must know a
  bundled name does not exist off-Unix (a profile loader dropping dead exec
  grants) reads this list rather than keeping a second copy.
- **diffutils** — `DIFFUTILS_TOOLS` (`["cmp", "diff"]`, `diffutils` feature),
  whose `cmp_main` / `diff_main` shims faithfully translate the upstream
  `diffutilslib` entrypoints (re-audit on a version bump).
- **ripgrep** — `RIPGREP_TOOLS` (`["rg"]`, `ripgrep` feature), routed through
  `ral-ripgrep-core::run_cli` by `rg_main` (which drops the argv[0] slot).

`uutils_invoke` is the bare dispatch over all three families (diffutils and
ripgrep matched ahead of the coreutils fall-through, each arm feature-gated);
`is_uutils_tool` is the membership predicate. These bundled heads share the
capability chokepoint with every other command — part of why ral is a
[[invariants/single-binary|single-binary]]. The `grep` cargo feature separately
backs the `re-*` regex string builtins.

A bundled head is a resolved command *image*, not a builtin in `CORE_BUILTINS`:
it is always an ordinary `ral --ral-bundled-tool <tool>` child carrying process
semantics ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
[[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]]).
That dispatch — the `Program::Tool` placement and the hidden
entrypoint — is the [[map/core/runtime|runtime]]'s; this page owns only the
registry of names, shims, and the in-binary `uutils_invoke` they converge on.
`docs/SPEC.md` §14.7 covers the single-binary tool surface.
