---
generated_at_commit: 0055ee6f
generated_at_date: 2026-10-01
covers_paths: [core/src/typecheck/, core/src/typecheck.rs]
---

# Map: core / typecheck

`core/src/typecheck/` is Hindley–Milner inference over the CBPV
[[map/core/ir|IR]]. Types sit on `Val` and `Comp` after
[[map/core/elaboration|elaboration]].

Entry points (`typecheck.rs`):

- `typecheck(top: &Toplevel, SessionSchemes) -> Result<Toplevel, Vec<TypeError>>`
  — this function checks a program: infer each phrase in order, extending
  `TyEnv` at each `Define`, then `annotate::annotate_toplevel` writes the
  verdict back, on success returning an *annotated* `Toplevel`. `annotate`
  writes two things onto the rebuilt IR: a generalised `Scheme` per name a
  `Phrase::Define` binds — landed on its own phrase, not a shared spine —
  resolved against the final unifier and closed by quantifying its
  residuals, that is, generalised against the empty environment; and a
  `Capture`/`Decode` pair around each computation a value demand captured
  ([[map/core/ir|ir]]). `infer_pipeline` records each stage's value
  type in `InferCtx::stage_types`, keyed by stage address, and `annotate`
  resolves them against the final unifier. The stage types are
  typing metadata for the structural REPL, not a transport channel — the
  evaluator never reads them, so an un-annotated stage keeps the elaborator's
  `Unit` placeholder without harm. A pipeline's value is its final stage's, so
  there is nothing per-pipeline to annotate
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  The seed for a check is one `SessionSchemes { bindings, aliases,
  builtins }`
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]):
  the scope's name-to-`Option<Arc<Scheme>>` map, the alias arms' schemes, and
  the shell's own `BuiltinTable`. A scheme never changes once built, so one
  `Arc` carries it from the `TyEnv` onto `Phrase::Define`, into the scope's
  `Binding`, and back into the next run's `TyEnv`: neither seeding nor a
  lookup copies one. Builtins are shell-scoped, so the checker
  types against exactly the surface the booted shell dispatches
  ([[map/core/builtins|builtins]]). `seed_env` is the one seeding routine, and
  the manifest reaches it as two: a table entry's rule, and a base-frame row's
  scheme, which lands in the env so that `lookup_handler` finds a base frame as
  it finds a handler
  ([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).
  Every name the seed and the program bind is therefore known to the checker: a
  `$name` outside them is `UnboundVariable` (T0071), and a session binding of a
  non-thunk in head position is `HeadBoundToValue` (T0072)
  ([[internals/type-inference|type-inference]]).
- `bake_prelude(top: &Toplevel) -> (Toplevel, Vec<(String, Scheme)>)` — called by
  `boot::bake_prelude_to_out_dir` from each host's build script: returns the
  annotated prelude `Toplevel` alongside the schemes harvested off its
  `Phrase::Define`s (`harvest_schemes`), which needs no tree walk since every
  phrase already carries its own — one pass behind both the build-time bake
  and a run's installs.
- `alias_arm_scheme(head, param, body, SessionSchemes) -> Result<Scheme, Box<TypeError>>`
  — infers an alias arm under the runtime handler calling convention, holds it
  to what `head` stands in for (`Inferencer::stands_in`), and closes it, for
  `install_alias` and `WithinScope::parse` to store on a frame. A handler or
  alias arm is a *fixed-arity lambda* — its calling convention is the surface form, not the
  runtime value's shape, so `param` is non-optional and `infer_alias_arm` types
  the arm `Fun(List(elem), body)`, forcing it on the argv list
  ([[invariants/fixed-arity|fixed-arity]],
  [[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
  Statically `infer_handler_comp` still types a non-`Lam` thunk (e.g. a computed
  `alias g $h`) by its bare body, binding it so `g x` is an arity mismatch
  rather than a silently discarded argument; the runtime install boundary is the
  sole complete gate on shape.

The sorts split with CBPV:

- value types `Ty` describe data;
- computation types `CompTy` are `Return(Ty)`, `Fun(Ty, CompTy)`, and `Var`;
- records are open-row-polymorphic ([[design/row-types|row-types]]: `Row` /
  `RowVar`).

Generalisation happens at `Bind`; recursive bindings (`LetRec` / `Rec`) stay
monomorphic to keep generalisation sound.

Internals:

- `infer.rs` — the `Inferencer`; `infer_comp`;
- `grade.rs` — the grade rules, as one `impl Inferencer`: the bind rule
  (`rhs_bound_ty`), a block meeting a demand of the other producer kind
  (`adapt` to an `Adaptation`, `coerce`), the join of a form's arms
  (`join_arms`; `join_target` takes the greatest `Verdict`), and a stdout
  redirect's `discharge`. `spine` reads a `CompTy` as a `Spine` (`ty.rs`:
  parameters and tail; `producer()` the `Producer` past them, `over` the
  same parameters on another tail; `CompTy::arrows` is the constructor);
- `index.rs` — the deferred reads `Lbl` (a bare label) and `Idx` (a computed key):
  owns `InferCtx::pending_labels` and `pending_indexes` and their settlement.
  A pending label is closed as a record at the boundary that owns it; an `Idx`'s
  variables are weak, nothing settles it at a generalisation, and
  `settle_pending_indexes` drains both at the unit's end and refuses what is left
  (`IndexContainerUnknown`, T0075);
- `unify.rs` — `Unifier`; binding refuses a cycle that crosses no data (`CyclicType`, T0073)
  and a head its variable's kind does not admit (`KindMismatch`, T0074);
  the weak set (`mark_weak`, inherited through `unite`), which `generalize.rs` subtracts,
  `seed_env` re-seeds (`reseed_weak`) and `typecheck` settles (`settle_weak`);
- `Site` (`core/src/types/site.rs`) — the type solved at a boundary call, frozen
  at the unit's end by `InferCtx::snapshot_sites` and written onto the `Exec` by
  `annotate`; the unifier keeps the span that first bound each variable so a
  refusal can point at the use that imposed what the value met;
- `kind.rs` — `Kind`: the closed set of predicates a type variable carries (`number`,
  `comparable`, `scalar`, `sized`, `data`), their meet, and what a head is;
- `ty.rs` — the data-only type definitions (`Ty`, `CompTy`, `Grade`, rows);
- `scheme.rs` — `Scheme`;
- `error.rs` — the error taxonomy: `TypeError` / `TypeErrorKind`, with
  constraint provenance as data (`Reason`, `Standing`, `UnitCall`);
- `explain.rs` — the single home of every user-facing type-checker sentence
  (hints and `TypeErrorKind::render_label`), a pure function of the error data
  so each message is unit-testable. Its wildcard-free `Reason` match gives each
  reason prose or deliberately lists it as hintless: the constraint's other
  side is fresh, or the error kind is already its own diagnosis;
- `annotate.rs` — the write-back pass (`annotate`) that rebuilds the checked
  IR with schemes, boundary sites, stage types, and `Capture`/`Decode` nodes;
- `generalize.rs`;
- `env.rs` — `TyEnv`, `InferCtx`;
- `fmt.rs` — type display;
- `builtins.rs` — the scheme factory each entry names, and the readings taken
  off it (`fixed_arity`, `builtin_type_hint`), which is where
  [[invariants/fixed-arity|fixed-arity]] is enforced: every entry in the table
  declares its arguments, so it has an arity and a value form. `audit_record`
  types `audit { }`'s report as `{outcome: `` `ok v | `err E``, trail:
  [Observation]}`; `observed_ty` gives each `Observation`'s `what` a closed
  variant, one arm per kind (`command`, `write`, `read`, `grep`, `check`,
  `worker`, `act`), each its own closed record — so reading a fact's fields is
  ordinary row typing rather than a `Map` lookup [[design/audit|audit]];
- `scope.rs` — the five structural scope nodes, and `check_options`: the rule
  that holds each written option of `within` and `grant` to a declared table,
  key by key — unknown key T0076, refused key T0026, a clash at the key's own
  sentence, a written catch-all arm checked in context;
- `contract.rs` — the declared tables themselves (`within`, `grant`, the rc
  file, the plugin manifest), each a closed keyset whose labels are held at a
  ground type, left to their decoder, or refused with a sentence of their own.
  `ascribe` holds a contract file's finished return to its table, after
  inference and in a scratch copy of the unifier (unknown key T0076, not a
  record T0077, and the row errors). It is the single registration point, so the runtime doors that read the same
  keysets — `apply_rc_key`, `LoadedPlugin::parse`, `decode_capability_map` —
  dispatch off it rather than each keeping a copy. No table reaches the
  unifier ([[decisions/260930_a-table-never-enters-the-unifier|a-table-never-enters-the-unifier]]).

`infer.rs`'s `infer_case` is left whole by decision
([[decisions/260530_infer-case-stays-whole|infer-case-stays-whole]]). Its one
companion, `infer_case_arm`, is a premise of the rule rather than a surface
helper: an arm is syntax, so typing one — bind its pattern, infer its body,
force its payload to agree with the scrutinee at that label — is a judgment
that stands alone
([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).

## The argv rule, and the exec gate

`argv_ty` (`infer.rs`) is the one rule for every argv boundary — a handler arm, a
base frame, an external — and yields `Ty::argv()`, `List String`: each element is
inferred under its own span for errors inside it — as is each list-literal
element and each map-literal value — and constrains the argv not at all, a `...`
must still spread a list, and every element crosses rendered
([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).

It carries *which* boundary it is at — `ArgvBoundary::InShell` or
`Exec(shown)` — and that is the whole of the difference between them. `Exec`
sends each written element through `gate_exec_arg`, which reads
`RefusedArg::of_ty` (`core/src/types/exec_arg.rs`) — the same declaration
`runtime::command::vet` reads at the spawn — and raises
`TypeErrorKind::ExecArgNotText` (T0057) where the resolved shape is refused,
saying nothing about a type variable or about a spread's elements.
`explain.rs` composes the message and takes the guidance from
`RefusedArg::remedy`, so the static refusal and the pre-spawn one carry one
sentence per shape
([[invariants/exec-argv-is-words|exec-argv-is-words]],
[[decisions/260812_exec-boundary-gated-statically|exec-boundary-gated-statically]]).

## The pipeline rule

`infer_pipeline` (`infer.rs`) has no adjacency loop. It infers each stage,
forces it to ready `Return` shape with `force_ready_shape` under
`Reason::PipelineStageShape`, records the stage's value type in
`InferCtx::stage_types`, and returns the *final* stage's value as the
pipeline's own. Every stage but the last must write (`stage_writes`): it is
accepted at `Unit` in any grade, but a decoder is refused by its own mark
(`stage_decoder`, `DecoderMidPipeline`, T0078), and a value or block literal in
stage position under `Reason::PipelineStageWrites` (T0011). A stage typed `Fun` is a function still waiting for an argument; the
hint says to apply it rather than pipe into it.

One further premise, about a stage's redirects rather than its type: past the
first position, `stage_root_stdin_feed` reads the stage's root — an `Exec`'s
fused redirects, a `ScopeOp::Redirect` frame, or the same past the binders
elaboration hoists out of a redirect target — and a `< f` or `<< w` on fd 0
there is `TypeErrorKind::DeadPipeEdge` (T0070), whose `StdinFeed` names which
of the two the message spells. Nothing else about a stage is checked, and
nothing inspects an `Ast` node to decide whether a pipeline is well formed — so
an unforced block literal in stage position is an ordinary value-returning
stage, accepted, and a read nested inside a stage is left alone.

## Capture by type

A command is a computation of type `F^w Unit`, printed `Command`; its value is
its output ([[decisions/260930_graded-f|graded-f]]). `CompTy::Return(Grade,
Ty)` carries `Grade::{Value, Output, Var}`; the grade store lives in `unify.rs`
beside the type, computation and row stores, `Scheme.grade_vars` quantifies it,
and `force_return_shape` opens a computation variable into a fresh grade.

`HeadClass` and `head_class` live in `infer.rs`, beside `exec_comp_ty`: they give
a bare head the class the runtime's lookup order gives it: `Binding`,
`Value(entry)` (a builtin row), `Arm` (a handler or base frame standing in for
a head), or `External`. No separate pass decides capture.

`grade.rs` holds the grade rules. `rhs_bound_ty` (the `Bind` and
`Phrase::Define` right-hand side) is the bind rule: `Return(Output, _)` records
the right-hand side's address in `InferCtx::captured` and binds `String`,
`Return(Value, a)` binds `a`, and `Return(Var(g), a)` binds `g := Value` and
`a`. `adapt` says how a block meets a demand of the other producer kind — a
command where a value is wanted is `Adaptation::Capture`, a `()` producer
where a command is wanted is `Adaptation::Upcast` — and `coerce` records a
captured block and unifies the type it is read at with the demand, reporting
a failure between what the two produce past their parameters
(`InferCtx::unify_comp_ty_as`). `capture_block` records a literal block by
its body under *exactly* its arity of written lambdas in `captured`; anything
else — a value in hand, or a literal whose arrows are not all written, `map {
!$f } $xs` — goes in `InferCtx::captured_vals` with its arity and is η-wrapped
whole, since a `Capture` frame between a lambda and its pending argument would
drop the lambda. `apply_args_capped` applies the two to a block argument, and
`join_arms` to each arm against the join. `discharge` turns a redirected
stdout's grade to `Value`.

`annotate.rs` is a plain structural rebuild (`annotate_comp`, `annotate_val_at`).
It wraps each recorded node through the one constructor `captured_string`,
which builds `Capture(body) to x. Decode(x)` — `x` a fresh name from
`InferCtx::fresh_name` — with the captured node's span on both the bind and
the decode; a recorded value in hand becomes a lambda whose body captures the
forced call. `CompKind::Capture(body)` types as `F Bytes` with `body` unified
to `F^w Unit`; `CompKind::Decode(val)` types as `F String` with `val` unified to
`Bytes`. Both rules fire only when re-inferring a tree that already carries
`annotate`-inserted nodes — a stored handler or thunk re-checked at a later
install. Where a computation is held as a function of unknown arity,
`eta_expand_arrow` η-expands it so every function-typed thunk's body is a
syntactic `Lam`.

## Arms and joins

`{ … }` is a thunk in every position: `if` and `case` carry `Spanned<Val>`
arms ([[map/core/ir|ir]]), and `check_arm` types the one it is handed against
the type its form's arms share. `join_arms` (`grade.rs`; `if`, `infer_case`
and `infer_try` all route through it) joins arms in two passes: each arm is
inferred against a fresh result, then `join_target` settles the join on the
greatest of the arms' `Verdict`s — `Value` (`F^p τ`), `Command` (`F^w Unit`)
or `Open` (plain unification) — and each arm meets it as a block meets a
demand (`adapt`, `coerce`), command arms last so a disagreement is reported
against the value the other arms fixed; a `()` arm stands as a command only
where the join is one. A disagreement
is `T0010` / `T0020` between two values; `T0011` is a disagreement of shape or
grade. The join reasons — `IfBranches`, `CaseArms`, `TryArms` (shared by `try`
and `?`, which elaborates to nested `try`) — carry no writer: `explain.rs`
derives the hint from the two types, saying that one arm is a command, whose
value is its output.

`Inferencer::stands_in(name, arm)` holds an arm to the command it stands in
for. `stands_for` is the scheme of the base frame or arm already in force
under `name`, else `[String] → Command`; `Reason::StandsIn(Standing)` names
which (`Standing::{Command, Own, EveryCommand}`). `catch_all_stands_in` is the
same check for the catch-all `handler:`. `alias_arm_scheme`
(`typecheck.rs`) and the free `catch_all_stands_in` apply them at the install
door, and `HandlerEntry::vet` / `parse_catch_all` render the error through
`types::refused_arm`.

## Display and diagnostics

`fmt_comp_ty_ctx` (`fmt.rs`) renders `Return(Value, A)` as `Returns A`,
`Return(Output, Unit)` as `Command`, and `Return(Var, A)` as `ν A` with the
grade drawn from `ν ξ ο π` (`FmtCtx::grade_names`); a block that prints is
`{Command}`, one that returns text `{Returns String}`. Open
variant rows mark their tail with the same backtick as their arms
(``[`...]`` / ``[`...ρ]``), while record tails stay `[...]` / `[...ρ]`.

`TypeErrorKind::CompTyMismatch { expected, actual }` is T0011, with
`CommandNotFunction`; T0012 is retired and never renumbered. The two
constraint notes beyond the join hint are data on the error: `Reason::
PipelineStageWrites { stage, next }` (T0011) says a stage feeds the next by
writing, `DecoderMidPipeline` (T0078) that a decoder ends the byte pipeline, and `TypeError.unit` (`UnitCall`, recorded by `note_unit_read` from
`TyEnv::note_called` / `InferCtx::unit_reads`) says that a name `let` bound to
what a call returned is `()`, and what to write instead. `()` is neither a word
nor an interpolant (kind `scalar`, `RefusedArg::Unit`).

`docs/SPEC.md` has the typing judgments.
