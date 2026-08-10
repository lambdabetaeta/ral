---
generated_at_commit: 95449d4
generated_at_date: 2026-08-10
covers_paths: [core/src/builtins/, core/src/builtins.rs, core/src/uutils.rs]
---

# Map: core / builtins

`core/src/builtins/` are the primitives implemented in Rust that run inside the
shell process. `builtins.rs` holds the `builtin_registry!` macro: each entry
binds its facets at once — `names`, [[map/core/typecheck|type rule]] (`ty`),
`doc` line, and runtime body (`call`) — into the `CORE_BUILTINS` static
(`&[BuiltinEntry]`), so the facets cannot drift apart. Arity is no facet:
`BuiltinEntry::fixed_arity` derives it from the type rule and caches it, and that
derivation is the classification — fixed arity seeds a `Value::Native` in the
base scope, a variadic or optional argv seeds a base handler frame
(`native_value`, `seed_natives_and_base` in `types/shell/host.rs`;
[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]).
A body is a `BuiltinBody` — a `Static` `fn(&[Value], &Mooring, &mut Shell) ->
Settled<Value>`, or a `Captured` closure of the same shape for a host frontend
with state to carry — so the run's mooring arrives beside the shell
([[map/core/shell-state|shell-state]]): it is how a body surfaces an event,
enquires, or starts a nested run parented under the run that called it. The
type-rule facet is a `BuiltinTypeRule` of two arms: `Sig` (a command signature)
or `Scheme` (a first-class polytype). The streaming reducer `fold-lines`
registers as an ordinary `Scheme` whose factory writes its forwarded
[[design/types|payload route]] directly ([[map/core/typecheck|typecheck]]);
there is no separate reducer arm. A `Sig`-ruled entry's first-class form is
derived from the signature by `derive_sig_scheme`
([[invariants/fixed-arity|fixed-arity]]); `BuiltinSig::value` remains as the
hand-written override for a scheme the templates cannot state, and no entry sets
it.
Builtins are *shell-scoped*: each shell's session carries a `BuiltinTable`
([[map/core/shell-state|shell-state]]) seeded from `CORE_BUILTINS`
(`core_builtin_table`), and a host's extra sets ride a `HostSurface` into
`boot::boot_shell` (`core/src/boot.rs`), so the checker's rule table, the base
scope, and the base frames all come from one manifest — there is no
process-global registry, and every path that builds or hydrates a shell must
seed through `install_builtins` or re-link a native by name. `register` clones the
baked prelude's bindings into each fresh environment. Three builtins sit
*outside* the macro, implemented in core but installed by a host. Two are a
pair with the hosts swapped: the public `WATCH_BUILTIN`
(`&[BuiltinEntry]`) wraps the still-private `concurrency::builtin_watch` /
`scheme::watch` so a host with a durable stdout sink (the interactive and
batch ral hosts) installs it while an agent host omits it
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]]); its mirror
`SERVICE_BUILTIN` wraps `concurrency::builtin_service` / `scheme::service` so
the agent host (exarch), whose lease frame reaps ordinary workers, installs
the durable-birth verb while the ral hosts — which grant no lease, so every
spawn of theirs already lives until cancel or exit — omit it. The third,
`DETACH_BUILTIN` (`cfg(unix)`, `sig::DETACH` over
`concurrency::builtin_detach`), is carried by a host that arms a detach
policy: installing the verb and arming the budget (`Shell::arm_detach`) are
one act, so absence is an unknown-name diagnostic rather than a veto, while
whether a given call may spend it is the live grant stack's question
(`GrantStack::permits_detach`).

Bodies are grouped by concern, one submodule each:

- `strings.rs` — string and regex primitives. `dedent` owns the raw-block
  framing rule: blank lines around a multiline block fall away before the
  common margin is stripped, while content-line whitespace is preserved;
- `collections.rs`, `predicates.rs`, `fs.rs`, `codecs.rs` — the last is also
  home to `builtin_echo`, `to-line`'s neighbour by nature: per-argument
  `str`, single-space intercalation, a newline to the byte channel.
  `write_encoded` (`codecs.rs`) writes its bytes to stdout and returns
  `Value::Unit`, so `to-csv`, `to-bytes`, `to-string`, `to-lines`, and
  `to-json` are writers: each types `A → F[Bytes] Unit`, and its
  encoded bytes are its sole payload;
