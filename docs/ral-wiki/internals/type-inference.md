---
generated_at_commit: b1ba0713
verified_at_commit: b1ba0713
verified_at_date: 2026-11-03
anchors: [Inferencer, Key, infer_index_step, Unifier, Kind, Kinded, admit, unite_tys, Pairs, bind_ty, bind_comp_ty, unify_row, infer_update, CachedFreeVars, WeakVars, mark_weak, reseed_weak, settle_weak, infer_record_val, infer_map_val, generalize, instantiate, freeze_toplevel, Pending, SessionSchemes, extract_return, force_return_shape, stage_root_stdin_feed, InferCtx, head_class, arm, join_arms, rhs_bound_ty, unify_producers, stage_writes]
---

# Type inference: the algorithm

[[design/types|The type system]] states *what* is well-typed; this is *how*
`core/src/typecheck/` infers it — constraint-based Hindley–Milner over the CBPV
[[map/core/ir|IR]] after [[internals/compilation-ladder|elaboration]].

**The Inferencer builds checked IR beside its types.** `infer.rs` traverses
`Val` / `Comp`, allocating fresh type, computation and row variables and
emitting unification constraints as it constructs pending nodes.
`infer_case` is kept whole by decision
([[decisions/260530_infer-case-stays-whole|infer-case-stays-whole]]). Builtin
signatures enter through per-builtin rules carried with the body
([[internals/builtins-registry|builtins registry]]: `fixed_arity`,
`builtin_type_hint`), the one source of a table entry's arity
([[invariants/fixed-arity|fixed-arity]]). The base-frame manifest's rows have no
arity at all: their schemes are seeded into the checker's env at boot, so a base
frame is looked up as a handler is
([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).

**A name is known statically, or it is refused.** A `$name` resolves against the
lexical and session bindings, then the value builtins, then the language's own
`true` and `false` (`LANGUAGE_CONSTANTS`, typed `Bool`); a miss is
`UnboundVariable` (T0071), never a fresh variable, and its hint ranks the nearest
names in scope by edit distance (`text::near_names`, shared with the record-key
decoder). The runtime's "undefined variable" survives only as a defensive
error. The same rule holds at a head: a binding met as a bare head is applied at
its thunk's computation type, a binding of still-unknown type is taken to be a
thunk, and a binding of anything else is `HeadBoundToValue` (T0072) —
`instantiate_comp` never mints a computation variable for it. A bound bare
head is elaborated as `Force` of the variable; the `App` rule reads its type
under `Reason::AutoderefHead`, including when it is still an unsolved variable,
so it does not suggest a `!` operator the user never wrote. A variable head in `App`
(`$date +%s`) is named the same way: a user's own `let ls = …` then `ls /tmp`
is told of the binding and of `^ls`, not of a `!` it never wrote; the hint
offers `^ls` conditionally, since the checker never reads `PATH`. T0072 stands on the
head's own span (`Ast::Call`'s head is `Spanned`) and names the value's shape
(`describe`), never its full type, which for a record can run to kilobytes.

**A block argument is checked last, and against its own call.** `apply_args_capped`
unifies the arrow spine from the arguments in source order, but a `Val::Thunk`
argument contributes only `Thunk(α)` on that pass; its body is inferred after
the loop, against the `α` the spine has since ground. `check_comp` then pushes
that expectation inwards: a `Lam` met with a known `Fun` binds the parameter to
the type the expectation names instead of a fresh variable. Synthesis in source
order would check the body of `map { |x| … } $xs` while the element type was
still free. Read rules do not depend on that knowledge: `map { |x| $x[$x] } [1]`
and the same lambda extracted into a `let` are both refused by immediate
unification. Everything else has nothing to push inwards and is inferred as
ever, the caller unifying.

**The Unifier solves types and rows at once** (`unify.rs`):

