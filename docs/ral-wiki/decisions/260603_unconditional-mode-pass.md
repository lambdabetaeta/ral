---
status: superseded
superseded_by: decisions/260809_pipes-are-positional-byte-wires
landed: [767a96bf, 602218d3, 51419f6c]
---

# The mode pass always runs; only value-type strictness is optional

**Landed** in three commits: `767a96bf` makes the inference pass unconditional and
narrows `--no-typecheck` to a value-type-warning window; `602218d3` is the harvest
— the second engine and its residue come out, the annotation slots drop their
`Option`; `51419f6c` closes the window — `--no-typecheck` retires with the
fragment/verdict apparatus that was its only consumer. The two latent bugs the
work surfaced and the loaders it newly checks are recorded below.

**The deprecation window is now closed.** Value-type errors are fatal on every
path, exactly as mode errors are; the flag, the fragment classification, and the
three-armed verdict that distinguished the two no longer exist. The sections
below that describe the window's behaviour are retired: they record *what the
window did* while it was open, not live behaviour. See *Closing the window* at
the end for what unwound.

The inference pass over each program is not advice the user may decline — it is
*compilation*. After [[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]
the evaluator reads each stage's modes off the IR, and the only thing that writes
those annotations is the checker. **The inference pass therefore always runs,
because it produces the mode wires the evaluator cannot run without; what stays
optional is solely how loudly the value-type fragment fails.**

This is the fourth and last of four decisions whose end state is a single
mode-inference engine; the prior three have landed. The arc:
session-scheme-continuity ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]) →
handler/alias mode preservation ([[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]]) →
IR mode annotation ([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]) →
the mode pass made unconditional (here). The through-line is **net code
removal**: the prior three made the typed-IR rung of the
[[internals/compilation-ladder|compilation-ladder]] real and proved the second
engine redundant. This page collects the corpses — it deletes the second engine
and its residue, narrows the `--no-typecheck` flag that was the engine's last
caller to a value-type-warning window, and finally retires that window: the flag
and the fragment/verdict apparatus built to honour it come out together. One
judgment, one engine, one verdict.

## Mode errors are always fatal — no expressiveness is lost

`--no-typecheck` today skips the static checker entirely (`ral/src/main.rs`,
`render_type_errors` gated by `if !no_typecheck`; `boot.rs::source_config_inner`,
the same gate). Once the checker is also the *only* source of the evaluator's
mode wires, skipping it is no longer "trust me, run it anyway" — it is "run with
no modes". So the mode pass separates from the value-type pass:

- **A mode mismatch is fatal today anyway, just later and worse.** A
  `∅`-into-`Bytes` edge that the static checker would reject is, under
  `--no-typecheck`, caught at evaluation by the runtime engine's
  `validate_pipeline` → `pipeline_mismatch`
  ([[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]],
  T0012; `core/src/evaluator/pipeline/analysis.rs`). Check-time fatality is the
  *same verdict, strictly earlier* — before any side effect of the turn runs,
  with a source span instead of a stage index. Making it always-fatal removes no
  program that runs today; it only moves the rejection forward.
- **The annotation is what the evaluator consumes.** With the runtime engine
  deleted, there is no fallback that re-derives modes from a live binding. An
  unannotated IR has no wires at all. "Skip the mode pass" is therefore not a
  policy the language can offer — it is a request to evaluate an incomplete
  program. The pass runs unconditionally on every evaluated IR: REPL turn, rc
  line, batch script, exarch turn.

## Value-type errors: fatal by default, a deprecation window downgrades them

> *Retired.* This section records the window while it was open; the flag and the
> `Verdict`/`ErrorFragment` apparatus it describes were removed in `51419f6c`.
> Value-type errors are now fatal on every path. See *Closing the window*.

The value-type fragment — the Hindley–Milner judgment over `Str`, `Int`, records,
lists, variants — is genuinely *advice the user could decline* in a way modes are
not: a value-type mismatch never feeds the evaluator a wire it needs, so
downgrading it to a warning leaves a runnable program. We commit to:

