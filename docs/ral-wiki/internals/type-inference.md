---
verified_at_commit: 584ed719
verified_at_date: 2026-09-21
anchors: [Inferencer, Unifier, Pairs, unify_row, unify_field, resolve_field, Field, Presence, PresenceVar, CachedFreeVars, unify_route, infer_record_val, infer_map_val, infer_field_read, generalize, instantiate, annotate, SessionSchemes, PayloadRoute, extract_return, force_return_shape, stage_root_stdin_feed, pin_arm_to_head, InferCtx, join_arm_results, solve_at_boundary, solve_and_finalize, ArmResults]
---

# Type inference: the algorithm

[[design/types|The type system]] states *what* is well-typed; this is *how*
`core/src/typecheck/` infers it — constraint-based Hindley–Milner over the CBPV
[[map/core/ir|IR]] after [[internals/compilation-ladder|elaboration]].

**The Inferencer walks the typed IR.** `infer.rs` traverses `Val` / `Comp`,
allocating fresh type, row, and route variables and emitting unification
constraints as it goes; `infer_case` is kept whole at ~100 lines by decision
([[decisions/260530_infer-case-stays-whole|infer-case-stays-whole]]). Builtin
signatures enter through per-builtin rules carried with the body
([[internals/builtins-registry|builtins registry]]: `fixed_arity`,
`builtin_type_hint`), the one source of a table entry's arity
([[invariants/fixed-arity|fixed-arity]]). The base-frame manifest's rows have no
arity at all: their schemes are seeded into the checker's env at boot, so a base
frame is looked up as a handler is
([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).

**A block argument is checked last, and against its own call.** `apply_args_capped`
unifies the arrow spine from the arguments in source order, but a `Val::Thunk`
argument contributes only `Thunk(α)` on that pass; its body is inferred after
the loop, against the `α` the spine has since ground. `check_comp` then pushes
that expectation inwards: a `Lam` met with a known `Fun` binds the parameter to
the type the expectation names instead of a fresh variable. Synthesis in source
order would check the body of `map { |x| … } $xs` while the element type was
still free, which is a difference the body can *observe* — the computed-key
index arm below is the rule that observes it, so `map { |x| $x[$x] } [1]` is
refused where the same lambda extracted into a `let` is not. That gap is a
known soundness hole and the deferral is what keeps it from widening; it is
pinned as it stands rather than claimed closed. Everything else has nothing to
push inwards and is inferred as ever, the caller unifying.

**The Unifier solves four sorts at once** (`unify.rs`):

- *Value and computation types* are equi-recursive — unified with **no** occurs
  check, so cyclic types are admitted rather than rejected. Termination rests on
  a co-inductive guard (`Pairs`): re-entering an in-progress equality obligation
  is an immediate success — the cyclic fixed point. The guard memoizes symmetric
  {ty, comp}-var *root pairs* **and** *one-sided* obligations — a var root against
  a finite structural key of the other side — so the same equi-recursive type
  anchored at a ty-var on one side and a comp-var on the other still converges
  rather than overflowing the stack
  ([[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]]).
  `CompTyKey::Return` fingerprints the payload route beside the return type, so
  two obligations differing only in route are never conflated, and `row_key`
  fingerprints a field *after* resolving its flag, so a slot keys the same once
  its absence resolves as the `Absent` it became — otherwise the guard stops
  recognising two types the field rule calls equal. That is the resolution
  half; the depth half is a resource bound no key closes (below).
- *Rows* unify by the Rémy rewrite (`unify_row`): an occurs check guards the
  tail variable, then mismatched labels are permuted past one another into a
  shared fresh tail ([[design/row-types|row-types]]). The `Empty`-against-
  `Extend` arms are a **peel**, not an error: `Empty` says every label off the
  spine is absent at the type the assignment gives it, so meeting a field there
  retires it — flag to `Absent`, payload to `δ_l` — and carries on. The two
  arms are symmetric, and with the field rule meeting an `Absent` they are the
  three sites that retire a label; a one-sided edit to any of them leaves an
  absence with two spellings, which is the one thing the equational theory
  cannot survive.
- *Presence flags* are a two-point union-find (`presences`), read through
  `resolve_field` — `resolve_row`'s twin, called from the same traversals.
  The field rule unifies flags and then payloads, **always**: there is no
  conditional, because the eager answer is the mgu (below). A `Present`
  against an `Absent` has no solution, and the row arm that matched the pair
  owns the message, being the only frame that knows the label.
- *Payload routes* unify **by equality** (`unify_route`): `Value`, `Bytes`, and
  route variables are first-class unification variables under one rule, and a
  ground `Value` does not unify with a ground `Bytes`. A clash is a live
  `RouteMismatch` (T0012), raised where two computations must genuinely be the
  same — a handler or alias arm against its head — and nowhere else. There is no
  adjacency premise: a `|` constrains no route at all
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

**A literal's kind is syntax, and a key's form picks the projection rule.**
The parser classifies a bracketed literal, so the checker has one rule per IR
constructor — `infer_record_val` and `infer_map_val`: a record's fields each
keep their own slot in a row, a map's values share one element type, and the
two types unify only with their own kind
([[design/records-and-maps|records-and-maps]]). A record literal is a **put**
over one base: it demands of the base a slot at each written label, at a fresh
flag over a fresh payload with nothing imposed on either, and returns those
labels `Present` at the types written over the base's own tail. So the rule
imposes nothing it does not need, an entry beats the base wherever it sits, and
the result is flat. Indexing splits the same way,
on the key rather than the target: a `Val::String` key is
`infer_field_read`, which unifies the target with `[label: α | ρ]` whatever its
current inference state, so an inline read and the same read extracted into a
block cannot disagree. The computed-key arm alone overloads on the target —
`List` by an `Int` key, `Map` by a `String`, a still-free target pinned by the
key's type — and is the one indexing verdict that depends on how much the
store already knows.

**One shape rule, `force_return_shape`, does the work an adjacency rule used to.**
`extract_return` (`infer.rs`) resolves a `CompTy` to `Return(route, value)`,
unifying against a freshly minted `Return` when it is still a variable. It is
how every value boundary reads a computation's two facts at once, and
`infer_pipeline` forces **every** stage through it under
`Reason::PipelineStageShape`: a stage typed `Fun` is a function still waiting
for an argument, and the diagnostic says to apply it rather than pipe into it.
The pipeline then takes its route and value type from one projection of the
final stage — never from peering past an arrow — and records the stage types for
the structural REPL along the way. The rule's one other premise reads no type at
all: `stage_root_stdin_feed` looks at a non-first stage's root redirects, and a
`< f` or `<< w` binding fd 0 there is `DeadPipeEdge` (T0070) — the feed answers
the stage's reads for its whole run, so the wire's producer writes for nobody
([[design/pipelines|pipelines]]).

**WF-2 is structural: the byte side is one computation.** `ρ = Bytes` implies
a `Unit` return type, and the checker cannot assume it: `PayloadRoute` and the
value type are independent fields, so the pairing must be established at the
moment a `Var` route becomes ground. WF-2's own consequence carries it — there
is exactly one byte-routed computation type, `CompTy::bytes()` =
`F[Bytes] Unit` — so landing on the byte side means unifying with that
computation whole, and no live code unifies a route against a detached
`Bytes`. Two operations land there:

- `conclude_byte_side` (`route_solver.rs`), when an arm join lands on the byte
  side — each non-subsumed arm unifies with `CompTy::bytes()`, open arms
  included;
- `pin_arm_to_head` (`infer.rs`), when a handler or alias arm is installed under
  a byte-routed head: the arm's value unifies with `Unit` in the same breath as
  the pin. A byte pin against a non-`Unit` arm is
  `PinFailure::ByteHeadReturnsValue`, reported by `handler_comp_scheme` under
  `Reason::HandlerRoutePin` and rendered by `HandlerEntry::vet` at the runtime
  install door.

**One join survives, and it is deferred.** `join_arm_results`
(`core/src/typecheck/route_solver.rs`) is the payload merge every arm form
funnels through — `merge_branches` for `if`, a `?` fallback chain, and `case`,
and `infer_try` for `try` — under the one subsumption instance
`Value Unit ⊑ Bytes`:

- an arm whose computation type is still a bare variable (a call to a function
  under inference, as in a recursive branch) is first forced to `Return` shape,
  so it joins rather than strict-unifies;
- a join with any byte-routed arm lands wholly on the byte side and ties every
  arm's value to `Unit`;
- with no byte-routed arm, a `Value`-at-`Unit` arm is the identity: still-open
  arms unify with one another and stay open, pinned to `Value` only by an arm
  carrying a genuine value payload;
- an arm still open when the join is raised defers, even beside a ground
  `Value`-at-non-`Unit` arm — that arm may yet ground `Bytes`, and the resulting
  mismatch must be the join's own verdict rather than foreclosed by pinning
  early.

A byte-routed arm alongside a `Value` arm at a non-`Unit` type is a type error:
the two disagree about where their payload lives. The fix is an explicit decoder
tail, `echo hi | from-string`, not an inserted coercion.

Two `Value` arms disagreeing on the payload's *type* is a different error, and
the diagnostic says so. The value side carries its own `Reason` — every arm is
already value-routed there, nothing about where the payload lives is in dispute,
and a decoder could not move an arm that is already on the value side.

**Conclude, store, solve-what-you-own.** `join_arm_results` applies whatever
conclusion is already determined and defers the rest as a stored `ArmResults` —
applying early is sound because a route only ever moves `Var → ground`, never
back, so an early conclusion cannot be invalidated later.
`InferCtx::solve_at_boundary(env)` runs at every in-inference point that
produces a scheme — the `Bind` let-generalisation, `infer_letrec`'s group
fixpoint, `handler_comp_scheme` — and solves exactly the constraints touching a
route variable not free in `env`: the variables about to be quantified, which a
constraint must not outlive. A constraint whose every variable is still free in
the environment belongs to an enclosing binding and is left *entirely*
untouched — not retried either, since a conclusion's side effects discipline
arms still under inference elsewhere, so running it at a boundary a syntactic
accident placed (any inner `let`, the elaborator's hoisted binds included) would
foreclose a sibling's `Value Unit` subsumption or move the join's error onto the
group unification. `InferCtx::solve_and_finalize` is the terminal drain — end of
check before `annotate`, plus the empty-environment `alias_arm_scheme` and
`binding_value_scheme` — where everything collapses. Each drain propagates to
quiescence, since one conclusion can determine a sibling, then collapses what it
owns: a constraint whose result grounded lands on that side with the side's full
protocol, one at a time with the worklist re-run between; only an all-open
residue equates, and a collapsed variable can still ground `Bytes` afterwards.
No drain defaults: `conclude_value_side` pins an open arm only once the join has
already landed on the value side, and it lands there only on evidence some other
arm supplied — an arm routed `Value` at a *solved* non-`Unit` type, which by then
has spent every chance to subsume onto a byte side. An arm whose value type is
merely not yet known to be `Unit` is **not** that evidence: reading absence of a
solution as a payload would make the verdict turn on how much of the store a
boundary happened to have solved, so that an equation added elsewhere — adding no
information the join lacked — could turn a rejection into an acceptance.

**Defaulting is declared, and it happens at two sites.** Where the program
supplies no evidence for a route, a stated rule picks `Value`. This is
ambiguous-type defaulting in the sense of Haskell's `default` declarations — a
rule of the language, applied at named positions, not an inference from anything
the program said:

- *The bind pin*, during inference (`infer.rs`, `Reason::RoutePin`). A binder
  observes its RHS's payload, so it must know where that payload lives: a `Fun`
  RHS is a lambda and is thunked unread, and otherwise `extract_return` yields
  the route, a still-open one is unified with `Value`, and only then is the bound
  type read (`String` on the byte side, the return type on the value side).
  Nothing pinned the route to `Bytes`, so there is nothing here to capture. The
  pin runs *before* the binder's own `solve_at_boundary`, so a deferred join
  bound to a name is decided by it: the join's result grounds `Value`, and
  `collapse_ground` takes it down the value side with that side's full protocol.
  Every `Bind` in the IR fires the rule, the elaborator's hoisted ones included.
- *The residue default*, at annotation (`InferCtx::ground`, `env.rs`). A route
  variable still unresolved when the checked IR is written reads `Value`. This is
  where the last open routes die: the per-node results driving `Capture`
  insertion, the scope arms' results, and the pipeline route the annotator reads
  to settle what a pipeline form yields (`annotate.rs`). `GroundRoute` has no
  variable case, so no open route survives into the checked IR.

Both rules are positional, not schematic: the pin fires on the route *instance*
at one binder, so a scheme that quantified a route variable stays
route-polymorphic and each use defaults on its own — `let x = !$spin 3` beside
`!$spin 2 | wc -l` typechecks.

**Principality holds up to those defaults, with its limits stated.** The claim
is over programs whose boundaries supply route evidence; where a program supplies
none, the two rules above decide, and the scheme is principal only relative to
them. Within that scope, typing verdicts do not depend on the order the solver
emits, retries, or collapses constraints: the join is a stored constraint
re-examined at the boundary that owns its variables rather than a decision read
off a partially-solved store; conclusions are monotone on a height-1 lattice,
ground-directed collapses are serialised through the worklist, and the all-open
residue equates by pure union-find merges. And no rule reads an arm's value type
for its *unsolvedness*, the one way a `Ty::Var` could smuggle the store's progress
into a verdict: the byte side imposes `Unit` on a subsumed arm rather than asking
whether it is `Unit` yet, and the collapse counts only a *solved* non-`Unit` type
as evidence of a payload.

Two limits are part of the claim. First, still-open arms joined under one binding
equate at the boundary that owns them, so route polymorphism holds *up to joined
arms sharing a variable*. Second, and this is the substantive one: `Value Unit ⊑
Bytes` is a subsumption, not an equation, so an arm still open when a join lands
on the byte side has two solutions, and the solver takes whichever side the store
already favours. `conclude_byte_side` pins such an arm to `Bytes`; the bind pin
would have read `Value`. Whichever rule reaches the variable first wins, and the
difference is visible in source — `{ |t| if true { echo hi } else { !$t }; let w =
!$t }` gives `w : String`, and the same two statements exchanged give `w : Unit`.
That is a genuinely unforced choice about which side a mixed join takes; it wants
declaring as a rule of the language, beside the two defaults above, rather than
proving away. Shape verdicts — the pipeline stage forcing, the sequence tail —
sit outside the claim; they are introduction-rule choices, not joins. See
[[decisions/260807_modes-solved-by-deferred-joins|modes-solved-by-deferred-joins]]
for the architecture, narrowed to this one constraint.

**The row algebra is unitary, and that is what carries principality over
rows.** The equation the design turns on is
`[x: θ·α] ≐ [x: θ'·String]`. Under an `Empty` that carried nothing it had two
incomparable solutions — `θ := Absent`, leaving `α` free, and `α := String`,
leaving the flags open — so there was no most general unifier and an eager
algorithm guessed. Under an `Empty` that means *absent at the type the
assignment gives this label*, it has one: `θ := Absent` retires the other side
too, and retiring it asks `String ~ δ_l`, so the killing solution *says
something* and is reachable from the eager one by instantiation. The eager
answer is therefore the mgu, the field rule may unify payloads unconditionally,
and principality over rows is the standard Hindley–Milner result over a unitary
algebra. Transitivity comes with it: a label has exactly one dead type, so two
rows cannot disagree about a dead payload and there is no second spelling of an
absence for a third row to disagree with. What this costs is one condition,
discharged as a rule rather than proved — *within one check, a label may not be
optional at two different ground types* — which binds the declared option rows
and nothing a program can write, a payload meeting `δ_l` only when its flag
dies.

**Two obligations are carried rather than discharged, and neither is a
theorem.** *Erasure* — that forgetting a payload behind an absent field lets no
program go wrong — rests on there being no elimination form for presence: `$r[l]`
demands `Present`, both pattern contexts demand fields, a put overwrites rather
than reads, and `has` / `get` / `keys` / `union` are map operations, so for a
value whose runtime shape its type describes the erased type describes exactly
what the unerased one did. The condition is not decoration: ral has operations
that traverse the *runtime* record rather than its type (`to-json`,
`values_equal`, `value_ordering`) and boundaries that hand out values no rule
has checked — `from-json` and `from-csv`, `use`'s asserted row, `$ENV`, session
values restored without a scheme. So the claim is *erasure is sound relative to
shape-correct values*, with those boundaries named as the assumption. Erasure
adds no reflective operation and widens no boundary; it inherits the ones there
([[decisions/260921_a-field-is-a-flag-and-a-type|a-field-is-a-flag-and-a-type]]).

*Recursion* is the second, and here the **policy is chosen and the proof is
owed**. The occurs check descends through payloads as well as along the spine.
The alternative — occurs along the spine only — would admit
`ρ = (x: Present·Record(ρ) ; Empty)`, a cycle anchored at no type or
computation variable: `apply_row_inner` carries no row-root guard,
`cyclic_roots_in_ty` cannot find it, and `Scheme` has no `row_bindings` to
snapshot it into. So spine-only is not an edit to the occurs check but a third
recursive sort, and this tree takes the descending policy. Presence changes
only that the descent consults `resolve_field` first.

`MAX_UNIFY_DEPTH` is a **resource bound and not a termination result**, and
presence-aware keys close only half of what it touches. The half they close is
*resolution*: a slot keys the same once its flag is dead as the `Absent` it
became, so the guard keeps recognising one obligation across the change. The
half they do not is *depth*, in both directions. `unify_ty_inner` fingerprints
its structural operand before resolving the other side, so an obligation can
spend the budget on a payload whose flag would have died had the absence
constraint been taken first; and retiring a field is itself the equation
`τ ~ δ_l`, so a payload nested past the ceiling cannot be retired at all —
unifying any structure against a variable fingerprints it, which is the cost
every rule pays and not one presence added. A resolved-absent slot holding an
over-deep payload is therefore unreachable: the budget refuses to build one.
What is guaranteed throughout is a graceful `TypeTooDeep` rather than a blown
stack.

**A route variable is quantified only in a declared slot or a forwarded pair.**
A builtin's computation-typed argument (`spawn`, `watch`, `service`, and the
`map`/`filter`/`each`/`fold` callback family), a scope's expected arm shape, and
`fold-lines`, which reads a route off its callback and hands it back beside the
value type it was paired with. `instantiate` refreshes each at every use; a call
site's actual argument grounds it, and otherwise it stays quantified inside an
arrow nothing consults. A deferred join mints a target route of its own, so the
restriction is stated over lifetime rather than origin: **no route variable
outlives its owning drain except as an alias of a declared slot variable or of a
ground route.**

**Generalisation is at the binding boundary** (`generalize.rs`). At each `Bind`
the inferencer takes the type's free variables minus those still free in the
environment and closes over the difference into a `Scheme`; `instantiate`
refreshes a scheme's bound variables at each use. Generalisation walks the
type structurally and unbudgeted, where unification charges a depth ceiling:
the walks are linear in a type the source built one constructor per statement,
and only unification descends past what the parser saw
([[decisions/260812_depth-is-guarded-where-it-multiplies|depth-is-guarded-where-it-multiplies]],
which also records checking's superlinear cost in nesting depth). The order
follows the SCC
structure the elaborator found — a non-recursive group generalises at its binding
point, a mutually recursive group stays monomorphic until its fixed point — which
is what keeps generalisation sound. A type error aborts with a positioned
expected-vs-inferred message (`fmt.rs`), where a byte-routed computation reads
`Command captured from stdout` and every other reads `Command A`.

**The quantifier is the prefix, not a binder.** A `Scheme` is the body's
ordinary `Ty` under a ∀-prefix of five `Vec`s of variable ids — value
(`ty_vars`), computation (`comp_ty_vars`), route (`route_vars`), row
(`row_vars`) and presence (`presence_vars`) sorts, each a `u32`-tagged unifier
root (`scheme.rs`). A flag has no structure, so the presence sort needs no
bindings map. There is no binder node and no de Bruijn index: a variable is
**bound iff it is listed**. A presence variable is not *named* in the printed
prefix — it has nothing a reader could say about it — so where sharing between
two instantiations matters, the test asserts on unifier roots rather than on
rendered text.

- *Elimination is substitution-with-freshening.* `instantiate` mints a fresh id
  in the current unifier for every listed variable and substitutes it through
  the body, so capture is impossible by construction — the fresh ids did not
  exist before the call.
- *Recursive types are not syntax but μ-equations attached to the prefix.*
  `comp_ty_bindings` / `ty_bindings` carry `(root, applied-binding)` pairs for
  each cycle in the body; `instantiate` re-ties them in fresh union-find slots,
  so two uses of one recursive scheme never share a cycle root.
- Because the prefix is nominal-by-listing, an open scheme leaving its minting
  unifier aliases another's variables: see [[invariants/schemes-leave-closed|schemes-leave-closed]].
- *A cached residual can be re-homed, so the cache's own check is narrower than
  its sorts.* `CachedFreeVars` holds the free variables a scheme did **not**
  quantify, and `env_free_vars` trusts it rather than re-walking. Erasure kills
  no variable — a retired payload is *united* with `δ_l` rather than dropped —
  but that union is what moves it: a residual `α` stops being its own canonical
  root the moment a flag beside it resolves to `Absent`. So the standing
  `debug_assert` that residuals are live roots covers the computation, route and
  presence sorts, and states the value sort's exclusion rather than asserting a
  property retirement can take away.

**The verdict survives into the next run.** The checker is a transformation:
on success `annotate` writes each top-level name-bind's generalised `Scheme` onto
its `Bind` node — and each `Pipeline`'s yield marker and per-stage value
types onto the node — the scheme closed against the empty environment so it
carries no residual variable that could alias the next run's fresh ids. The next
run's check is seeded from the live session — one `SessionSchemes` (the scope's
name→scheme map plus the alias arms' schemes) — so a name bound in run *N* is
checked at its inferred scheme in run *N+1*; a name from an unchecked path (a
`source`d file, a plugin) is `None` and infers afresh
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

See also [[design/types|types]]; map [[map/core/typecheck|typecheck]]. Judgments:
`docs/SPEC.md` §17.3.