- *Value and computation types* are equi-recursive, but **every cycle must cross
  data**: binding a variable to a structure that reaches it along `U`, `→`, `F`
  and `Handle` alone is refused as `CyclicType` (T0073) by a search at the two
  bind sites (`bind_ty`, `bind_comp_ty`), which stops at a list, map, record or
  variant ([[decisions/260930_recursion-is-guarded-by-data|recursion-is-guarded-by-data]]).
  A cyclic scheme prints with a μ. Termination of unifying the cycles that remain rests on
  a co-inductive guard (`Pairs`): re-entering an in-progress equality obligation
  is an immediate success — the cyclic fixed point. The guard memoizes symmetric
  {ty, comp}-var *root pairs* **and** *one-sided* obligations — a var root against
  a finite structural key of the other side — so the same equi-recursive type
  anchored at a ty-var on one side and a comp-var on the other still converges
  rather than overflowing the stack
  ([[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]]).
  `row_key` fingerprints each slot's type. The depth of a key is a resource
  bound no key closes (below).- *Rows* unify by the Rémy rewrite (`unify_row`): an occurs check guards the
  tail variable, then mismatched labels are permuted past one another into a
  shared fresh tail ([[design/row-types|row-types]]). The A field met against
  `Empty` is an error: `Empty` says every label off the spine is absent, so the
  record on the `Extend` side has a label the other lacks, and the arm that
  matched the pair owns the message, being the only frame that knows the label.
  A label on both sides unifies its two types.

**A literal's kind is syntax, and a key's form picks the projection rule.**
The parser classifies a bracketed literal, so the checker has one rule per IR
constructor — `infer_record_val` and `infer_map_val`: a record's fields each
keep their own slot in a row, a map's values share one element type, and the
two types unify only with their own kind
([[design/records-and-maps|records-and-maps]]). A record literal with a spread is an
**update** of one base (`infer_update`): it demands of the base a slot
at each written label, at a fresh payload with nothing imposed on it, and
returns those labels at the types written over the base's own tail. So a field
may change its type, a spread never adds a label, and the result is flat.
All written keys, including constant double-quoted strings, name record fields;
any computed key makes a map, as does the explicit `[:, …]` opener.

Indexing matches the IR's `Key`, never the shape of a `Val`:

- `Label(l)` unifies the target with `[l: α | ρ]`, under `RecordFieldRead`.
- `Position(n)` unifies it with `[α]`, under `ListPositionRead`.
- `Data(v)` unifies it with `Map α`, under `MapKeyRead`, and `v` with
  `String`, under `MapIndexKey`.

`infer_index_step` applies each rule immediately. There is no deferred read
constraint, shape default, special generalisation parameter or index-made weak
variable. `nth : ∀α ν. Integer → [α] → Command[ν] α` handles computed list positions, so
both map and list helpers retain their element polymorphism. The surface is
one to one with the formal core's primitive `index (fld ℓ)`, `index (pos k)`
and `index key`
([[decisions/261102_a-written-key-is-a-name|a written key is a name]]).

**Capture is decided by the type.** `CompTy::Return(Grade, Ty)` carries a grade
— `Value`, `Output` or a variable — in a fourth atomic union-find beside the
type, computation and row stores, quantified by `generalize` (`Scheme.grade_vars`)
and never weak. A *command* — an external, `^name`, a path head, a value row that
writes, or a handler arm standing in for one — is `Return(Output, Unit)`,
printed `Command`; `CompTy::command()` builds it and `CompTy::pure` is
`Return(Value, _)`. `Unifier::unify_producers` meets the grade/type pair for returners and handles.
`Output` immediately refuses an opposing type that cannot admit `Unit`.
For output-capable grades, a Unit-capable variable or differing Unit capability
requires payload validation before grades join, so failed equations leave no
hazardous aliases. Pairs containing known `Value` and other safe pairs keep
grade-first unification. The inferred invariant is `F^w A ⇒ A = Unit`:
all rules introduce output at unit and preserve the pairing
([[design/capture|grades and capture]]). `CompKind::Capture` checks its body
as a command and produces bytes at a fresh grade.

- `head_class` (`infer.rs`, beside `exec_comp_ty`) resolves a bare head by the
  order checker and runtime share: `Binding`, `Value(entry)`, `Arm`, `External`.
- `rhs_bound_ty`, shared by `Bind` and `Define`, reads the resolved RHS:
  `Fun` binds a thunk; `Return(Output, Unit)` captures and binds `String`;
  `Return(Value, a)` binds `a`; a free grade fixes to `Value`, and a
  computation variable opens to `Return(Value, fresh)`. The last case is
  the sole default. `infer_let` applies the returned `Bound` directly to the
  RHS node it inferred.
- `produces` pairs a fresh grade with a value type for `return`, primitives,
  `~`, capture and handled alias statements. `Decl::scheme` freshens ordinary
  builtin result grades; boundaries keep `p`, commands `w`, and already
  graded results keep their grade. `spawn` and `audit` also produce at fresh
  grades. A free unit producer can unify with `w` without a runtime term.