- `shell.rs` — `cd`, `alias` / `unalias`;
- `concurrency.rs` — `spawn` / `watch` / `service` / `detach` and the handle
  verbs
  `await` / `poll` / `race` / `cancel` (builtins under their bare names; `par`
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
  one settle. `try_settle` is the shared non-blocking sample (cached outcome, else a
  `try_recv` completed through `complete_handle`; a `Disconnected` receiver — a
  panicked worker — settles as the same failure `await` reports, so `poll`/`race`
  see a finished block rather than spinning). `await`/`race` `project_completed` the
  outcome to `{value, stdout, stderr}`, re-raising `` `err ``; `poll` is total,
  wrapping it as `` `settled `` `{stdout, stderr, outcome: `ok/`err}` (the `` `err ``
  payload built through the shared `evaluator::scope::error_record`, the record
  `try` hands its handler) or `` `pending `` `{stdout, stderr}` (a *cumulative,
  non-destructive* `peek_buffer` snapshot of the running worker's output — the
  buffers are left for the one-shot completion `take_buffer`, so a partial poll
  never steals bytes), and leaving `last_status` at 0 since the block's status is
  data. `await` and `poll` gate first on `ensure_live`, the cancelled pre-check
  ([[decisions/260615_poll-total-failed-arm|the settle decision]],
  [[decisions/260702_partial-poll-pending-output|partial-poll-pending-output]]).
  A detached worker hangs under the durable session root, not the run's
  foreground scope, so a foreground cancel never reaps it; `await` shares
  `race`'s cancel-aware wait loop (`wait_first_settled`), so a deadline unwinds
  the wait while the root-scoped worker survives
  ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).
  Under a frame that grants a `WorkerLease`, `spawn` arms a self-re-arming
  `process::reaper` callback — the idle-observation lease chain: a
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
  the spawn door: `spawn_child` takes a `LeaseClass`, and only a `Worker`
  birth arms it — `service` registers `Durable` and arms nothing, so no
  reaper entry ever exists for it; the absent chain *is* the durable
  policy, whose only bounds are the handle's own `cancel`, the host's
  `/clear`, and process exit.
  The spawn door also enforces the frame's admission cap
  (`Mooring::worker_cap`): a birth of any class *reserves* its seat at
  the door (`WorkerRegistry::reserve`) — refused while `cap` workers are
  running or reserved, with an error naming `await`/`cancel` as the
  remedies, the reservation held across thread spawn and released into the
  registered entry, so a racing sibling birth never sees a filling seat as
  free (`workers` is retired — [[map/exarch/builtins|builtins]]); settled
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
  which inherits a snapshot of the parent's mobile state; the body is evaluated
  *directly* with `eval_comp(.., Tail::Yes)` under a child scope, deliberately
  bypassing the `eval_top_level` / `eval_block` boundary, because the worker's own
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
  a resident, process-local reference: it cannot cross the pipeline-stage helper
  wire, so returning one from a helper-evaluated stage raises the wire diagnostic
  *"cannot return a handle from sandboxed evaluation"* (`core/src/serial.rs`)
  rather than a generic failure
  ([[internals/capability-enforcement|capability-enforcement]]);
- `modules.rs` — the cacheless `use` / `source` loader. `evaluate_source` is
  the shared parse + elaborate + evaluate core — `check_source` compiles
  against the live session, peeking the `FileId` its own registration will
  mint so the module's spans carry its real identity, and `evaluate_checked`
  holds the cycle stack and depth bound; `use` is a scope-projecting wrapper over it,
  `source` evaluates into the caller's scope. Module loads carry no cache, so
  the guards keep re-evaluation terminating — see
  [[decisions/260606_cacheless-module-loader|cacheless-module-loader]];
- `misc.rs` — including `surface`, which forwards a tagged variant to the host's
  [[map/core/shell-state|`SurfaceSink`]] and is the identity under a bare REPL;
- `math.rs` — the Float rounding builtins (`round`, `floor`, `ceil`, `trunc`);
- `help.rs` — `help` (arity-0 command index) and `explain <name>` lookup;
- `print.rs` — the value pretty-printer shared by the REPL and exarch's
  tool-result rendering (`PrintParams`; a rendering utility, not a registered
  builtin). One printer, one policy, per-reader numbers: truncation preserves
  identity — the depth limit summarises (keys and heads) and only the floor
  beneath it counts, a string elides unless it *is* the whole value, and a byte
  budget is spent inside each container, which closes with `…N more`. The two
  readers differ only in window, quote fence, and absorbable bytes;
- `util.rs` — shared helpers, JSON coercion.

The capability `Value`-map decoder is *not* a builtin: it lives beside the
authority layer in `capability/decode.rs` (`decode_capability_map`), consumed by
the `grant` control operator (`evaluator/scope.rs`) and the `--capabilities`
ceiling (`capability/load.rs`) — see [[map/core/capabilities|capabilities]],
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
That dispatch — the `ExecImage::BundledTool` placement and the hidden
entrypoint — is the [[map/core/runtime|runtime]]'s; this page owns only the
registry of names, shims, and the in-binary `uutils_invoke` they converge on.
`docs/SPEC.md` §21 covers the single-binary tool surface.
