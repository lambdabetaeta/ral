---
generated_at_commit: f9ac94f8
generated_at_date: 2026-11-12
covers_paths: [core/src/shell/builtins/, core/src/shell/builtins.rs, core/src/uutils.rs]
---

# Map: core / shell / builtins

`core/src/shell/builtins/` are the primitives implemented in Rust that run inside the
shell process. `shell/builtins.rs` holds the `builtin_registry!` macro: each entry
binds its facets at once — `names`, [[map/core/typecheck|type rule]] (`ty`),
`doc` line, and runtime body (`call`) — into the `CORE_BUILTINS` static
(`&[BuiltinEntry]`), so the facets cannot drift apart. A `BuiltinEntry` is a
`Decl` (everything the checker may know: name, `Convention`, doc,
diagnostic, a `boundary` flag) plus the body only the runtime runs.
`Convention::Value(rule)` carries the type rule; `Convention::Argv` carries
none and fixes `[String] → Command` through `Arm::ty`, shared with named
handlers. The `handlers_ty` map member uses that same constant. Its
constructors set `boundary` from the body they are handed, so the flag and the
body cannot disagree. Application arity is no facet: `Decl::fixed_arity`
derives it from a value row's closed scheme, caching it with the
settle-at-`Unit` fact in one `Spine`. Argv rows have no application arity.
What a row does with stdout is in its scheme: a writing row (the encoders, `echo`, `help`, `explain`, `clear`,
`reset`, `ints-to-bytes`) ends in `Command`, `F^w Unit`, and there is no
separate output facet. `Decl::scheme` gives ordinary results fresh grades,
while boundaries retain `p` and already graded results retain their grade
([[design/capture|grades and capture]]). Settling at `Unit` is enforced at the
same door as arity:
`Decl::settles_at_unit` reads the declared result off that curry spine,
and a row whose scheme says `Unit` answers unit from `call_body`
whatever its Rust body computed, so no builtin
can hand a value of another type to a name the checker believes is `Unit`.
The manifest is *authored as two*, and
that authoring — not the arity — is the classification: a table entry seeds a
`ValClos::builtin` in Σ, a base-frame row seeds a base handler frame
(`Runtime::install_set` in `shell/runtime/state/construct.rs`;
[[design/builtins|builtins]], [[design/exec|exec]]).
A body is a `BuiltinBody` — a `Static` `fn(&[ValClos], &Mooring, &mut Shell) ->
Settled<ValClos>`, or a `Captured` closure of the same shape for a host frontend
with state to carry — so the run's mooring arrives beside the shell
([[map/core/shell-state|shell-state]]): it is how a body surfaces an event,
enquires, or starts a nested run parented under the run that called it. The
value convention's type rule is a `BuiltinTypeRule`, a scheme factory —
`fn(&mut Binders) -> Ty`, closed by `Binders::close` — and nothing else. The streaming reducer
`fold-lines` is an ordinary one whose factory (`scheme::fold_lines`) is a
plain scheme ([[map/core/typecheck|typecheck]]);
there is no separate reducer arm. Beside it sits an optional diagnostic facet,
which is not a typing rule: it diagnoses a decoder handed an argument
([[internals/builtins-registry|builtins-registry]]). A caught record handed to
`fail` instead earns generic shape guidance from `shape_hint` in
`typecheck/explain.rs`: it recognises the closed `ErrorRecord::ty()` shape by
field labels and types, including `site`, regardless of row order, and points
to `raise $e` or `$e[message]`.
An entry's first-class type
is its scheme, so it has one by construction, every entry in the table
having declared its arguments ([[invariants/fixed-arity|fixed-arity]]).
The public `ValClos::builtin` wraps an entry as `⟨Val::Builtin(entry),∅⟩`.
Force enters `BuiltinEntry::eta`, a λ-chain ending in `CompKind::Builtin`
(δ), over the captured entry. β binds supplied arguments; a partial is a boxed
λ residual, not a separate native state. `Machine::call` invokes an entry
at exactly its arity without entering η. Boundary partials retain their
checked site in inference-expanded `Exec` blocks
([[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]]).
Builtins are *shell-scoped*: each shell's session carries a `BuiltinTable`
([[map/core/shell-state|shell-state]]) seeded from `CORE_SETS`
(the core rows, the boundaries and the base frames, one list), and a host's
extra sets ride a `HostSurface` into `HostSurface::shell` and `shell::boot::boot_shell`
(`core/src/shell/boot.rs`), so the checker's rule table, the base
scope, and the base frames all come from the manifest the shell was booted with
— there is no process-global registry, and every path that builds or hydrates a
shell must be built over a `Runtime` that carries them or re-link a native by name.
`register` clones the baked prelude's bindings into each fresh environment.
`BOUNDARY_BUILTINS` (`from-json`, `from-jsonl`, `from-json-at`, `from-csv`,
`env`) also sits
outside it, for a different reason: each row is a `BuiltinEntry::boundary`, whose
body takes the `Site` the checker solved at the call and lets the value in
through `Site::admitted` (`shell/runtime/admit.rs`), which words a mismatch as
the door's refusal ([[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]]).