- `apply_args_capped` checks a literal block against its parameter through
  `demand_block`, then unifies with the parameter as expected. A block in
  hand meets the parameter directly; neither gets a capture wrapper.
- `redirected` forces an outer return shape under `Reason::RedirectShape`.
  Stdout gets a fresh result grade at the same payload type, leaving the
  input grade alone; stdin, stderr and `2>&1` preserve it.
- Each inference rule returns its node beside its type. `infer_let` builds
  `cap M to d. decode d` for a captured RHS and `return (thunk M)` for a
  function RHS (Let-fun). No later pass decides these rewrites.

**Returned-thunk application is explicit in the checked tree.**
`autoderef_thunk_return` records every argument position before which a
returned thunk must be forced. Positions may repeat when several thunks are
unwrapped before one argument. `apply_args_capped` returns the positions with
the inferred argument nodes; the `App` or binding-resolved `Exec` rule builds
`App(head, prefix) to h. App(Force h, suffix)`, repeated as needed. Redirects
wrap the whole split call; binding capture also wraps the whole. A
re-inferred checked tree records no further split
([[invariants/ir-pure-cbpv|the IR invariant]]).

**Bind and statement rules remain distinct.** `Seq` uses statement inference
without generalisation; `Bind` uses the let rule. Lambdas bind monomorphic
names; destructuring binds follow the whole lambda chain. Options are record
nodes whose entries are checked in written span order.

`let x = f` with `f = { hostname }`, `let x = !$t` and `let x = g 5` capture
when their resolved RHS is a command. `let x = map { |f| echo $f } $xs`
instead streams each callback's output and binds unit elements.

**Arms meet fresh results, then unify in source order.** `join_arms` checks
each arm against `Fun(params…, ρᵢ)`, with a fresh result and the parameter
types supplied by the form. It then unifies each `ρᵢ` with the join as the
expected side, in source order. `infer_case` supplies known payload types;
`infer_try` uses the same join, as does `?`. There is no coercion, target
verdict or reordering. A failure blames the later arm; its hint distinguishes
that arm from the earlier command or value. An `else`-less `if` wraps its
body as `{ body; () }`, whose fresh unit grade can join either producer kind.

**One shape rule, `force_return_shape`, does the work an adjacency rule used to.**
`extract_return` (`infer.rs`) resolves a `CompTy` to its return type, unifying
against a freshly minted `Return` (fresh grade and type) when it is still a variable. `infer_pipeline`
forces **every** stage through it under `Reason::PipelineStageShape`: a stage
typed `Fun` is a function still waiting for an argument, and the diagnostic says
to apply it rather than pipe into it. A stage feeds the next by writing, so
`stage_writes` unifies every stage but the last with `F^ε Unit` at its own grade
(any grade is accepted) and refuses a decoder by its own mark (`stage_decoder`,
`DecoderMidPipeline`, T0078). The pipeline's type is the last stage's; its value
is its final stage's, and the stage types are recorded for the structural REPL
along the way. The rule's one other premise reads no type at all:
`stage_root_stdin_feed` looks at a non-first stage's root redirects, and a `< f`
or `<< w` binding fd 0 there is `DeadPipeEdge` (T0070) — the feed answers the
stage's reads for its whole run, so the wire's producer writes for nobody
([[design/pipelines|pipelines]]).

**An arm stands in for the head it replaces.** `Inferencer::arm(block, arm)`
checks a handler or alias arm: it takes the parameters its slot hands it, read
off its type, and produces what `standsFor(name)` produces — the scheme of the
base frame or arm already in force under `name`, else `[String] → Command`. An
arm for `curl` therefore has a command's value, its output (a `()`-returning stub
is admitted when its grade is free and captures `""`), and one for `detach` returns
what `detach` returns; the installed scheme has the head's producer kind. A refusal is `Reason::StandsIn(Standing)`, with
`Standing::{Command, Own, EveryCommand}` for a command, a head's own scheme and
the catch-all handler. The run-time install doors apply the same rule, over
what the arm's closure captured (`Shell::closure_schemes`)
(`HandlerEntry::vet` through `alias_arm_scheme`, and the catch-all through
`catch_all_stands_in`) and render the same sentence (`shell::runtime::refused_arm`).

