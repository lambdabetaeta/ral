---
generated_at_commit: f5720be
generated_at_date: 2026-08-11
covers_paths: [core/src/typecheck/, core/src/typecheck.rs]
---

# Map: core / typecheck

`core/src/typecheck/` is Hindley–Milner inference over the CBPV
[[map/core/ir|IR]]. Types sit on `Val` and `Comp` after
[[map/core/elaboration|elaboration]].

Entry points (`typecheck.rs`):

- `typecheck(comp, SessionSchemes) -> Result<Comp, Vec<TypeError>>` — this
  function checks a program. On success it returns an *annotated* `Comp`.
  `annotate` rebuilds the IR after inference and writes three things onto
  it: a generalised `Scheme` on each top-level `Bind` node, resolved against
  the final unifier and closed by quantifying its residuals — that is,
  generalised against the empty environment; a `PipeYield` and a `Vec<Ty>` of
  per-stage value types on each `Pipeline`; and a `Capture` node wherever a
  value demand meets a computation whose payload route grounds `Bytes`
  ([[map/core/ir|ir]]). `infer_pipeline` records each stage's value type in
  `InferCtx::stage_types`, keyed by stage address, and the *pipeline's* own
  final route in `InferCtx::pipeline_routes`, keyed by the pipeline comp;
  `annotate` resolves both against the final unifier, and grounding that route
  is the **last** place a route is read — a `Bytes` pipeline yields `Unit`, so
  the checker's verdict leaves as syntax and no route reaches the evaluator. The stage types are
  typing metadata for the structural REPL, not a transport channel — the
  evaluator never reads them, so an un-annotated stage keeps the elaborator's
  `Unit` placeholder without harm. There is no per-stage annotation left,
  because there is no interior adjacency rule left to record
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
  The seed for a check is one `SessionSchemes { bindings, aliases,
  builtins }`
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]):
  the scope's name-to-`Option<Scheme>` map, the alias arms' schemes, and the
  shell's own `BuiltinTable`. Builtins are shell-scoped, so the checker
  types against exactly the surface the booted shell dispatches
  ([[map/core/builtins|builtins]]). `seed_env` is the one seeding routine.
- `bake_prelude(comp) -> (Comp, Vec<(String, Scheme)>)` — called by the consumer
  `build.rs`: returns the annotated prelude comp alongside the schemes harvested
  off its `Bind` nodes (`harvest_schemes`), one walk behind both the build-time
  bake and a run's installs.