**Every registered boundary declares a data result.** This is a manifest-wide
invariant, not a property of a hand-written decoder list:

- every type variable in its declared result has `Kind::DATA`; every row
  variable there is deep, requiring data in all added fields and payloads.
  Functions and handles are excluded at every depth;
- core's JSON factories (`scheme::from_json`, `scheme::from_json_at`,
  `scheme::from_jsonl` in `core/src/typecheck/builtins.rs`) also type `env`
  and `from-csv`. Host factories obey the same rule: exarch's three
  `answer_ty` variables in `exarch/src/shell_eval/builtins/harness.rs`, and
  `_ed-state` in `ral/src/repl/plugin/ed_builtins.rs`;
- acceptance tests walk each host's whole manifest: core, ral's REPL and
  batch surfaces, and exarch. Every `BuiltinBody::Boundary` entry must have
  data result variables and deep result row variables, so adding a boundary
  needs no change to a test's list;
- using a boundary result as a function or handle is a kind error at the
  use, before decoding or readmission. Ordinary data-shape mismatches remain
  boundary-admission refusals. See [[design/codecs|codecs]].

Three entries sit *outside* the macro, implemented in core but installed by a
host. The public `WATCH_BUILTIN`
(`&[BuiltinEntry]`) wraps the still-private `concurrency::builtin_watch` /
`scheme::watch` — a watched worker's lines leave as `` `watch [label, line] ``
surfaces through the session's deferred sink (`Sink::Watch`), which outlives
the run — so only a host with a deferred sink installs it, and elsewhere
`watch` is an unknown-name diagnostic rather than a runtime refusal
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]). The second,
`DETACH_BUILTIN` (`cfg(unix)`, over `concurrency::builtin_detach` /
`scheme::detach`), is a value row of arity three — typed
`String → String → [String] → F [pid: Int, desc: String]` — and is carried
only where a host installs it, so absence is an unknown-name diagnostic rather
than a veto. It seeds a first-class native; handlers cannot intercept it,
and a value binding stubs it. Its program lookup bypasses bindings and the
whole handler stack as `^program` does. Whether a given call may spend it is
the live grant stack's question
(`GrantStack::permits(Flag::Shell(ShellVerb::Detach))`). The third, `SURFACE_BUILTIN`, wraps
`misc::builtin_surface` / `scheme::surface_op` under the bare name `surface`;
exarch declares its own entry over the same body under `exarch-surface`, with
its own doc, so no host's name enters core's vocabulary. `builtin_surface` is
`pub` for exactly this — the one core body two hosts each name their own way.

Filesystem query schemas live below the shell: `fact/fs.rs` declares
`DirEntry` (`list-dir`) and `FileInfo` (`file-info`) and constructs them purely
from observed metadata. `shell/builtins/fs.rs` owns the queries; the checker
reads the same records' `Typed` declarations.