**The `()` note.** `TyEnv` records, for a name `let` bound to what a call
returned (`note_called`, `called_head`), the callee; a read of that name at type
`()` is noted in `InferCtx.unit_reads`, and an error raised at that span whose
kind concerns `()` gains a sentence that `deploy` returns `()` and that
`| from-line` captures what it prints (`TypeError.unit`, `UnitCall`); the pipe
takes everything a stage writes, so the remedy is operationally right. `()` is
neither a word nor an interpolant: its kind is `scalar`.

**Principality is the textbook result, with its limits stated.** It is principal
against a declarative system whose generalised kinds are *proper* — neither
empty nor a single nullary head — which `unite_tys` enforces by refusing an
empty meet and binding a one-head meet (`{ |x| $[$x + 1]; length $x }` is refused
at the meet, though it is bound and never used), and which holds of the schemes
in the environment and the builtins. Failures are data: a failure escaping a
generalised right-hand side carries a closed error type, so no generalised
variable escapes with it. Only the bind rule defaults a free grade. Arguments, handler producers and
join results unify without adaptation; joins retain source-order blame.

Reads impose ordinary equalities immediately; no read constraint
is drained at the unit's end. Shape verdicts — the
pipeline stage forcing, the sequence tail — are introduction-rule choices, not joins.

**The row algebra is textbook.** A slot is a label and a type, so the Rémy
rewrite is unitary and principality over rows is the standard Hindley–Milner
result. The two rules that build a row from a program — the closed literal and
the update — mint every payload fresh or as written, and nothing else writes
into a row.

No declared table reaches the unifier. A form's options are consulted key by
written key at the form, and a contract file's return is ascribed its table
after inference, in a scratch copy of the unifier, never entering the store
([[decisions/260930_a-table-never-enters-the-unifier|a-table-never-enters-the-unifier]]).
So two tables naming one label at two types cannot let the order of two
constraints decide a verdict.

**One obligation is carried rather than discharged, and it is not a theorem.**
*Recursion*: the **policy is chosen and the proof is owed**. The occurs check descends through payloads as well as along the spine.
The alternative — occurs along the spine only — would admit
`ρ = (x: Record(ρ) ; Empty)`, a cycle anchored at no type or
computation variable: `apply_row_inner` carries no row-root guard,
`cyclic_roots_in_ty` cannot find it, and `Scheme` has no `row_bindings` to
snapshot it into. So spine-only is not an edit to the occurs check but a third
recursive sort, and this tree takes the descending policy.

`MAX_UNIFY_DEPTH` is a **resource bound and not a termination result**. What
is guaranteed throughout is a graceful `TypeTooDeep` rather than a blown stack.

**Generalisation is at the binding boundary** (`generalize.rs`). At each `Bind`
the inferencer takes the type's free variables minus those still free in the
environment and closes over the difference into a `Scheme`, each quantifier
list sorted by variable id so a scheme — and the letters its `Display` prints it
under — is a function of the type alone; `instantiate` refreshes a scheme's
bound variables at each use. A pattern's binders generalise together: the
pattern's shape is solved first, then each binder's type is closed over the
environment, so acceptance does not depend on sibling order
(`let [a, [b]] = []` and `let [[b], a] = []` are both refused when `a` is later
used as a number); the Agda `to⟨ p ⟩` does the same. Generalisation walks the
type structurally and unbudgeted, where unification charges a depth ceiling:
the walks are linear in a type the source built one constructor per statement,
and only unification descends past what the parser saw
([[invariants/term-depth|term-depth]]). The order
follows the SCC
structure the elaborator found — a non-recursive group generalises at its binding
point, a mutually recursive group stays monomorphic until its fixed point — which
is what keeps generalisation sound. A type error aborts with a positioned
expected-vs-inferred message (`ty/fmt.rs`), where a computation reads
`Returns A`, `Command`, or `Command[ν] A`. A free grade is hidden as
`Returns A` when its paired type excludes unit; schemes quantify only visible
grades. `{ echo hi }` is `{Command}` and `{ 'hi' }` is `{Returns String}`.

**Deep types are checked without a guard, and checking them is superlinear.**
The shape that reaches the structural walks without ever charging `deeper()`
nests one constructor per binding (`let x1 = [f: x0]`, `let x2 = [f: x1]`, and
so on). Under `ral --check`, release build, 8 MB stack:

