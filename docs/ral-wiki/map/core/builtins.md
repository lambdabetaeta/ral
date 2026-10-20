---
generated_at_commit: 3bf43d16fc35212e3a3b354a91ffa2fcac136724
generated_at_date: 2026-10-19
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
(`native_value`, `Runtime::install_set` in `types/shell/construct.rs`;
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
shell must be built over a `Runtime` that carries them or re-link a native by name.
`register` clones the baked prelude's bindings into each fresh environment.
`BOUNDARY_BUILTINS` (`from-json`, `from-jsonl`, `from-json-at`, `use`, `worker`) also sits
outside it, for a different reason: each row is a `BuiltinEntry::boundary`, whose
body takes the `Site` the checker solved at the call and admits the value it
lets in against it ([[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]]).
Three entries sit *outside* the macro, implemented in core but installed by a
host. The public `WATCH_BUILTIN`
(`&[BuiltinEntry]`) wraps the still-private `concurrency::builtin_watch` /
`scheme::watch` — a watched worker's lines leave as `` `watch [label, line] ``
surfaces through the session's deferred sink (`Sink::Watch`), which outlives
the run — so only a host with a deferred sink installs it, and elsewhere
`watch` is an unknown-name diagnostic rather than a runtime refusal
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]). The second,
`DETACH_BUILTIN` (`cfg(unix)`, over `concurrency::builtin_detach`), is the
base-frame manifest's second row — typed `List String -> F Any`, no arity to it
— and is carried by a host that arms a detach policy: installing the verb and
arming the budget (`Shell::arm_detach`) are one act, so absence is an
unknown-name diagnostic rather than a veto, while
whether a given call may spend it is the live grant stack's question
(`GrantStack::permits(Flag::Detach)`). The third, `SURFACE_BUILTIN`, wraps
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
  `Value::compare` / `Value::equals`, and `lt`, `gt`, `is-empty`,
  `sort-list`, `identity` are prelude one-liners over `<`, `>`, `length`
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
- `concurrency.rs` — the `spawn` form's door (`spawn_form`), `watch` /
  `detach`, the handle verbs
  `await` / `poll` / `race` / `cancel` / `worker`, and the `workers` listing
  (`builtin_workers`), split by concern: the root keeps the
  verbs of birth, `concurrency/birth.rs` the `Birth` door (`spawn_child`),
  `concurrency/eliminate.rs` the eliminators and `worker`,
  `concurrency/combinators.rs` the structured fan-out — `par` (`builtin_par`,
  a Rust builtin), the `all` form's door (`all_form`) and the `Litter` they
  share (`race` ends its losers through `end_unclaimed`) — and `concurrency/tests.rs` the tests; the worker's
  deferred surface lives in `types/handle/surface.rs`, its
  cradle and brood in `types/handle/brood.rs` (builtins under their bare
  names; the `is-done` predicate is prelude code over them). `spawn` and
  `all` are control operators, not rows: the machine forms their operands and
  calls the two doors ([[map/core/evaluator|evaluator]]). All
  but `worker` and the host-installed two seed through `CORE_BUILTINS`;
  `worker` seeds through `BOUNDARY_BUILTINS`, and the other two live here
  too but reach a session via `WATCH_BUILTIN` / `DETACH_BUILTIN`, not core. `builtin_detach` is the surface discipline
  alone — the birth itself is the ordinary external-command machinery down to
  the double-fork in `runtime/command/detach.rs`
  ([[map/core/runtime|runtime]]), and it yields a `{pid, desc}` receipt, not a
  `Handle`, so none of the eliminators below apply to it. A handle is one
  private lock over a `HandleState`: `Running`, `Settled(CompletedHandle {
  stdout, stderr, end })`, or `Claimed`. `HandleInner::new` returns the handle
  with its one `Settler`, which the worker thread owns: `Settler::settle`
  flushes the worker's deferred surface, then `complete` drains the buffers
  into `Settled` (or drops the end of a handle already `Claimed`) and sets the
  end `Stamp`; a `Settler` dropped unsettled — a panic — settles the handle
  as a panic naming the worker, so `poll`/`race` see a finished block rather
  than spinning. Observers read a `Sample` — `Pending`, `Settled(R)` or
  `Claimed` — and the only transitions are `complete`, `observe` (a read that
  lends the result to a closure under the lock), `claim_settled` (moves a
  settled result out and leaves `Claimed`, in one step) and `claim_and_stop`
  (the only way from `Running` to `Claimed`: it fires the worker's node, and
  discards a settled result). So a result is *claimed* exactly once, by
  construction. The registry lock may take a handle's lock, never the
  reverse. `await` is exact: `join` waits (`claim_first`) until it claims the
  handle's settled result or finds it claimed by another; `exact` `replay`s the
  stdout bytes, then the stderr bytes, into the awaiter's own sinks and
  `conclude`s as the block ended — the value (admitted by `admit_handled`),
  the failure as it stands, an `exit` or a stop re-raised, a panic resumed —
  so `await` of a block is that block run there. A handle another claimer has
  fails `await_handle` with `<verb>: this worker's result was already
  claimed`, an ordinary error, and `join` returns `None` for it, which a
  worker's return rule skips. `race` claims the first handle it finds
  settled, in list order, skipping any another caller claimed, and fails as
  `await` does only once every handle is claimed, at entry or mid-wait; it
  then claims, stops and drains its losers before it returns. An empty list
  is its own error.
  `cancel` claims the handle and stops it, and is nothing on one already
  claimed. The handle carries its block's grade (`Ty::Handle(Grade, Ty)`,
  printed `Handle (Returns α)` / `Handle Command`), so `await` returns the
  grade the block had. `poll` never writes and never claims, reporting the
  settle as `` `settled `` `{stdout, stderr, elapsed, outcome: `ok/`err}` (the
  `` `err `` payload built through `Error::record`, the record `try` hands its
  handler) or `` `pending `` `{stdout, stderr, elapsed}` (a *cumulative,
  non-destructive* `CapturedBytes::peek` snapshot of the running worker's
  output, peeked before the handle is observed — the buffers are left for the
  worker's one-shot `CapturedBytes::take` as it settles, so a partial poll
  never steals bytes) — the block's
  outcome is data, not a status. A worker stopped from elsewhere settles with
  its own `WorkerStopped` record, an `exit`, a foreign stop or a panic is
  re-raised, as `await` re-raises them: `poll` is total on an unclaimed
  handle, and a handle `cancel` or `await` claimed fails as it does for
  `await`
  ([[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]],
  [[decisions/260615_poll-total-failed-arm|the settle decision]],
  [[decisions/260702_partial-poll-pending-output|partial-poll-pending-output]]).
  A `spawn` or `watch` worker hangs under its shell's cradle, not the run's
  foreground scope: under the durable session root in a run, so a foreground
  cancel never reaps it, and under its parent worker's node inside a worker,
  whose base settles it (`Brood`: each child it claims awaited on return,
  cancelled and drained on a halt, a child another claimer has skipped), so
  `cancel` of a worker reaches its tree. A `par` or `all` child hangs under
  the call's current scope and is the call's to end (`Litter`). `await` shares
  `race`'s cancel-aware wait loop (`claim_first`), so a deadline unwinds
  the wait while a session worker survives
  ([[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]).
  The spawn door is `spawn_child`, which takes a `Birth` — `Spawn`, `Watch` or
  a calling expression's `Call` — and so the verb a refusal names, where the
  child's node hangs, its `Owner`, whether its birth is announced, and how its
  bytes are wired: a `Call` child writes to the call's live sink, as a
  pipeline stage does, and flushes no `` `done ``. It mints the worker's
  `WorkerId` first and builds the handle around it: `HandleInner` carries the
  `id`, the `title`, and the worker's clock, a birth `Stamp` and an end
  `Stamp` that its `Settler` sets on every exit path of the worker thread, from
  which `elapsed` and `settled_at` are derived, each in one place. The
  registry entry adds only the `Owner`. A birth's `title` — the name every
  eliminator's error, the trail, and the completion event use — comes from the
  birth itself: a `spawn`'s title (non-empty and one line, `legible_title`),
  else `shell.call_site()` — `block at <script>, line N` — which the run's
  `TitlePolicy` may refuse before any birth; a watch's label; a `par` child's
  `par #i`, an `all` child's label. The completion event (`DoneEvent`) carries
  the handle's id, title and elapsed time beside its outcome, including on
  failure and panic. No policy reaps a running worker: it ends by completing,
  by `cancel`, or with its session. `worker <id>` (`builtin_worker`, a
  boundary row) takes back the handle of any worker still in the registry,
  running or settled, by that id; the value it settles with is admitted at the
  `await` site (`admit_handled`). The spawn door also enforces the frame's
  admission cap (`Mooring::worker_cap`): a birth *reserves* its seat at the
  door (`WorkerRegistry::reserve`) — refused while `cap` workers are running
  or reserved, with an error naming the verb the caller actually wrote and
  `await`/`cancel` as the remedies, the reservation held across thread spawn
  and released into the registered entry, so a racing sibling birth never sees
  a filling seat as free; settled entries lingering under retention hold no
  seat. A settled entry's one bound is retention, armed once at boot
  (`Shell::arm_worker_retention`, beside the binding lease): the registry
  keeps its own clock — one `tick_epoch` per source dispatch — and the engine
  sweeps at each settled run's ready boundary (`sweep_retention`, engine
  housekeeping), stamping an entry at the first sweep that observes it settled
  and expiring it once its unclaimed result has sat stamped a full retention
  of ral calls. The expiry records a `Retention`-cause `ReapNotice`, which the
  engine pushes at the next settled run's ready boundary as a `` `notice ``
  surface event (`emit_ready_boundary_notices`; exarch decodes it back via
  `card::value_to_notice`). An unarmed registry (the REPL) retains settled
  entries indefinitely; a claimer still removes an entry the moment it claims
  the result, so the sweep only catches what nobody claimed. A worker runs its
  thunk on a fresh `std::thread` via `Shell::spawn_thread`
  ([[map/core/shell-state|shell-state]]), seeded from the parent's session;
  `worker_body` hands the thunk, its capture riding in its closure, straight
  to `machine::force` ([[map/core/evaluator|evaluator]]), deliberately
  bypassing the `Toplevel`/`Phrase` boundary, because the worker's own `Shell`
  is the only one its bindings touch and they die with the thread. The worker
  carries the parent's grant stack, so a forced block *inside* the worker
  still meets the standard boundary rule and any external child it spawns is
  confined per-command — a `spawn` under a `grant` cannot escape it. Every
  `spawn_child` also files the freshly-minted handle on `shell.local.workers`
  — a per-shell registry, host-independent and carrying no policy
  ([[map/core/shell-state|shell-state]]); `await`, `race`'s winner and its
  cancelled losers, `cancel`, and a brood's or litter's end claim the handle
  (so no other brood or litter answers for it) and remove the entry from
  whichever shell claims it, and `poll`, on either arm, or a `workers` listing
  never touches the registry. **A listing is data; `worker <id>` is the
  door.** `workers` maps every entry, running or retained, ids ascending,
  through `worker_row` (`types/shell/host.rs`, shared with the `` `workers ``
  probe) to a seven-field record — `id`, `title`, `owner` (`` `session ``, ``
  `worker id ``, `` `call ``), `state` (`` `running `` or `` `done `` with the
  `Done` ending), `started` (Unix seconds), `elapsed` (seconds, off the
  handle's `Stamp`s), `settled` (`` `some `` Unix seconds or `` `none ``) —
  reading the state with `settled`, which caches but claims nothing, and
  omitting a handle claimed since the entry was read (`WorkerRow::ral_value`;
  the probe's eighth field, `retention-left`, is the host's alone). It holds
  no handle, so a top-level `workers` crosses the engine protocol. A `Handle`
  is process-local: `Shell::bequeath` scrubs it from every fork, and a run
  whose result is or holds one ends `Unreturnable` (`core/src/run/report.rs`);
- `modules.rs` — the cacheless `use` loader (a boundary: it builds the export
  record and admits it with `Site::admit_module`, holding each function to the
  scheme the module was checked at). It runs its phrases through
  `load::module_phrases`, the cycle stack and depth bound it shares with the
  host loading door `load::evaluate_source` (rank 10, `core/src/load.rs`,
  which also holds `check_source`, the compile door both share: it checks
  against the live session, registering the source first so the module's
  spans carry its real identity, and hands back a failure as an `Error`
  carrying its `Rejection` ([[map/core/diagnostics|diagnostics]])); `use` is a
  scope-projecting caller of that door, running under the session environment
  rather than the caller's own block-local scope. Module loads carry no cache,
  so the guards keep re-evaluation terminating — see
  [[decisions/260606_cacheless-module-loader|cacheless-module-loader]];
- `misc.rs` — including `builtin_surface`, the body `SURFACE_BUILTIN` (above)
  wraps: it forwards a tagged variant to the host's
  [[map/core/shell-state|`SurfaceSink`]] and is the identity under a bare REPL;
- `math.rs` — the Float rounding builtins (`round`, `floor`, `ceil`, `trunc`);
- `help.rs` — `help` (arity-0 language and command index) and `explain <name>`
  lookup, both reflecting on the session — its scope and the shell's
  registries, never the caller's block. `LANGUAGE_DOCS` gives every parser
  keyword and Boolean literal a standalone guide; a completeness test pairs
  that table with `CONTROL_OPERATORS`, `FLOW_KEYWORDS`, and
  `LANGUAGE_CONSTANTS`, so new syntax cannot land undocumented. `LANGUAGE_GUIDES`
  adds reference topics to the same lookup and index, including `redirects`.
  `within` owns handler and option restrictions, and `return` covers host
  configuration records; `try` details the error record's optional source
  location. The `grant` guide owns the capability schema and examples of exclusions and computed
  paths; the agent prompt points here for those details. One `Where`
  answers both halves of a resolved command entry: the line naming the frame
  that would run, and the doc ladder that asks *that* registry — a session
  binding owning its name outright rather than inheriting the doc of what it
  shadows. The shared renderer separates a doc's conventional `usage  — prose`
  opening and labels `Usage:`, `Type:`, `Source:`, and `Shadows:`; absent facts
  are omitted. `locate_all` returns the whole resolution chain, so `explain`
  also names what a name shadows, a PATH binary included, which is the
  question `which` answers wrongly for every name ral provides;
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
  `edit-hash` matches. One layout rule belongs to the printer, not to any
  value: a list of two or more non-empty records sharing one key sequence
  prints one record per line, each `label: value` cell padded to its column's
  widest, still pasteable surface syntax (`rows`); a cell that would break a
  line, or a list past the depth limit, keeps the ordinary layout. So a
  `workers` result reads as a table in the REPL and in exarch's `VALUE`
  alike, and no worker-specific code renders anything;
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