- **Value-type errors are fatal by default.** Every evaluated path reports them
  and refuses to evaluate, matching today's REPL and exarch behaviour (both route
  through `compile_and_typecheck`, which returns `CompileOutcome::Types` and skips
  eval — `ral/src/repl/exec.rs`, `exarch/src/shell_eval.rs`).
- **A deprecation-window flag downgrades them to warnings.** `--no-typecheck`
  keeps its name for the window: zero churn for the users who already type it, and
  it is retired *with* the flag, so a rename would only add a second name to
  delete. Its meaning narrows from "skip the checker" to "downgrade value-type
  errors to warnings" — the inference pass still runs (it must, for the wires),
  the mode fragment is still fatal, and a value-type error prints as a warning and
  evaluation proceeds.

The split between the two judgments is a fact of the error type. `TypeErrorKind`
carries `fn fragment() -> ErrorFragment { Mode, ValueType }`
(`core/src/typecheck/scheme.rs`): a `ModeMismatch` is the mode fragment, a
`CompTyMismatch` is the mode fragment exactly when one of its component diffs is a
channel disagreement (`CompDiff::Stdin` / `Stdout`) — the two computations failed
to agree on a wire — and every other error is the value-type fragment.
`typecheck_verdict` (`core/src/typecheck.rs`) returns the three-armed
`Verdict::{ Clean(Comp), ValueErrors { comp, errors }, ModeErrors(errors) }`: a
clean check carries the comp annotated with schemes on the spine and wires
everywhere (`annotate(_, true)`); a `ValueErrors` carries a runnable
*scheme-less-but-wired* comp (`annotate(_, false)` — the wires were written, the
spine schemes withheld) beside its errors; a `ModeErrors` carries no comp, because
no consistent wire set exists. `compile_to_verdict` is the flag-honouring twin of
`compile_and_typecheck`; the latter stays intact and strict for exarch, `--check`,
and the build. `--no-typecheck` downgrades only the value-type fragment, rendered
through ariadne with `Warning` severity (`core/src/diagnostic.rs`); REPL, batch,
and rc all honour the flag, while `--check` stays strict regardless.

**A turn whose check reports errors contributes no schemes.** The window makes
reachable a turn that warns yet evaluates, so it forces a decision the total error
path of [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]
never had to make: what schemes seed turn *N+1* off a turn whose check errored.
None. Schemes generalised out of a failed unification are not verdicts; seeding
the next turn with them would cascade one turn's error into spurious errors on
every later use of the binding. Under the window a warned turn evaluates an IR
whose mode `Wire`s are written as ever — the mode fragment checked clean, or the
turn would not evaluate at all — but whose `Bind` scheme slots stay empty: its
binds install values with no scheme, and turn *N+1* checks those names as bare
names, exactly ADR 1's rule that `--no-typecheck` bindings are scheme-less, with
a fresh type variable per use site. Mechanically this extends the withholding the
code already performs — `typecheck`'s error path returns no annotated comp
(`Err(ctx.errors)` before `annotate_binds`, `core/src/typecheck.rs`); the warn
path keeps that withholding for the scheme slots while still emitting the wires,
rather than annotate-and-downgrade. The symmetry with the fatal mode fragment closes it:
a turn that *evaluates* under the window has clean modes but possibly unsound value
types, so withholding its schemes keeps the cross-turn seed sound without blocking
the user.

**Honest risk.** Every checker over-constraint becomes a hard blocker the day
value-type strictness is the default. The 260601 ADR, the day mode-strictness was
first enabled, surfaced *two* genuine over-constraints — the higher-order
callback modes (`map { echo $x }`) and the `try`/`guard`/`audit` thunk bodies —
both fixed by making the rule mode-polymorphic rather than by relaxing it. More
are likely latent. The warnings window is the mitigation: a user blocked by a
spurious value-type error sets `--no-typecheck`, keeps working, and files the
case. **A checker over-constraint is a checker bug to fix, not a reason to keep
the escape hatch** — the window exists to buy time to fix those bugs, not to make
the checker permanently optional. The flag retires when the bug stream dries up.

## rc files at boot