| nesting depth | wall clock | result |
| --- | --- | --- |
| 20,000 | ~1s | exit 0 |
| 50,000 | 54s | exit 0 |
| 150,000 | 11min | exit 0 |

- *No overflow and no `TypeTooDeep`* at 293x `MAX_UNIFY_DEPTH`: the unify
  budget never fires on this shape, and the walks survive alone.
- *Cost is superlinear*: 3x the depth is 12x the time, consistent with
  generalisation at each binding walking a type grown one constructor per
  binding, Θ(N²) visits (inferred from the curve, not profiled). A 50,000-deep
  type needs a 50,000-line file, so it is recorded, not urgent.

**Weak variables are the one subtraction.** `generalize` quantifies every
variable of the type that is neither free in the environment nor *weak*. A
variable of any sort — type, computation or row — is weak once
`Unifier::mark_weak` has said so, and the mark is permanent for the unit:
`Store::unite` hands it to the survivor, and a variable of any sort
unified with a weak one, even one already fixed to a structure, becomes weak, so
a later mismatch is still seen; binding a weak variable to a structure makes
every variable of the structure weak, with the same source — so the weak set is
the image of the weak seeds under the accumulated substitution. A scheme or binding keeps a weak variable as itself
(`apply_ty_keeping_weak`) until its unit ends. A weak
variable is treated as free in the environment, so a `let` over it is
monomorphic in it, and the scheme lists it in `Scheme.weak` instead of
quantifying it; its `Display` prints it `_α` after the binders.
There is no value restriction: a `let` of anything generalises
([[decisions/260930_a-let-generalises-what-is-not-weak|a-let-generalises-what-is-not-weak]]).
Only a checked boundary call marks its result
([[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]]);
the rule places `Pending::Open(ty)` on the constructed call. After inference
and ascription succeed, freeze snapshots that result as a `Site` on the call.
A site's graph preserves type and computation roots, and canonical
free row-tail and grade ids. Reinstantiation freshens each identity once across
the whole graph, so checking module exports in one scratch unifier keeps the
same sharing constraints as direct unification.
`settle_weak` closes a scheme at its unit's end: simultaneous substitution
updates live weak-variable images and resolves residual grades throughout the
body and all μ-equations, even with no weak variables. Local ∀/μ names stay
opaque. Remaining free grades are quantified and cached grade residuals cleared.
Cycles reached through the live weak images are snapshotted (`cyclic_bindings`),
so `μα. Map α` survives the unit without resolving a local binder as a live slot.

**A type variable carries a kind** (`kind.rs`): a set of admissible heads and a
deep bit, and a row variable a deep bit alone. The unconstrained `any` has every
head; five are named — `number` (`+ - * /`, unary `-`), `comparable` (`<`, `lt`,
`gt`, `sort-list`, `int`, `float`), `scalar` (each interpolation part), `sized`
(`length`, `is-empty`) and `data`, every head but a block and a handle, *deep*
(`==`, `!=`, `str`, the encoders). The unifier holds them beside the store, on
the free variable's root: `Kinded { kind, witness }`, the witness being the span
in force (`Unifier::at`, set by `InferCtx` before it unifies or instantiates)
when the kind last narrowed. `unite_tys` unites at the meet and refuses an empty
one, citing both witnesses; a meet with one nullary head binds the variable to
it. `admit` runs at `bind_ty` and checks the head, and a deep kind walks the
structure (`data_ty`, `data_row`, with a visited set for a data cycle) and
imposes `data` on each free variable and deep bit it reaches; a row variable that
is deep imposes the same on whatever it is later bound to (`admit_row`). A
refusal is `KindMismatch` (T0074), which `explain.rs` words by the kind and
guides by the reason, and which `diagnostic.rs` draws with a second caret at the
witness. A kinded variable still free at the end of a unit needs no default.
Operators take a fresh kinded variable and unify both operands with it
(`infer_binary`), and so does each interpolation part; `%` is `Int → Int → Int`.
A scheme quantifies `(TyVar, Kind)` and `(RowVar, bool)` and prints `∀α:number.`;
instantiation re-mints each, and a weak residual keeps its kind
([[decisions/260930_operators-are-kinded|operators-are-kinded]]).

Kinds do not select read rules. The written key already determines the
required head; a conflicting kind is refused by ordinary unification.