`fs.rs` also owns `builtin_temp_dir` and `builtin_temp_file`: `require_rm`
calls `shell::exec::command::would_run("rm", …)` before `writable_temp_dir`
or creation. It preserves lookup/admission's error and denial record, adding
one hint explaining the need for `rm`; checking launches nothing. The parent
and absent leaf retain the normal write judgments and exclusive creation.
Prefer the prelude's `with-temp-dir` / `with-temp-file` for lexical lifetimes
([[decisions/261111_a-guard-acquires|a-guard-acquires]]).

The read and CSV schemes live in `typecheck/builtins.rs`:

- `env : ∀α:data. Returns α` admits the effective environment against its site's record
  or map shape (`ambient.rs`); values remain text.
- `from-csv : ∀α:data. Returns [α]` parses text cells and admits the rows against the
  site's type (`codecs.rs`). Written field reads use records; computed reads
  use maps. The row stays a decoder for misuse diagnostics.
- `to-csv : ∀α:data. [α] → Command` accepts record or map rows, requiring the
  first row's keys and scalar cells at runtime. Strings, numbers and booleans
  render as `str` renders them; other cells name their row index and key.
- `nth : ∀α ν. Integer → [α] → Command[ν] α` (`collections.rs`) refuses a negative Int and
  then reads through `evaluator::expr::position`, the read a written position
  uses; field reads and map lookups are `expr::field` and `expr::lookup`.

Bodies are grouped by concern, one submodule each:

- `strings.rs` — string and regex primitives, including `lines` (`String ->
  [String]`), the same split as `from-lines` run over a string's bytes rather
  than the channel, sharing `util::read_lines`/`lossy_line_list`. `dedent`
  owns the raw-block framing rule: blank lines around a multiline block fall
  away before the common margin is stripped, while content-line whitespace is
  preserved;
- `collections.rs`, `predicates.rs` (`keys`, `has`: ordering and equality are
  `ValClos::compare` / `ValClos::equals`, and `lt`, `gt`, `is-empty`,
  `sort-list`, `identity` are prelude one-liners over `<`, `>`, `length`
  and `sort-list-by`), `fs.rs`, `codecs.rs` — the last is also
  home to `builtin_echo`, `to-line`'s neighbour by nature: every argument
  rendered through the total `to-string` — `ValClos`'s `Display`, mapped over the
  argv — single-space intercalation, a newline to stdout.
  `write_encoded` (`codecs.rs`) writes its bytes to stdout and returns
  `ValClos::UNIT`, so `to-csv`, `to-bytes`, `ints-to-bytes`, `to-string`,
  `to-lines`, `to-json`, and `to-jsonl` are writers: each types
  `A → Command` at its own operand type, and its encoded bytes are what it
  writes. `to-bytes` takes `Bytes` and `ints-to-bytes` takes `[Int]` —
  two names, no union in the table. `builtin_from_jsonl` reads through
  `util::stdin_lines` and parses each line alone, so an error names the input's
  line. Each JSON direction has one typed refusal, worded by each codec in its
  own terms: `json_to_value`'s `OutOfRange`, `value_to_json`'s
  `Unrepresentable`, which `to-jsonl` prefixes with the element's index;
- `shell.rs` — `cd`. No builtin installs or removes a handler; persistent
  handlers are host-installed through rc or manifest `handlers:`,
  and scoped frames use `within [handlers: …]`;
