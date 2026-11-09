---
generated_at_commit: 3e3fbc84
generated_at_date: 2026-11-03
covers_paths: [core/src/typecheck/, core/src/typecheck.rs]
---

# Map: core / typecheck

`core/src/typecheck/` is Hindley–Milner inference over the CBPV
[[map/core/ir|IR]]. Each rule builds its checked node beside its type after
[[map/core/elaboration|elaboration]]; a successful unit freezes its sites.

Entry points (`typecheck.rs`):

- `typecheck(phrases: &[Spanned<Phrase<()>>], SessionSchemes, Option<ReturnContract>)
  -> Result<Toplevel, Vec<TypeError>>` — infer phrases in order, extending
  `TyEnv` at each `Define`, and build `Phrase<DefineSchemes, Pending>` nodes.
  Rules immediately construct capture/decode, Let-fun, returned-thunk splits,
  boundary saturation and imported protection. Each `Define` carries its
  harvested schemes unsettled. After nested checks and return-contract
  ascription succeed, one `UnitId` is made and `freeze_toplevel` snapshots
  sites, settles schemes with `settle_weak`, and builds `Toplevel.admits`
  from first residual uses. A unit with errors is refused whole and its
  built IR dropped without freezing ([[map/core/ir|ir]]).
  A pipeline's value is its final stage's, with no per-stage type annotations
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  The seed for a check is one `SessionSchemes { bindings, aliases,
  builtins }`
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]):
  the scope's name-to-`Option<Arc<Scheme>>` map, the alias arms' schemes, and
  the shell's `Manifest` — Σ, the checker's own view of the builtins: one `Decl`
  per row (name, `Convention`, doc, type rule, diagnostic, boundary flag, and
  one cached curry spine for arity and settle-at-unit), never a body. The
  table projects each installed set into it once; `SessionSchemes::new(manifest)`
  and `SessionSchemes::from_prelude(&Toplevel, manifest)` build a seed with no
  live shell. A scheme never changes once built, so one
  `Arc` carries it from the `TyEnv` onto `Phrase::Define`, into the scope's
  `Binding`, and back into the next run's `TyEnv`: neither seeding nor a
  lookup copies one. Builtins are shell-scoped, so the checker
  types against exactly the surface the booted shell dispatches
  ([[map/core/builtins|builtins]]). Filesystem query types come from
  `fact::{DirEntry, FileInfo}` (`fact/fs.rs`), not builtin bodies. `seed_env` is the one seeding routine, and
  the manifest reaches it as two: a table entry's rule, and a base-frame row's
  scheme, which lands in the env so that `lookup_handler` finds a base frame as
  it finds a handler
  ([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).
  Every name the seed and the program bind is therefore known to the checker: a
  `$name` outside them is `UnboundVariable` (T0071), and a binding of a
  non-thunk run as a program (bare head, `$name args`, `!$name`) is
  `HeadBoundToValue` (T0072)
  ([[internals/type-inference|type-inference]]).
- `bake_prelude(phrases, &Manifest) -> Toplevel` — called by
  `shell::boot::bake_prelude` from each host's build script: returns the
  annotated prelude `Toplevel` and nothing else. Every phrase already carries
  its schemes, and `SessionSchemes::from_prelude` reads them back through
  `Toplevel::exported_schemes`, so no second blob of schemes is baked.
- `alias_arm_scheme(head, arm, SessionSchemes) -> Result<Scheme, Box<TypeError>>`
  — holds an installed arm's closure to the arm rule (`Inferencer::arm`,
  `Arm::Named(head)`), seeded by `Shell::closure_schemes` from what the
  closure captured, and closes it, for `HandlerEntry::vet` to store on an
  alias frame; `catch_all_stands_in` is the same for `within [handler: $k]`.
  These one-shot inference sessions discard their constructed IR.
  An arm's parameters are its slot's — the argv, or the name and the argv —
  and its arity is its type, never its spelling, so `{ forward 'git' }` is as
  unary as `{ |args| … }` ([[invariants/fixed-arity|fixed-arity]],
  [[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).

The sorts split with CBPV:

- value types `Ty` describe data;
- computation types `CompTy` are `Return(Grade, Ty)`, `Fun(Ty, CompTy)`, and `Var`;
- records are open-row-polymorphic ([[design/row-types|row-types]]: `Row` /
  `RowVar`).

Generalisation happens at bindings; recursive members (`Rec`) stay
monomorphic while their group is inferred, then generalise at their bindings.

Internals:

- `infer.rs`: the `Inferencer`; `infer_comp`; `infer_prim` types every
  value-only primitive; `infer_update` is the record-update rule;
  `infer_index_step` matches
  `Key::{Label, Position, Data}` and immediately requires a record row, a list,
  or a map with a `String` key, respectively. No read constraint is deferred
  and no read makes variables weak;
- `grade.rs` — the bind rule (`rhs_bound_ty`, `Bound::{Ran, Thunked, Captured}`),
  fresh producers (`produces`), source-order joins (`join_arms`), and
  `redirected`, which requires outer `Return` shape and gives stdout a fresh
  result grade without constraining its input grade. `spine` reads parameters
  and tail; `map_producer` rebuilds a producer past its parameters for arms;

- `unify.rs` — `Unifier`; binding refuses a cycle that crosses no data (`CyclicType`, T0073)
  and a head its variable's kind does not admit (`KindMismatch`, T0074);
  the weak set (`mark_weak`, inherited through `unite`), which `generalize.rs` subtracts,
  `seed_env` re-seeds (`reseed_weak`) and `typecheck` settles (`settle_weak`).
  Each union-find store is typed by its sort (`Store<T: Unifiable>`, keyed by
  `T::Var`: `TyVar`, `CompTyVar`, `RowVar`, `GradeVar`), so a type root cannot
  index the computation store, and `Scheme`'s cyclic bindings and `Visited`
  carry typed roots too. `reach` is the one guarded walk over the variables
  under a `ty::Term` (whose `children` is the one enumeration of sub-terms):
  free-variable collection and the weak-source search are both a fold over it;
- `Site` (data in `core/src/ty/site.rs`, built by `typecheck/site.rs`, walked
  and admitted by `core/src/shell/runtime/admit.rs`) — the type solved at a boundary
  call, placed on the inferred `Exec` as `Pending::Open(ty)` and snapshotted
  at the successful unit's end by freeze. Sites can retain free weak variables
  and delayed contracts; they need not be ground. `Site::exports` hands the
  module check one scratch unifier over a whole export record, so export types that share variables are
  checked together; the unifier keeps the span that first bound each variable so a
  refusal can point at the use that imposed what the value met;
- the type language (`Ty`, `CompTy`, `Grade`, rows, `Kind`, `Scheme`, `Site` data) is
  [[map/core/ty|ty]], below this module;
- `error.rs` — the error taxonomy: `TypeError` / `TypeErrorKind`, with
  constraint provenance as data (`Reason`, `Standing`, `UnitCall`);
- `explain.rs` — the single home of every user-facing type-checker sentence
  (hints and `TypeErrorKind::render_label`), a pure function of the error data
  so each message is unit-testable. Its wildcard-free `Reason` match gives each
  reason prose or deliberately lists it as hintless: the constraint's other
  side is fresh, or the error kind is already its own diagnosis;
- `freeze.rs` — `freeze_toplevel` maps pending sites to final sites, settles
  `Define` schemes and builds unit readmissions in `Toplevel.admits`.
  `Open(ty)` snapshots under the unit's final unifier; `Kept(site)` preserves
  the existing `Arc`. `Imported(ty)` occurs only on `Val::Protected` and
  retains the membrane only when the snapshot `has_delayed()`.
  Group rebuilding is memoised by `Arc::as_ptr`. `Prim::map_operands` and
  `Redirects::map` perform structural conversion here; primitive inference
  sub-rules construct their own variants in their required inference order.
  Freeze makes no capture or typing decisions;
- `generalize.rs` — quantification, capture-avoiding instantiation and weak
  reseeding over the whole scheme graph; settlement substitutes live weak-variable
  images and resolves residual grades, even with no weak variables, leaving local
  ∀/μ names opaque. It quantifies remaining free grades and clears cached grade
  residuals. Closure is syntactic over the body and equations. Cached residual ids
  are seeds resolved through the current unifier, not assumed to remain unbound
  canonical roots
  ([[invariants/schemes-leave-closed|schemes-leave-closed]]);
- `env.rs` — `TyEnv`, `Pending`, `InferCtx`: the unifier, errors and position;
  unit-read diagnostics and nested checks; recursive member types plus their
  shared pending group, memoised by source `Arc` identity; residual bindings
  and first-use types; and the synthetic-name counter. Synthetic names are
  minted in inference order;
- `site.rs` — `impl Site { snapshot, instantiate, scheme, exports }`: freezing
  solved boundary and imported-weak occurrence types, preserving sharing and
  weakness on reinstantiation, and rebuilding authoritative closed contracts
  for protection-node re-inference and module exports;
  type display is `Display` for `Ty` and `Scheme` (`core/src/ty/fmt.rs`);
- `builtins.rs` — Σ: `Decl`, a manifest row minus its body (name, `Convention`,
  doc, type rule, diagnostic, `boundary`, and one lazily computed `Spine` of arity
  and settles-at-`Unit`), and `Manifest`, the sets of them a table projects at
  install; the scheme factory each entry names, and the readings taken
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
  key by key, in written span order even though the `FieldsNode` is sorted:
  unknown key T0076, refused key T0026, a clash at the key's own sentence.
  `Holds::CatchAll` (`within`'s `handler`) alone takes contextual
  handler inference at the written form; `Holds::Shaped` infers an ordinary
  value and unifies it with the minted type, and `Holds::Table` queues the
  value's nested check with its span (the value is inferred normally and never
  constrained). One helper builds the expected type for inline checking and
  scratch ascription;
- `contract.rs` — the declared tables themselves (`within`, `grant`, the rc
  file, the plugin manifest), each a closed keyset whose labels are held at a
  ground type, left to their decoder, or refused with a sentence of their own.
  A table's key holds a ground type, a shaped type minted from the Rust enums
  (`EditorVerb`, `ShellVerb`, `XdgKind`), a nested `Table` or a catch-all.
  `ascribe` holds a contract file's finished return to its table, after
  inference and in a scratch copy of the unifier (unknown key T0076, not a
  record T0077, and the row errors); a nested table is ascribed by the same
  recursive function, from the queue `typecheck` drains once, after inference
  and before the contract ascription (a map or `()` where
  a nested table belongs is refused as a non-record). The one-shot sessions
  drop their queue unread. A clash at a closed row names the row's tags and the
  nearest one. It is the single registration point, so the runtime decoders that read the same
  keysets — `RcPlan::decode`, `LoadedPlugin::parse`, `decode_capability_map` —
  dispatch off it rather than each keeping a copy. No table reaches the
  unifier ([[decisions/260930_a-table-never-enters-the-unifier|a-table-never-enters-the-unifier]]).

`infer.rs`'s `infer_case` is left whole by decision
([[decisions/260530_infer-case-stays-whole|infer-case-stays-whole]]). It binds
each arm's payload to the scrutinee's known payload at that label (a fresh type
if unknown), so known payloads constrain arm parameters before bodies are
inferred, including arms that forward a thunk payload. `join_arms` then infers
the bodies and joins their grades, and the scrutinee row is unified with
exactly the arms' label set. An arm is syntax, so that row is always closed
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
sends each written element through `gate_arg`, which reads
`RefusedArg::of_ty` (`core/src/ty/exec_arg.rs`) — the same declaration
`shell::exec::command::check_argv` reads at the spawn — and raises
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
`Reason::PipelineStageShape`, and returns the *final* stage's value as the
pipeline's own. Every stage but the last must write (`stage_writes`): it is
accepted at `Unit` in any grade, but a decoder is refused by its own mark
(`stage_decoder`, `DecoderMidPipeline`, T0078), and a value or block literal in
stage position under `Reason::PipelineStageWrites` (T0011). A stage typed `Fun` is a function still waiting for an argument; the
hint says to apply it rather than pipe into it.

One further premise, about a stage's redirects rather than its type: past the
first position, `stage_root_stdin_feed` reads the stage's root — an `Exec`'s
fused redirects, a `CompKind::Redirect` frame, or the same past the binders
elaboration hoists out of a redirect target — and a `< f` or `<< w` on fd 0
there is `TypeErrorKind::DeadPipeEdge` (T0070), whose `StdinFeed` names which
of the two the message spells. Nothing else about a stage is checked, and
nothing inspects an `Ast` node to decide whether a pipeline is well formed — so
an unforced block literal in stage position is an ordinary value-returning
stage, accepted, and a read nested inside a stage is left alone.

## Capture at a binding

**Only `Bind` and `Phrase::Define` capture.** `CompTy::Return(Grade, Ty)`
pairs `Grade::{Value, Output, Var}` with one value type. `Scheme.grade_vars`
quantifies grades, and `force_return_shape` opens a computation variable with
a fresh grade ([[design/capture|grades and capture]]).

- `rhs_bound_ty` reads the resolved RHS: `Fun` binds a thunk;
  `Return(Output, Unit)` captures and binds `String`; `Return(Value, a)`
  binds `a`; a free grade fixes to `Value`; a computation variable unifies
  with `Return(Value, fresh)`. This is the sole default.
- `infer_let` applies the returned `Bound` to its inferred RHS node:
  `captured_string` builds `Capture(body) to x. Prim::Decode(x)` with the
  RHS span on both nodes; Let-fun builds `return (thunk M)`.
  No argument or arm is wrapped.
- `CompKind::Capture` checks a command body and returns bytes at a fresh
  grade; `Prim::Decode` checks bytes and returns text at a fresh grade.
  Re-inference preserves those explicit boundaries.
- `produces` gives term producers fresh grades. `Decl::scheme`
  (`builtins.rs`) freshens ordinary builtin results, leaving commands,
  boundaries and already graded results alone. Boundary results stay `p`.
  `map` accepts either callback grade; `filter`/`sort-list-by` demand values.
- `Unifier::unify_producers` is shared by returners and handles: `Output`
  immediately refuses an opposing type that cannot admit `Unit`. For
  output-capable grades, a Unit-capable variable or differing Unit capability
  requires payload validation before grades join: failure must leave no
  hazardous alias. Pairs containing known `Value` and other safe pairs keep
  grade-first unification.
- `redirected` forces outer return shape under `Reason::RedirectShape`;
  a partial application is refused. Stdout produces at a fresh grade with
  the input payload type; stdin/stderr and `2>&1` preserve the input grade.

`HeadClass`/`head_class` (`infer.rs`, beside `exec_comp_ty`) share the
runtime's lookup order: `Binding`, `Value(entry)`, `Arm`, `External`.

## Application and binders

`autoderef_thunk_return` records each argument index before which a returned
thunk must be forced, once per unwrapped thunk. `apply_args_capped` returns
these positions with the inferred arguments; the `App` or binding-resolved
`Exec` rule builds explicit bind/force segments. Repeated indices give
repeated forces before one argument.
Redirects and binding capture wrap the whole split call. Re-inference is
idempotent ([[invariants/ir-pure-cbpv|the IR invariant]]).

`apply_args_capped` applies arguments without coercion. `demand_block` checks
a literal block against its parameter, then unifies with the parameter as
expected; values in hand meet the parameter directly. No η-wrapper is added.

An `App` head shaped as `Force(v)` is checked directly under
`Reason::AutoderefHead`, including an unsolved bare-name head, preserving
head diagnostics without suggesting an unwritten `!`. Arguments are a fixed
slice of values. Elaboration refuses a spread into a written value head or
lexically bound head; `exec_comp_ty` refuses one when a bare head resolves to
a builtin or session binding. Argv spreading remains separately checked.

`Lam` binds one monomorphic name. Destructuring parameters are binds after
the lambda chain, at the parameter spans, so a mismatch points there during
type checking as at runtime. Their types remain free in the environment and
cannot generalise. `Bind` always uses let inference; `Seq` uses statement
inference without generalisation.

## Arms and joins

`{ … }` is a thunk in every position: `if` and `case` carry `Spanned<Val>`
arms ([[map/core/ir|ir]]). `join_arms` (`grade.rs`, used by `if`, `infer_case`
and `infer_try`) checks each arm against its own fresh result, then unifies
those results with the join as expected, in source order. A `case` supplies
known payload types before body checking. No verdict selection, coercion or
arm reordering occurs; a disagreement blames the later arm.

The reasons `IfBranches`, `CaseArms`, `TryArms` retain provenance. In
`explain.rs`, `command_against_value` identifies the output side even when
the other grade is free. `grade_hint` distinguishes the blamed arm from an
earlier one and suggests explicit capture for text, `succeeds` for Bool, or
a returned/decoded value of the required type. Argument hints use the same
type distinction.

`Inferencer::arm(block, Arm)` is the one arm rule: the block meets the
parameters its slot hands it (`Arm::Named`, the argv, under
`Reason::AliasParam`; `Arm::CatchAll`, the name and the argv, under
`Reason::CatchAllParams`), and its producer unifies directly with its head
producer. `stands_for` is the scheme of the base frame or arm already in force
under the name, else `[String] → Command`; `Reason::StandsIn(Standing)` names
which (`Standing::{Command, Own, EveryCommand}`). The checker raises a refusal
where the arm is written (`checked_arm`, `install_arm`); `alias_arm_scheme`
and `catch_all_stands_in` (`typecheck.rs`) apply the same rule at the install
site, and `HandlerEntry::vet` / `parse_catch_all` render the error through
`shell::runtime::refused_arm`.

## Display and diagnostics

`fmt_comp_ty_ctx` (`ty/fmt.rs`) renders `Return(Value, A)` as `Returns A`,
`Return(Output, Unit)` as `Command`, and a free grade as `Command[ν] A`, with
names from `ν ξ ο π`. If the paired type excludes unit, a free grade prints
`Returns A`: concrete non-unit types, and quantified variables whose kinds
exclude unit. `FmtCtx` names and quantifies only visible grades. Handles
print their suspended producer, such as `Handle (Command[ν] α)`.
Open variant rows mark their tail with the same backtick as their arms
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