Today an rc file that fails its type check is skipped: `source_config_inner`
prints the errors and returns `Err("skipped due to type errors")`, which
`source_config_file` surfaces as one `cmd_error` and otherwise swallows — the boot
continues. This is exactly how the same function already handles an rc *parse*
error (`compile(...).map_err`, the same `Err` path), so the policy is uniform:

- **An rc file is reported and skipped, never fatal to the boot.** Under the
  unconditional mode pass the offending rc file is reported and skipped while the
  session still starts, aligned with the parse-error precedent. A broken rc must
  not strand the user at no shell. This policy is live: a type error of either
  kind in rc skips that file (it has no runnable annotation) but the boot survives.
  (While the window was open, a value-type error in rc instead downgraded to a
  warning and the file still applied — retired in `51419f6c`.)

## exarch needs nothing

exarch already runs mandatory checking every turn — `run_shell` calls
`compile_and_typecheck` unconditionally and has no `--no-typecheck` flag in its
surface at all (`exarch/src/shell_eval.rs`). This series merely brings the
REPL/scripts/rc world up to the bar the agent path already meets.

## The harvest — what this page deletes

This is the series' payoff. With the IR carrying ground wires and the pass
mandatory, the second engine and its scaffolding have no caller left. The harvest
landed in `602218d3` (with the three whole-file removals having physically ridden
into history alongside a concurrent session's commits — see below):

- **`core/src/ty.rs` deleted in full** (616 lines, `wc -l` at `edf47cd9`). The
  structural judgment, the `ModeUnifier` union-find, `InferCtx`, and
  `CommandTypeResolution` / `command_is_internal` (ADR 3 records the `internal`
  field is *written* at three sites and *read nowhere* — dead, deleted with the
  rest, not because it is inference but because what it classified into is
  unused). The static twins in `typecheck/infer.rs` survive; the runtime copies
  go. The file grew from ADR 3's predicted 506 lines while landing `edf47cd9`:
  the dual-run alignment added *transitional* code that kept the two engines'
  verdicts identical until this file went — `external_spec` (an external head's
  open input, mirroring `external_exec_comp_ty`), `env_binding_type`'s
  `seen`-set recursion break and lexical closure-body resolution
  (`env_binding_type_seen`), and `named_head`'s peering through `Force(Thunk(..))`
  and a `Bind`'s `rest`. None of it survived the file; all of it existed only to
  hold the dual-run green, and died with the engine it duplicated.
- **`core/src/classify.rs` deleted in full** (33 lines). `prelude_command_spec` /
  `PRELUDE_BYTE_FNS` — the hand-maintained `["map-lines", "filter-lines",
  "each-line", "view"]` byte-mode list, a *third* copy of mode knowledge the
  shell-free fallback consulted because it could not walk a prelude body. The
  baked prelude schemes and the IR annotation carry the real inferred modes; the
  residue is superseded.
- **`core/tests/type_system.rs` removed** (65 lines). The engine-internal test
  target dies with the engine it exercised; the branch-join behaviour it pinned is
  ported to observable `rhs_output` tests.
- **`validate_pipeline`'s runtime unification and the `pipeline_mismatch`
  diagnostic deleted** (`core/src/evaluator/pipeline/analysis.rs`). They were
  already bypassed on annotated IR: `specs_from_wires` reads ground,
  adjacency-consistent wires. What `validate_pipeline` enforced demotes to a plain
  `debug_assert!` in `specs_from_wires` that the adjacent wires agree (the checker
  unified the wires it emitted, so a debug build only asserts it). `rg
  pipeline_mismatch` has no code hits; the static T0012 rendering is unaffected.
- **The `ModeStore` trait and the genericity of `unify_mode` folded into the
  checker's unifier** (`core/src/mode.rs`). The trait existed for one reason: to
  drive the equality rule through *both* engines' variable stores so they could
  not drift (260601's "neither can drift again"). With one engine left the sharing
  mechanism has done its job — `unify_mode` becomes a plain method on the checker's
  `Unifier`, and `ModeStore` / the runtime `ModeSlot` / `ModeVar` union-find go
  with it. The lattice (`PipeMode`, `PipeSpec`, `ByteMode`, `Wire`, `ModeMismatch`)
  survives: it is what schemes serialise and what the annotation pass grounds into
  a `Wire`.
- **ADR 3's dual-run scaffolding deleted.** The transitional `debug_assert!`s that
  compared each annotation against the old runtime engine's verdict — `runtime_specs`
  / `stage_runtime_spec` (`analysis.rs`, the debug-only stage oracle), the
  `specs_from_wires` cross-check, and the cross-check arm of `rhs_is_byte_output`
  (`evaluator/comp.rs`, inlined into `eval_bind_rhs` once the cross-check left it a
  bare read) — lost their right-hand side when `ty.rs` went and were removed with
  the engine.
- **The annotation slots lose their transitional optionality.** With every
  evaluated IR passing through annotation, ADR 3's `Wire` slots no longer need to
  model "checker has not run yet." **The elaborator emits a placeholder wire that
  the annotator overwrites; the slots are plain — `Pipeline.wires: Vec<Wire>`,
  `Bind.rhs_output: ByteMode`, not `Option`.** This is the simpler shape and the
  right one: a read site that unwraps an `Option<Wire>` would be re-encoding "the
  pass might have been skipped" — a state this ADR makes unreachable. A non-optional
  slot makes the invariant a type fact, and the IR reads as inevitable rather than
  transitional. The elaborator's placeholder is `Wire::EMPTY` — the same
  `Var → Empty` default the annotator writes for an unconstrained wire (ADR 3's one
  defaulting rule), so an un-annotated-but-evaluated path — which this ADR forbids —
  would degrade to all-`Empty` rather than panic, but no such path survives.
- **The `no_typecheck` plumbing strip — DONE by `51419f6c`.**
  `RunOpts.no_typecheck` and the clap `--no-typecheck` arg (`ral/src/main.rs`), the
  threading through `BatchOpts`/`InteractiveOpts` and `Session::boot` →
  `load_profiles` → `source_config_file` → `source_config_inner`
  (`ral/src/repl/session.rs`, `ral/src/repl/session/boot.rs`), and the
  flag-honouring gates at the batch and rc check sites all come out. The Option-drop
  above did *not* wait on this: it required only the unconditional pass, not the
  window's close. See *Closing the window* for the apparatus that came out with the
  flag.

**Whole-file removals and the concurrent commit.** `core/src/ty.rs` (616) and
`core/src/classify.rs` (33) were physically dropped from the tree by a concurrent
documentation session — `25bc13f5` carried both file deletions, and the
test-target removal `core/tests/type_system.rs` (65) rode `55ba22c3`. `602218d3`
completes the parcel: it lands the slot reshaping, the `mode.rs` fold, the
diagnostic demotion, the dual-run teardown, the loader checks, and the latent-bug
fixes that make the engine's absence sound.

**Net deletion.** `core/src/ty.rs` (616), `core/src/classify.rs` (33), and the
`type_system.rs` target (65) come out whole. The `ModeStore`/`unify_mode`
genericity in `mode.rs`, `validate_pipeline` + `pipeline_mismatch`, and the
dual-run assertions are a further net reduction; the still-parked `no_typecheck`
plumbing across `main.rs` / `session.rs` / `boot.rs` is the last to go. The series
is net-negative by several hundred lines: **one judgment, one engine.**

## Every evaluated path is now checked

The unconditional pass closed the two remaining holes where IR reached the
evaluator unchecked:

- **The `source` / `use` loaders** (`core/src/builtins/modules.rs`) loaded a file's
  source and evaluated it without a check. A new `check_source` routes them through
  `compile_and_typecheck` with any type error fatal — a loader carries no
  deprecation flag (none ever reached it), so a mode or value-type error is
  surfaced as a `Break::Error`, the file reported and not run.
- **Plugin files** (`ral/src/repl/plugin/load.rs`) likewise run
  `compile_and_typecheck`, treating any type error as fatal.

Sandbox child bodies were already sub-nodes of annotated parents; the baked
prelude is annotated at build time. The synthetic single-stage uutils pipeline
(`dispatch_uutils_via_pipeline`, `core/src/evaluator/command/uutils.rs`) is an
external head with no upstream, so it *synthesizes* its own ground wire at
construction — a lone stage's open external input grounds to `∅`, its output is
bytes, mirroring `PipeSpec::ext`. This
was verified wire-for-wire by the dual-run asserts while the old engine still
existed, then the scaffolding went.

## Two latent bugs surfaced by the work

Making the evaluator read modes off the node — rather than re-infer them at
runtime — turned two previously-invisible mistakes fatal. Both are recorded here
because the symptom (an empty annotation read where one was expected) is the kind
a future node-reading consumer could reintroduce:

- **`Ast::Background` hoisted its pipeline eagerly.** The elaborator arm for
  `cmd &` lowered the inner pipeline with `to_val`, *forcing* it into a value
  instead of suspending it in a thunk the `Background` node spawns. This broke both
  fragments and the runtime spawn even under `--no-typecheck`. Fixed to
  `Background(Val::Thunk(body))` (`core/src/elaborator.rs`), with three behavioural
  tests.
- **`eval_fuzz`'s harness discarded the annotation.** The test harness evaluated
  the *bare elaborated* comp, throwing away the annotation it had just computed.
  This was harmless while modes were re-inferred at runtime — the discarded wires
  were recomputed — and fatal the moment the evaluator reads wires off the node (it
  would read the elaborator's `Wire::EMPTY` placeholder). The harness now runs the
  checked IR (`core/tests/eval_fuzz.rs`).

## Closing the window

The window's value was a deprecation buffer for checker over-constraints (see
*Honest risk*); with the bug stream dry, it retires. **The whole fragment/verdict
apparatus `767a96bf` built to honour the flag had no other consumer, so it unwinds
with the flag — a value-type error is now fatal exactly as a mode error is, which
is what the pre-existing strict path already expressed.** The strip is a net
reduction of ~250 production lines:

- **The flag and its plumbing.** `RunOpts.no_typecheck`, the clap
  `--no-typecheck` arg, and every thread — `Session.no_typecheck`, the
  `no_typecheck` params on `step`/`execute_input` and
  `load_profiles`/`source_config_file`/`source_config_inner` — are gone
  (`ral/src/main.rs`, `ral/src/repl/session.rs`, `ral/src/repl/exec.rs`,
  `ral/src/repl/session/boot.rs`). `--no-typecheck` is now an unknown flag.
- **The three-armed verdict collapses to "clean comp / errors."**
  `typecheck_verdict` folds back into `typecheck` — the direct
  `infer + (errors ? Err : Ok(annotate(_, true)))` it was before the window —
  and `Verdict` is deleted (`core/src/typecheck.rs`). `CompileVerdict` and
  `compile_to_verdict` are deleted; `compile_and_typecheck` / `CompileOutcome`
  is the single compile entry again (`core/src/lib.rs`), the path exarch,
  `--check`, and the loaders all already shared.
- **The fragment classification is deleted.** `ErrorFragment` and
  `TypeErrorKind::fragment()` / `TypeError::fragment()` had no reader once the
  verdict split was gone (`core/src/typecheck/scheme.rs`). The `CompDiff::Stdin`
  / `Stdout` variants stay — they are the unifier's real per-channel diff
  structure (`unify_comp_ty`, rendering); only the `fragment()` reader went.
- **The warning renderer is deleted.** `format_type_warning_ariadne` and the
  `ReportKind` parameter that distinguished it from the error render are gone
  (`core/src/diagnostic.rs`); `render_ariadne` and `render_messageless` hardcode
  `Error`, since every surviving site always rendered errors. The static T0012 /
  type-error rendering at the error sites is byte-for-byte unchanged.

The CLI-tier tests that pinned the window's *downgrade* behaviour
(warn-and-evaluate, warn-and-apply-rc) retired with it; the tests that asserted
live behaviour — a value-type error blocks the binary, a mode error blocks, an rc
type error is skipped while the boot survives — were ported to the no-flag world
in `ral/tests/type_errors_block.rs` (renamed from `no_typecheck.rs`). The
in-process `fragment()` classification tests (`core/tests/typecheck.rs`) and the
warned-turn-installs-no-scheme tests (`core/tests/session_schemes.rs`) were deleted
with the code they exercised.

## Sequencing

The planned order was independently verifiable steps; the *implementation* reversed
two of them. The mode pass went unconditional **before** the engine was deleted —
deleting the runtime fallback first would have left wireless `--no-typecheck` IR
unrunnable, since the unconditional pass is what guarantees every evaluated IR
already carries wires. So `767a96bf` (steps 1+3) preceded `602218d3` (step 2 and
the Option-half of step 4).

1. **Flip authority — DONE** by `edf47cd9` (ADR 3's third commit). The consumers
   read the `Wire` off the node when the checker has annotated it
   (`resolve_pipeline` → `specs_from_wires` → `analyze_stage`; `eval_bind_rhs` →
   `rhs_is_byte_output`), with the old engine surviving only as the
   `--no-typecheck` fallback (`specs_from_runtime`) and the dual-run cross-check.
   ADR 2's install-time `alias_arm_scheme` — the single engine invoked at install,
   reading no IR node — survives unchanged.
2. **Delete the engine + residue + runtime diagnostic — DONE** by `602218d3`
   (the whole-file removals having ridden `25bc13f5` / `55ba22c3`).
   `core/src/ty.rs` and `core/src/classify.rs` removed;
   `validate_pipeline`/`pipeline_mismatch` demoted to the adjacent-wire
   `debug_assert!`; `ModeStore`/`unify_mode` folded into the checker's `Unifier`.
   The tree builds and the suite is green with those files absent.
3. **Open the deprecation window — DONE** by `767a96bf`. Value-type errors became
   fatal by default; the flag downgraded them, the inference pass ran regardless.
   A value-type error warned-and-ran under the flag and blocked without it; a mode
   error never evaluated. The window then **closed** in `51419f6c` — the flag
   retired, value-type errors are fatal on every path.
4. **Strip the flag plumbing — DONE.** The `Option`-drop on the `Wire` slots landed
   in `602218d3` (it required only the unconditional pass, not the window's close);
   the `no_typecheck` removal from `RunOpts` and every thread landed in
   `51419f6c`. *Verified:* `rg no_typecheck` empty, `rg pipeline_mismatch`
   empty.

## Dependencies

- **ADR 1** carries each session binding's scheme forward, so a session-bound call
  site annotates with a real wire, not a defaulted one.
- **ADR 2** forbids any handler or alias from changing a head's modes after the
  check, so a compile-time annotation stays valid through dynamic rebinding.
- **ADR 3 (landed `d4aa75be`, `e3c23b4b`, `edf47cd9`)** writes the wires and
  proves they agree with the old engine across the suite; this page is what let
  ADR 3's deletion inventory actually land, by removing the unchecked fallback that
  was the old engine's last caller.

**Verified at the `602218d3` landing:** the full test suite is green with
`core/src/ty.rs` and `core/src/classify.rs` absent — 33 test binaries, 0 failures
(the `type_system` target removed, the `no_typecheck` target added, against the
pre-series 33); `-D warnings` and clippy clean; `rg pipeline_mismatch` returns
nothing. Smokes: T0012 renders on `echo foo | length`; map-lines / uutils /
external pipelines run; a value-type error under the deprecation flag prints as a
warning and *still evaluates*, the same error without the flag blocks, and a mode
error never evaluates regardless of the flag.

**Verified at the window-close landing (`51419f6c`):** `-D warnings` build
and clippy clean across the workspace; the suite is green — still 33 test binaries
(the `no_typecheck` target renamed to `type_errors_block`, not dropped), 0
failures. `rg` finds no code reference to `no_typecheck` / `ErrorFragment` /
`typecheck_verdict` / `CompileVerdict` / `compile_to_verdict` /
`format_type_warning` (wiki hits are this retired-flag history). Net −249
production lines. Smokes: a value-type error now *blocks* (`let x = hello; return
$[$x + 1]` exits nonzero with a T0010 Error — no flag to downgrade it); map-lines
and external pipelines run; `--no-typecheck` is an unknown flag.

See also
[[decisions/260603_session-scheme-continuity|session-scheme-continuity]],
[[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]],
[[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]],
[[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]],
[[internals/compilation-ladder|compilation-ladder]],
[[map/repl/startup|startup]],
[[map/exarch/shell-eval|shell-eval]].