- `alias_arm_scheme(head, param, body, SessionSchemes) -> Result<Scheme, PinFailure>`
  — infers an alias arm under the runtime handler calling convention, pins it to
  `head` (`Inferencer::pin_arm_to_head`), and closes it, for `install_alias` and
  `WithinScope::parse` to store on a frame. A handler or alias arm is a
  *fixed-arity lambda* — its calling convention is the surface form, not the
  runtime value's shape, so `param` is non-optional and `infer_alias_arm` types
  the arm `Fun(List(elem), body)`, forcing it on the argv list
  ([[invariants/fixed-arity|fixed-arity]],
  [[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
  Statically `infer_handler_comp` still types a non-`Lam` thunk (e.g. a computed
  `alias g $h`) by its bare body, binding it so `g x` is an arity mismatch
  rather than a silently discarded argument; the runtime install boundary is the
  sole complete gate on shape. `head_pipe_route` yields a *known* head's
  resolved route and a fresh variable for an unknown one, so reinterpreting a
  known head with an incompatible route is the rejected failure while a fresh
  alias defines its own.

The sorts split with CBPV:

- value types `Ty` describe data;
- computation types `CompTy` are `Return(PayloadRoute, Ty)`, `Fun(Ty, CompTy)`,
  and `Var`;
- records are open-row-polymorphic ([[design/row-types|row-types]]: `Row` /
  `RowVar`).

Generalisation happens at `Bind`; recursive bindings (`LetRec` / `Rec`) stay
monomorphic to keep generalisation sound.

Internals:

- `infer.rs` — the `Inferencer`; `infer_comp`;
- `route_solver.rs` — the deferred arm-result join: owns
  `InferCtx::route_constraints`, the only logic in the checker that decides a
  join by casing on a route's groundness;
- `unify.rs` — `Unifier`;
- `route.rs` — the payload-route types, private to `typecheck`;
- `ty.rs` — the data-only type definitions (`Ty`, `CompTy`, rows), re-exporting
  the route types from `route.rs`;
- `scheme.rs` — `Scheme`;
- `error.rs` — the error taxonomy: `TypeError` / `TypeErrorKind`, with
  constraint provenance as data (`Reason`, `CompDiff`), plus `PinFailure`, the
  two ways an arm can fail to install under a head;
- `explain.rs` — the single home of every user-facing type-checker sentence
  (hints and `TypeErrorKind::render_label`), a pure function of the error data
  so each message is unit-testable;
- `annotate.rs` — the write-back pass (`annotate`) that rebuilds the checked
  IR with schemes, pipeline yields, stage types, and `Capture` nodes;
- `generalize.rs`;
- `env.rs` — `TyEnv`, `InferCtx`;
- `fmt.rs` — type display;
- `builtins.rs` — per-builtin type rules (`fixed_arity`, `builtin_type_hint`),
  whose arity rules enforce [[invariants/fixed-arity|fixed-arity]];
- `scope.rs` — the five structural scope nodes.

`infer.rs`'s `infer_case` is left whole by decision
([[decisions/260530_infer-case-stays-whole|infer-case-stays-whole]]). Its one
companion, `infer_case_arm`, is a premise of the rule rather than a surface
helper: an arm is syntax, so typing one — bind its pattern, infer its body,
force its payload to agree with the scrutinee at that label — is a judgment
that stands alone
([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).

## The payload route

`typecheck/route.rs` is a leaf module knowing nothing of `Ty`. It holds four
types: `PayloadVar`, `PayloadRoute { Value, Bytes, Var }`, its resolved
counterpart `GroundRoute { Value, Bytes }`, and `RouteMismatch`. All carry serde
derives, because they ride inside a `Scheme` into the postcard-baked prelude.

The module is private to `typecheck`, and `GroundRoute` is `pub(in
crate::typecheck)`: routes cannot flow past annotation, because past annotation
their names do not exist. `PayloadRoute`, `PayloadVar`, and `RouteMismatch`
stay public — they are in `CompTy`, in `Scheme`, and in `Unifier`'s result, all
of which host crates write against.

`CompTy::Return(PayloadRoute, Box<Ty>)` is the whole annotation. The route says
which of a computation's two independent products a *value boundary* reads — the
evaluator's return or its stdout — and says nothing about whether stdout carries
anything ([[design/types|types]]). `Unifier::unify_route` is one plain method
demanding equality on ground routes; `CompTyKey::Return(PayloadRoute,
Box<TyKey>)` carries it into the one-sided-obligation fingerprint, so two
obligations differing only in route stay distinct
([[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]]).
`Unifier::fresh_route` mints an open one; `InferCtx::ground` defaults a residual
to `Value` at annotation time, the only defaulting site.

A builtin's route is the projection of its declared signature, read once:
`typecheck::builtins::sig_route` maps a `CompTemplate` onto a `PayloadRoute` —
`Pure` and `LinesStep` to `Value`, `Return { route, .. }` to its declared ground
route, and `Never` (`fail` alone) to a fresh variable, since a divergent
computation joins either side of a byte/value split. `ret_bytes()` builds the
byte shape and pairs it with `TyTemplate::Unit` at construction, so WF-2 holds
structurally for every encoder, `echo`, `help`, `explain`, and the terminal
controls. `external_exec_comp_ty` (`infer.rs`) gives every external command
`Return(Bytes, Unit)` for the same reason.

`scheme::fold_lines` is the one hand-written route: it mints a single variable
and uses it for both the callback's result and the reducer's, which is what
makes `map-lines` / `filter-lines` / `each-line` (prelude wrappers over it) take
their boundary behaviour from their callbacks. `spawn`, `watch`, and `service`
forward a route off the thunk they are handed. No builtin mints a route for its
*own* result: nothing but an alias pin could ever ground one.

## WF-2, carried by the one byte computation

`ρ = Bytes` implies a `Unit` return type. `PayloadRoute` and the value type
are independent fields, so the rule is carried by its consequence: there is
exactly one byte-routed computation type, `CompTy::bytes()` = `F[Bytes] Unit`
(`ty.rs`, the dual of `CompTy::pure`), and landing on the byte side means
unifying with it whole — no live code unifies a route against a detached
`Bytes`:

- `route_solver.rs`'s `conclude_byte_side` unifies each non-subsumed arm with
  `CompTy::bytes()` when a join lands on the byte side, open arms included;
- `infer.rs`'s `pin_arm_to_head` unifies the arm's value with `Unit` in the
  same breath as a pin that lands on bytes, returning
  `PinFailure::ByteHeadReturnsValue` rather than pinning a bare route and
  discarding the value type.

`alias_arm_scheme` refuses the install on either `PinFailure`;
`handler_comp_scheme` reports instead, mapping `Route` onto
`TypeErrorKind::RouteMismatch` (T0012) and `ByteHeadReturnsValue` onto a
`CompTyMismatch` (T0011) whose one `CompDiff::ReturnType` names `Unit` against
the arm's actual type, both under `Reason::HandlerRoutePin`. `HandlerEntry::vet`
(`core/src/types/handler.rs`) renders both at the runtime install door.

## The pipeline rule

`infer_pipeline` (`infer.rs`) has no adjacency loop. It infers each stage,
forces it to `Return` shape with `force_return_shape` under
`Reason::PipelineStageShape`, records the stage's value type, and returns the
final stage's `CompTy` unchanged. A stage typed `Fun` is a function still
waiting for an argument; the hint says to apply it rather than pipe into it, or
to read the incoming bytes with a decoder. Nothing else about a stage is
checked, and nothing inspects an `Ast` node to decide whether a pipeline is well
formed — so an unforced block literal in stage position is an ordinary
value-returning stage, accepted.

## The arm-result join

`route_solver.rs` owns one constraint, `ArmResults` — a plain struct, not an
enum — and `InferCtx::route_constraints` is its store. `join_arm_results` is the
single emission point, reached through `merge_branches` (for `if`, a `?`
fallback chain, and `case`) and `infer_try`. It first tries to *conclude*
against the unifier's current state, applying the conclusion immediately when
one exists — sound because a route only ever moves `Var → ground`, never back —
and otherwise stores an open constraint and returns a fresh target route and
value type.

The join runs under the one subsumption instance `Value Unit ⊑ Bytes`: a
byte-routed arm pulls the whole join onto the byte side and ties every arm's
value to `Unit`; no byte arm and every arm ground `Value` pulls it onto the
value side; any arm still open defers, even beside a ground
`Value`-at-non-`Unit` arm, because that open arm may yet ground `Bytes` and the
resulting conduit mismatch must be the join's own verdict.

The store drains through two entry points, and ownership is the difference.
`InferCtx::solve_at_boundary` runs at every in-inference point that produces a
`Scheme` (`infer.rs`'s `Bind` let-generalisation, `infer_letrec`'s group
fixpoint, and `handler_comp_scheme`) and solves only the constraints touching a
route variable not free in the environment — the variables that boundary is
about to quantify, computed by `generalize.rs::env_free_vars` over writable
positions (`owned_by_env`); a constraint wholly owned by the environment is left
untouched, neither collapsed nor retried, for its owning boundary.
`InferCtx::solve_and_finalize` is the terminal drain — the end of `typecheck`
before `annotate`, plus `alias_arm_scheme` and `binding_value_scheme`, which
generalise against an empty environment — and collapses everything. Each drain
retries to quiescence, since a conclusion can unblock a sibling, then collapses
what it owns: ground-directed residues first (`collapse_ground`, following the
grounded result's side with that side's full protocol), one at a time with the
worklist re-run between. No constraint outlives the generalisation of its
variables
([[decisions/260807_modes-solved-by-deferred-joins|modes-solved-by-deferred-joins]]).

## Display and diagnostics

`fmt_comp_ty_ctx` (`fmt.rs`) renders `Return(Bytes, _)` as
`Command captured from stdout` and every other `Return` as `Command A`, so a
stdout-captured command and a command returning a first-class `Bytes` never
differ by punctuation alone. An open route prints as nothing inside a `Command`
type; `fmt_route` / `fmt_route_ctx` print one on its own, which the mismatch
renderer is the only caller of — and the reason `absorb_comp` absorbs the route
into the shared variable-letter table, so two types sharing a route variable
give it a consistent letter. `fmt_scheme` does not quantify routes.

`CompDiff` has two variants, `Route` and `ReturnType`. `TypeErrorKind::
RouteMismatch` is T0012, raised only at handler and alias pins, and reads that
the two computations disagree about where their payload lives.

## Capture insertion

`CompKind::Capture(body)` types through `Inferencer::infer_comp`: its own route
grounds `Value`, its value type is `Bytes`. `CompKind::Decode(body)` is the
reading step over it, and its value type is `String`. Both rules fire only when
re-inferring a tree that already carries `annotate`-inserted nodes — a stored
handler or thunk re-checked at a later install.

`annotate.rs` inserts the coercion during its write-back walk, as demand
propagation, through the one constructor `captured_string`, which builds
`Decode(Capture(body))` with the captured node's span on both halves — two
nodes of syntax, so no name is resolved and no binder installed where the
checker composes them
([[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]]).
A `Demand` is `Value` or `Discard`. It reaches a `Seq`'s tail,
a `Bind`'s `rhs`, each arm of an `If`, `Chain`, `Try`, or `Case`, and the body
of a force of a syntactic thunk.
Where a `Value` demand meets a node whose recorded route grounds `Bytes`,
`annotate_demand` wraps it. `ArmWalk` (`Plain`, `Descend`, `Wrap`)
decides how a join arm is rebuilt; `Wrap` is the subsumption instance, wrapping
a whole `Value`-at-`Unit` arm so its capture contributes the empty string.
`annotate_join_arm` dispatches a `Comp` arm this way, and every `Case` arm is
one, since arms are syntax. An opaque scope arm has no arm syntax to wrap, so
`eta_expand_captured` η-expands it instead.

`docs/SPEC.md` has the typing judgments.