- `concurrency.rs` — the `spawn` form's door (`spawn_form`), `watch` /
  `detach`, the handle verbs
  `await` / `poll` / `is-done` / `first-settled` / `race` / `cancel`, and the
  `workers` listing (`builtin_workers`), split by concern: the root keeps the
  verbs of birth, `concurrency/birth.rs` the `Birth` door (`spawn_child`) and
  the one exit rule a gather and a worker's base share (`exit_brood`),
  `concurrency/eliminate.rs` the eliminators, observations (`builtin_poll`,
  `builtin_is_done`, `builtin_first_settled`), and the one
  cancel-aware sweep (`first_ready`) under `await`, `race`, a gather's join and
  `first-settled` (`race` ends its losers and drains any already-`stopping`
  handles through `stop_and_drain`, `core/src/handle/brood.rs`), and `concurrency/tests.rs` the tests; the
  worker's deferred surface lives in `core/src/handle/surface.rs`, its
  cradle, brood and `BroodGuard` in `core/src/handle/brood.rs` (builtins under their bare
  names; `par`, `both` and `first-of` are prelude code over them,
  [[map/core/prelude|prelude]]). `spawn` and `gather` are control operators,
  not rows: the machine forms `spawn`'s operands and calls its door, and runs
  `gather` as a frame (`Frame::Gather`) that settles its `BroodGuard` by
  `exit_brood` on return, `BroodGuard::end` on halt, and `BroodGuard::stop` on
  `abandon`/`Drop` ([[map/core/evaluator|evaluator]]). All
  but the host-installed two seed through `CORE_BUILTINS`;
  the other two live here
  too but reach a session via `WATCH_BUILTIN` / `DETACH_BUILTIN`, not core. `builtin_detach` is the surface discipline
  alone — the birth itself is the ordinary external-command machinery down to
  the double-fork in `shell/exec/command.rs`
  ([[map/core/runtime|runtime]]), and it yields a `{pid, desc}` receipt, not a
  `Handle`, so none of the eliminators below apply to it. A handle is one
  private lock over a `HandleState`: `Running`, `Stopping` (cancellation
  requested while `Running`, worker thread still unwinding), or
  `Ended { ended, result }` (worker thread finished), whose `result` cell holds
  the `CompletedHandle { stdout, stderr, end }` while unclaimed (*ready*) and is
  taken once claimed (*consumed*).  Observers see these as the four-way `Sample`
  (`Running`, `Stopping`, `Ready`, `Consumed`). `HandleInner::new` returns the handle with its one
  `CompletionPermit`, which the worker thread owns: `CompletionPermit::finish`
  drains the buffers, transitions `Running` → ready or `Stopping` →
  consumed, and sets the end `Stamp` *before* calling
  `DeferredSurface::flush(DoneEvent)`, so a host woken by the deferred batch
  never observes the handle still non-terminal; a `CompletionPermit` dropped
  unfinished — a panic — completes the handle as a panic naming the worker and
  flushes its deferred surface, so `poll`/`race` see a finished block rather
  than spinning. Observers read a `Sample`, paired with the elapsed time in one
  consistent `Snapshot` (`HandleInner::snapshot`, `snapshot_io` for `poll`'s
  byte peeks), which also feeds `workers`/probe rows, and the transitions are `complete` (via
  `CompletionPermit::finish`), `claim_ready` (moves a `Ready` result out and leaves
  `Consumed`, in one step) and `claim_and_stop` (`Running` → `Stopping` firing
  the worker's node, or `Ready` → `Consumed` discarding the settled result;
  idempotent on `Stopping` and `Consumed`). So a result is *claimed* at most
  once, by construction. The registry lock may take a handle's lock, never the
  reverse. `await` is exact: `first_ready` polls the caller's cancel scope
  first, before inspecting any handle (so a pending cancellation is never lost
  to an already-`Ready` handle), then `join` waits (`claim_first`) until it
  claims the handle's `Ready` result or finds it already claimed (`Stopping` or
  `Consumed`); `exact` `replay`s the stdout bytes, then the stderr bytes, into
  the awaiter's own sinks and calls `End::into_settled` as the block ended —
  the value, the failure as it stands, an `exit` or a stop re-raised, a panic resumed —
  so `await` of a block is that block run there. A handle in `Stopping` or
  `Consumed` fails `await_handle` with `<verb>: this worker's result was already
  claimed`, an ordinary error, and `join` returns `None` for it, which a
  gather's return rule skips. `race` skips `Stopping` and `Consumed` handles
  when choosing a winner, failing as `await` does only when no handle is
  `Running` or `Ready` (at entry or mid-wait), claims the first `Ready` winner
  in list order, and then claims, stops and drains every remaining `Running` or
  already-`Stopping` handle within one shared cleanup grace before it returns.
  An empty list is its own error.
  `cancel` calls `claim_and_stop` (`Running` → `Stopping`, `Ready` →
  `Consumed`, and a no-op on `Stopping` or `Consumed`). The handle carries its
  block's grade (`Ty::Handle(Grade, Ty)`, printed `Handle (Returns α)` /
  `Handle Command`), so `await` returns the grade the block had. `poll` never
  writes and never claims, reporting all four states as data:
  `` `running [stdout, stderr, elapsed] ``,
  `` `stopping [elapsed] `` (a running worker carries a *cumulative,
  non-destructive* `CapturedBytes::peek` snapshot of its output so far, peeked before the handle is observed so the buffers stay intact for the
  worker's one-shot `CapturedBytes::take` at completion),
  `` `ready [stdout, stderr, elapsed, outcome: <ok: α | err: ErrorRecord>] ``
  (the ok/err half of `End::outcome`, its `` `err `` payload built through
  `outcome_value` and `ValClos::failure`, the witnessed record `try` hands its
  handler), or
  `` `consumed [elapsed] ``. A worker stopped from elsewhere settles with its
  own `cancelled`/`timed-out` error; on a `Ready` handle an `exit`, a
  foreign stop or a panic is re-raised, as `await` re-raises them, while `poll`
  never fails because a handle is `Stopping` or `Consumed`. `is-done`
  (`builtin_is_done`) is the boolean predicate over the same states, returning
  `true` for `Ready` or `Consumed` and `false` for `Running` or `Stopping`
  without raising
  ([[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]],
  [[decisions/260615_poll-total-failed-arm|the settle decision]],
  [[decisions/260702_partial-poll-pending-output|partial-poll-pending-output]]).
  A `spawn` or `watch` worker hangs under its shell's cradle, not the run's
  foreground scope: under the durable session root in a run outside any
  gather, so a foreground cancel never reaps it, and inside a gather — a
  worker's body or a `gather { … }` — under the gather's entry scope. A
  `guard` shield does not change this cradle: a worker is masked only if its
  owner is inside the shield, as with a `gather` opened during acquisition or
  release, not merely because it was spawned there. It is enrolled
  in its `Brood` (`Cradle::owning` paired with a unique `BroodGuard`): when the
  gather exits it closes membership first (rejecting and cancelling any late
  birth from a stage thread still unwinding against that cradle), awaits each
  unclaimed child on return, cancels them on a halt (`BroodGuard::end`), and on
  every normal or halting exit waits within one shared grace for every child it
  accepted to reach a terminal state (`Ready` or `Consumed`)
  (`BroodGuard::drain`), so `cancel` of a worker reaches its tree and nothing a
  gather started outlives it by more than the grace. `await` shares `race`'s
  cancel-aware wait loop (`claim_first`, over `first_ready`), so a deadline
  unwinds the wait while a session worker survives
  ([[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]],
  [[decisions/261020_gather|gather]]). `outlive` uses the same `claim_first`
  and `replay` when `Run.last` returns: session workers still unclaimed are
  claimed in birth order, their bytes written and all five endings dropped.
  Lifetime joins do not make a loose worker's failure the run's.
  The spawn door is `spawn_child`, which takes a `Birth` — `Spawn` or
  `Watch` — and reads the owner off the cradle: with a brood in scope the
  child is the brood's (`Owner::Gather` or `Owner::Worker`, from its
  `BroodOwner`), enrolled (or cancelled and rejected with `the enclosing gather
  has ended; the new worker was cancelled` if the enclosing gather has already
  closed), writing to the run's live sink as
  a pipeline stage does, unannounced and flushing no `` `done ``; with none it
  is the session's, its surface deferred. Hosts that announce gate its
  `done` on a present unclaimed row at notice time (`WorkerState::unclaimed`);
  batch announces none. It mints the worker's
  `WorkerId` first and builds the handle around it: `HandleInner` carries the
  `id`, the `title`, and the worker's clock, a birth `Stamp` and an end
  `Stamp` that its `CompletionPermit` sets on every exit path of the worker
  thread before flushing its deferred surface, from which a `Snapshot`
  derives `elapsed` and the end stamp, in one place. The
  registry entry adds only the `Owner`. A birth's `title` — the name every
  eliminator's error, the trail, and the completion event use — comes from the
  birth itself: a `spawn`'s title (non-empty and one line, `legible_title`),
  else `shell.call_site()` — `block at <script>, line N` — which the run's
  `TitlePolicy` may refuse before any birth; a watch's label. The completion
  event (`DoneEvent`) carries
  the handle's id, title and elapsed time beside its outcome, including on
  failure and panic. No policy reaps a running worker: it ends by completing,
  by `cancel`, or with its owner, and its handle capability is held only in the
  bindings that received it at birth (there is no `worker <id>` lookup builtin).
  The spawn door also enforces the frame's
  admission cap (`Mooring::worker_cap`): a birth *reserves* its seat at the
  door (`WorkerRegistry::reserve`) — refused while `cap` workers are `Running`,
  `Stopping`, or reserved, with an error naming the verb the caller actually
  wrote and `await`/`cancel` as the remedies, the reservation held across thread
  spawn and released into the registered entry, so a racing sibling birth never
  sees a filling seat as free; `Ready` and `Consumed` entries hold no
  seat. A `Ready` entry's one bound is retention, armed once at boot
  (`Shell::arm_worker_retention`, beside the binding lease): the registry
  keeps its own clock — one `tick_epoch` per source dispatch — and the engine
  sweeps at each settled run's ready boundary (`sweep_retention`, engine
  housekeeping), stamping an entry at the first sweep that observes it `Ready`
  while its parent worker (if any) is no longer live (`Running` or `Stopping`)
  and expiring it once its unclaimed result has sat stamped a full retention
  of ral calls. The expiry records a `Retention`-cause `ReapNotice`, which the
  engine pushes at the next settled run's ready boundary as a `` `notice ``
  surface event (`emit_ready_boundary_notices`; exarch decodes it back via
  `Notice::from_surface`). An unarmed registry (the REPL) retains `Ready`
  entries indefinitely; a claimer removes an entry when it transitions to
  `Consumed`, so the sweep only catches `Ready` results nobody claimed. A worker
  runs its thunk on a fresh `std::thread` via `Shell::spawn_worker`
  ([[map/core/shell-state|shell-state]]), seeded from the parent's session;
  `worker_body` takes the thunk's `(Arc<Comp>, Env)` and enters
  `machine::evaluate` directly ([[map/core/evaluator|evaluator]]), deliberately
  bypassing the `Toplevel`/`Phrase` boundary, because the worker's own `Shell`
  is the only one its bindings touch and they die with the thread. The worker
  carries the parent's grant stack, so a forced block *inside* the worker
  still meets the standard boundary rule and any external child it spawns is
  confined per-command — a `spawn` under a `grant` cannot escape it. Every
  `spawn_child` also files the freshly-minted handle on `shell.local.workers`
  — a per-shell registry, host-independent and carrying no policy
  ([[map/core/shell-state|shell-state]]). **A listing is data.** `workers` maps
  every `Running`, `Stopping`, or `Ready` entry (`Consumed` entries are
  omitted), ids ascending, through `worker_row` (`shell/runtime/state/host.rs`, shared
  with the `` `workers `` probe) to a seven-field record — `id`, `title`,
  `owner` (`` `session ``, `` `worker id ``, `` `gather ``), `state`
  (`` `running ``, `` `stopping ``, or `` `ready `` with
  `Done = Ending<(), ErrorRecord, CancelCause>`),
  `started` (Unix seconds), `elapsed` (seconds, off the handle's `Stamp`s),
  `settled` (`` `some `` Unix seconds iff `state` is `` `ready ``, else
  `` `none ``) — derived from a single consistent snapshot per handle
  (`HandleInner::snapshot`) without claiming it (`WorkerRow::ral_value`;
  the probe's eighth field, `retention-left`, is the host's alone). It holds
  no handle, so a top-level `workers` crosses the engine protocol. A `Handle`
  is shell-local: `Shell::marshal` writes its title, and a child on either
  seat reads it as a withheld handle of the same title, position and type;
  all observations refuse it with `withheld-handle`. A run
  whose result is or holds any handle projects to `Err`, status 1
  (`core/src/shell/run/report.rs`);