**The quantifier is the prefix, not a binder.** A `Scheme` is the body's
ordinary `Ty` under a ∀-prefix of four `Vec`s of variable ids — value
(`ty_vars`), computation (`comp_ty_vars`), row (`row_vars`) and grade
(`grade_vars`) sorts, each a
`u32`-tagged unifier root (`scheme.rs`). There is
no binder node and no de Bruijn index: a variable is
**bound iff it is listed**.

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
- *The residual cache stores seeds, not stable roots.* `CachedFreeVars` holds
  ids a scheme did **not** quantify. `env_free_vars` resolves each through the
  current unifier, following unions and structural bindings across all four
  sorts rather than assuming an id is still an unbound canonical root.
- *Closure is syntactic over the whole graph.* Every variable in the body and
  every μ-equation must be quantified, a μ-root or a listed weak residual;
  resolving against a live unifier could hide an escaping id as ground.

**The verdict survives into the next run.** Inference builds each
`Phrase::Define` with its harvested per-name schemes, still unsettled.
Successful freeze applies `settle_weak` and asserts closure over the whole
scheme graph before it leaves the unit. A pipeline carries no per-stage type
annotations. The next run's check is seeded from the live session — one `SessionSchemes` (the scope's
name→scheme map plus the alias arms' schemes) — so a name bound in run *N* is
checked at its inferred scheme in run *N+1*; a host binding without a stored
scheme is `None` and infers afresh
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

A residual *weak* variable is the exception to that closure, and it is kept
across units rather than quantified: the scheme records it (`Scheme.weak`), a
unit settles its `Define` schemes before they are stored (`settle_weak`, so a
residual the unit fixed is stored as what it was fixed to), and `seed_env`
re-seeds each residual of a stored scheme as a fresh weak variable of the next
unit's unifier (`reseed_weak`). Reseeding simultaneously alpha-renames every
local ∀/μ binder, including row and grade binders, to names disjoint from the
fresh weak residuals; otherwise a new residual could be captured by an old
binder with the same numeric id. The name is then used at one type in the next
unit, as it was in the defining one, and no foreign id is ever read.

**Sites freeze once the unit succeeds.** IR site parameters default to
`Arc<Site>`; inference builds `Comp<Pending>` and `Val<Pending>` instead.
`Pending::Open(ty)` becomes a snapshot under the final unifier.
`Pending::Kept(site)` preserves an existing checked site's `Arc`, including
its unit identity. Only `Val::Protected` uses `Pending::Imported(ty)`:
freeze snapshots the occurrence's type and drops that membrane unless
`Site::has_delayed` reaches a thunk or handle. Boundary values are built as
blocks holding saturated calls, and under-applied boundary calls are
η-expanded during inference, not freeze.

An imported residual used as an `Exec` head instead becomes
`Force(Protected { Variable(name), site: Open(ty) })`; its protection stays.
These execution-head and value-occurrence rules are distinct. Snapshot graphs
preserve canonical row/grade identities and whether remaining variables are
weak or quantified; freezing does not make all types ground.

A membrane admits nothing. Freeze constructs `Toplevel.admits` from
`InferCtx.residual_uses`, snapshotting each imported name's first-use type
in first-use order. These readmissions share one fresh `UnitId` with all new
sites and run before the first phrase. Weak variables are shared across the
unit, so the first-use type covers every occurrence. `InferCtx.unit_reads`
instead supplies diagnostics for names bound to unit-valued calls.

Inference memoises member types and the pending `Arc<GroupNode>` by source
group identity; freeze memoises final groups by pending `Arc` identity. Every
projection therefore shares the group in both phases. An erroneous unit's
built IR is discarded whole without freezing, and one-shot inference sessions
discard the IR they build.

Recursive wrapping carries delayed obligations through containers, thunk
arguments/results (`CompKind::Protect`), and handle views at
`await`, ready `poll`, and `race`, without forcing or consuming them at import.
Only weak `comparable` identities acquire session fixings; a quantified
parameter remains fresh on every call. Generated wrappers capture the raw value
with a closed scheme rebuilt from the contract in a scratch unifier.
Re-inference also checks protection nodes against their contracts,
preserving weak results and the raw computation's grade when a host rebinds a
returned wrapper.

See also [[design/types|types]]; map [[map/core/typecheck|typecheck]]. Judgments:
`docs/SPEC.md` §17.3.