- `misc.rs` — `fail` takes a `String`, raising `Reason::raised` (a `Claim`
  of status 1); `raise` re-raises the `ErrorFact` a witnessed map holds
  (`MapRef::witness`) and otherwise decodes the closed `[frame, message, status,
  site]` record into a claim, refusing status 0 (`InvalidCause::ZeroStatus`)
  and above 255. There is no `describe` builtin. `exit` refuses outside
  `0..=255`. See
  [[decisions/261105_the-error-record|the error record]]. `builtin_surface` is the body
  `SURFACE_BUILTIN` (above) wraps: it forwards a tagged variant to the host's
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
  opening, heads the entry `name · source`, and labels `usage`, `type`,
  `path`, and `shadows`; absent facts are omitted. `locate_all` returns the
  whole resolution chain, so `explain`
  also names what a name shadows, a PATH binary included, which is the
  question `which` answers wrongly for every name ral provides. The command
  head is judged as a launch would be (`Command::lookup` under the live grant
  stack): a bundled tool reads `bundled`, a refused head `denied by grant`
  naming the path as requested;
- `print.rs` — the shell's runtime values as a `Printable` view onto the one
  value pretty-printer, which lives beneath the shell in `data/print.rs`
  (`PrintParams`, `pretty_print`; re-exported here), with the REPL's numbers
  in `REPL_PRINT_PARAMS`. `Printable::Child` carries variant payloads as well
  as list/map children (`Cow<ValClos>` for runtime values, references for
  `FOValue`). The REPL, exarch's tool-result rendering and a grant
  shown to its agent share it; a rendering utility, not a registered
  builtin. One printer, one policy, per-reader numbers: truncation preserves
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

**`use` is a control operator, not a builtin or boundary.** Elaboration calls
`shell::load::load_module` to read and link one written path under the grant
in force at check time. The checker instantiates its decided public schemes;
`shell::load::run_module` runs that checked tree under Σ and returns its public
bindings. Modules carry no cache; cycles and depth are refused by a check-time
chain, not runtime shell state. See
[[decisions/260606_cacheless-module-loader|cacheless-module-loader]].

The capability `ValClos`-map decoder is *not* a builtin: it lives at
`shell/grant/decode.rs` (`decode_capability_map`), consumed by the `grant`
control operator (`shell/evaluator/machine.rs`), the `--capabilities` ceiling
(`shell/load/profile.rs`) and spawn grants (`shell/grant/spawn.rs`) — see [[map/core/capabilities|capabilities]],
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
`bundled_name` returns the static name of a bundled tool. `bundleable_tools`
lists every tool some build bundles, whatever features this one has: the names
a profile may list outright, each admitting the host binary where this build
does not bundle it. These bundled heads share the
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
`docs/SPEC.md` §14.7 covers the single-binary tool surface. The utility
subprocess entrypoint retains its own `i32` result clamp in `uutils.rs`; it is
not part of the shell ending family
([[design/endings|endings]]).
