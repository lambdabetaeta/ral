---
verified_at_commit: 22b85799e5e435dff38ea1a2e7ec56d443d9f62b
verified_at_date: 2026-11-11
anchors: [Generator, Solver, Settled, Solution, Readback, Inferring, BindHole, CallHole, Mode, propagate, close_region, generalize, instantiate, settle_weak, reseed_weak, unify_producers, SessionSchemes, Closed]
---

# Type inference: constraints, closure, readback

**The checker generates without reading a solution, solves by rank, then reads
back one checked tree.** Hindley–Milner inference runs over the CBPV
[[map/core/ir|IR]], after surface [[map/core/elaboration|elaboration]]. The
architecture is fixed by
[[decisions/261110_elaboration-is-decided-by-suspended-matches|suspended matches]];
[[design/types|types]] states the type language, and
[[map/core/typecheck|typecheck]] locates its implementation.

## Generation has no observations

The generator writes equations and builds a *pending tree*, not an elaborated
verdict from a partially solved type.

- The solver owns the private unifier. Instantiation and contract import
  return unknowns; a scheme is an opaque token to generation. Syntax and
  builtin declarations may supply known structure.
- `Solver::binding(rhs, forced)` returns a fresh binder and a `BindHole`;
  `Solver::call()` returns a `CallHole`, and `argument(call, head, callee,
  arg, …)` the result of that call's next argument. Each suspends a match that
  carries its hole and, for diagnostics, the variable it scrutinises when the
  program names one.
- An equation answers only whether it held, to suppress a secondary complaint
  after refusal. This is not an observation that selects a successful typing
  or elaboration rule.
- A lambda checked against an expectation first writes an arrow equation,
  then checks its body with the fresh parameter and result. Literal block
  arguments are generated after the other arguments; argument matches always
  form a left-to-right chain.
- Joins check each arm against a fresh result and equate the results. Arguments
  and handler producers also unify directly: no implicit capture there.
- Key form selects the projection equation immediately: a written label demands
  a record slot, a literal position a list, a computed string a map. Reads
  introduce neither suspended matches nor weakness
  ([[decisions/261102_a-written-key-is-a-name|a written key is a name]]).
- A bare head is classified from Σ and lexical bindings alone: binding,
  value builtin, base frame, external. Base frames keep their own schemes and
  in-shell gate; every other command name takes the external gate, handled or
  not. Handled-name sets serve diagnostics only. A binding is required to be
  a thunk by equation, with named-head provenance for T0072; an unbound `$name`
  is T0071, never a fresh unknown.

## Two matches choose elaboration

**Only a known scrutinee head selects a branch, and the solver records the
choice on the match's hole.** Readback reads those decisions; it never matches
a head again.

- The binding match `bound τ C`: an arrow binds `U C` by running (`Mode::Run`);
  `F^p A` binds `A` by running (`Mode::Run`); `F^w Unit` captures output and
  binds `String` (`Mode::Capture`). A free computation or grade suspends. A
  payload whose head or kind excludes `Unit` entails grade `p`.
- `bound` never reads the binder type `τ` to select its branch. A use demanding
  `String` cannot decide whether the RHS should be captured.
- The argument match `apply C A D`: an arrow equates its parameter with `A`
  and result with `D`. A producer requires its payload to be a thunk, records
  a split before this argument, and continues matching that thunk's
  computation.
- Producer-only thunk cycles are refused by the data-guarded cycle check, so
  returned-thunk peeling cannot traverse such a cycle forever.
- Global producer improvement entails `p` for every registered returner or
  handle pair whose payload head or kind excludes `Unit`, not only those at
  binding matches. A unit payload alone does not entail either grade.
- A suspended match waits under the root of its scrutinee. Every public
  constraint is one operation followed by one `propagate`, which drains the
  roots the store touched: it improves the producer pairs on them and
  re-steps their waiters, until nothing is touched. A match that still waits
  is filed under its root's current representative. At rest every suspended
  scrutinee is an unknown; everything its match mentions has rank no greater
  than that unknown's.

Holes are phase annotations of the IR. The pending tree is
`Inferring`-phase: `CompKind::Bind` and `Phrase::Define` carry a `BindHole`,
and `CompKind::App` and `CompKind::BindingCall` (a bare head bound in scope)
carry a `CallHole`. In `Checked`, the evaluator's phase, those annotations are
`()` and `BindingCall`'s is uninhabited, so no hole reaches the evaluator.

## Ranks determine the closing boundary

**Rank is region depth, with the unit at zero.** Every union-find root has a
rank, bound or free.

- Fresh unknowns take the rank in force. Unification retains the lower rank;
  binding lowers a structure's reachable unknowns. A suspended match lowers
  everything it mentions to its scrutinee's rank.
- A `let` region includes both its RHS and pattern equations. On exit the
  solver defaults the still-young matches, propagates, then returns to the
  outer rank. Generalisation quantifies reachable unknowns above that outer rank,
  one ordered map per sort; all pattern binders use the solved pattern.
- A match on an older scrutinee survives an inner exit. This prevents an inner
  binding from committing an enclosing parameter's elaboration.
- An unresolved producer grade lowers its payload dependency to its rank,
  not conversely. A fresh grade beside a weak, Unit-capable payload can still
  generalise. Structural lowering from a weak root still reaches every grade
  inside it, including those in returned thunks. Young pairs retire before
  generalisation; instantiation registers fresh pairs, so improvement never
  mutates quantified definitions.
- The defaults are equations chosen together: an unresolved binding becomes
  `F^p α` and binds `α`; a free binding grade becomes `p`; both record
  `Mode::Run`. An unresolved argument becomes an arrow and records no further
  split. The default's reason carries `defaulted_at`, the closing span.
- A binding and an argument match waiting on one root have incompatible
  defaults: T0080 (`ConflictingDefaults { binding, call, forced, callee }`)
  cites both uses, and its hint names the variable the binding forces or the
  call applies, when there is one. Each chosen default
  replaces its match with the complete branch equations, so a failed default
  is not replayed. A refused region retires unresolved young matches; this
  recovery exports no typing result.
- Successful exit leaves no young match. Instantiation copies quantified
  unknowns, so later uses cannot choose a definition's elaboration. No partial
  schemes or retained region tree are needed.

A recursive group is checked once, in a region of its own, at its first
projection: members are bound monomorphically, and each member's type is
generalised on exit. The generator caches the member schemes with the pending
group by source identity; every projection, the first included, instantiates
its member's scheme. No group variable reaches a sibling region, so regions
form a stack. Readback memoises rebuilt groups by pending identity.

## Weakness is rank zero

**A boundary result has one type per unit because no `let` can quantify rank
zero.** Lowering the result to zero also lowers everything that meets it;
`WeakSource` carries diagnostic provenance, not a separate generalisation rule.
This is the rank formulation of
[[decisions/260930_a-let-generalises-what-is-not-weak|a let generalises what is not weak]].

- A scheme's `Role::Weak` entries are its unquantified value, computation and
  row residuals, with their kinds/deep bits. Grades also carry ranks but have
  no weak role: a scheme's grades are a set, all quantified.
- `settle_weak` substitutes residual images and resolves grades throughout the
  body and all μ-equations, preserving local ∀/μ names. It quantifies remaining
  free grades and returns a `Closed`; closure is checked syntactically over the
  whole graph.
- A new unit re-seeds stored weak residuals at zero and simultaneously
  alpha-renames local binders into disjoint names. No foreign live id is read
  ([[invariants/schemes-leave-closed|schemes leave closed]]).
- There is no value restriction and no environment free-variable scan.

## Unit closure precedes every export

A unit is `typecheck` or one of the one-shot sessions `arm_stands_in`,
`binding_value_scheme`. A REPL phrase starts with its
predecessors' closed schemes.

- Seeding occurs at rank zero from `SessionSchemes`: binding schemes, handled
  names for diagnostics and the host's builtin `Manifest`. No handler scheme
  is persisted or seeded. A builtin declaration caches one closed
  scheme and its curry spine; it never receives the live unifier.
- A one-shot enters a region before checking its body and closes the unit
  before returning. A lexical binding's scheme is generalised after region
  exit and settled only after successful closure. An install-time arm check
  exports only a verdict, never a scheme.
  `Site::scheme` likewise enters its scratch region before instantiating the site.
- A top-level `Run` introduces no region. `Solver::settle` closes rank zero
  and yields `Settled`: every elaboration match has retired, including on
  refusal. Every mode and split is fixed.
- Unless settling reported errors, the return contract is held live on
  `Settled`, using the manifest for stand-ins. It can equate and resolve types,
  instantiate schemes, mint fresh types, queue nested tables and diagnose;
  it cannot open a hole or enter a region. A contract is a use of the returned
  value, never a choice of elaboration.
- `Settled::close` then judges final-head argv gates and queued nested tables,
  attaches `()` notes and returns a `Solution` only without errors; otherwise
  the errors, sorted stably by source span.
- A `Solution` is read-only: decisions (`mode`, `splits`), resolution, sites,
  settlement and scratch judgements. Readback freezes its sites, schemes and
  readmissions only after the live holds and close succeed.
- Gates refuse known `Unit` at base-frame argv boundaries and the declared
  refused shapes at external heads, including handled ones. An external gate
  retains whether its head was handled where written, so its diagnostic can
  explain why a handler does not widen admission. A still-free head remains
  a runtime question; spread elements are not statically gated. The runtime distinction
  for arbitrary handlers is documented in `docs/SPEC.md` §6.6, not closed by
  this inference architecture.
- A unit with errors is discarded before readback or scheme export.

## A return contract holds live after settling

**Generate → settle → hold → close → freeze is one inference, not a loop.**
A table is consulted key by key, never unified as a record type.

- `Held::Unify` uses `equate_held`, as written options do. Factory shape and
  result equations also propagate through the live solver. Their solutions
  reach decoder sites, schemes, readmissions and final argv gates.
- The rc's and manifest's `handlers:` use `Holds::Arms`/`Held::Arms`.
  `stands_in` is the sole arm-to-head relation for these fields, scoped arms
  and the catch-all. Record labels select Σ's stand-in; computed-name maps
  hold their value type to `[String] → Command`. Install-time checking still
  judges each actual name. Imported arms retain their module's sites.
- The walk re-reads each row's tail after every field. After all live holds,
  unknown, missing and refused keys are judged on the final types. Each
  walked row is re-read to its final tail; fields the walk did not hold are
  judged on scratch, without repeating a live clash or its weak-variable note.
- Contract equations introduce ground types apart from fresh variables used
  once. Base-frame result payloads must be ground (their grades may quantify).
  A field revealed in a row already left therefore has no variable needing
  solving: the final scratch judgement suffices, without repeated inference.
- Nested tables are queued, then checked on scratch at close against the
  final live types. They do not feed equations back into inference.
- A wholly variable tail remains a runtime question: decoded data contains
  no thunk. Unresolved field and member types likewise retain runtime checks.
  Written tails use field spans; computed tails use the tail phrase's span.

## Readback fixes terms and sites

**A successful closed solution supplies every elaboration choice.** Readback
replaces each `BindHole` by its `Mode` — run or capture/decode — and
each `CallHole` by explicit bind/force segments before its split indices
(ascending, repeats allowed). A `BindingCall` becomes an `Exec`, its
redirects fused when unsplit and wrapping the whole call when split. Capture
wraps the whole RHS. See
[[design/capture|capture]] and
[[invariants/ir-pure-cbpv|the checked IR invariant]].

- A boundary's solved type (`Exec.site`, a `Ty` while inferring) is
  snapshotted under the unit's `UnitId`. On a protected value,
  `Membrane::Kept(site)` preserves a checked tree's site and unit;
  `Membrane::Imported(ty)` keeps the membrane only when the contract has
  delayed parts. `CompKind::Protect.site` is `Arc<Site>` in every phase: only
  runtime membranes build it, and the checker re-checks it as kept.
- Sites preserve sharing, cycles, kinds and canonical free identities in all
  four sorts, not necessarily ground types. A membrane admits nothing by
  itself; `Toplevel.admits` records imported residual names' first-use types
  for readmission before execution.
- Readback settles each definition's schemes into `Closed`. The prelude bake,
  live scope and sealed seed transport those same schemes
  ([[decisions/260603_session-scheme-continuity|session scheme continuity]]).
- Re-checking a checked tree accepts it and inserts no further elaboration
  (`readback.rs`: `elaborated_capture_and_repeated_splits_reinfer_unchanged`).
  Raw unifier ids in schemes and sites are not canonical, so a re-checked tree
  need not be `==` to the original.

## Unification and the scope of the claims

The underlying equations retain the type system's existing constraints.

- Value/computation cycles must cross data; `Pairs` guards repeated recursive
  equalities, including one-sided anchors
  ([[decisions/260606_unify-one-sided-obligations|one-sided obligations]]).
- Rows use Rémy rewrite over finite spines. The occurs check follows only tail
  edges; productive recursion through field types is admitted
  ([[design/row-types|row types]]). Every stored field payload has a type root,
  reusing an existing variable or aliasing a structured type, so every new cycle
  has a type/computation anchor understood by schemes and sites. No row
  μ-binder is needed.
- Row binding checks depth and deep-data obligations before introducing
  aliases, then lowers the stored row to its owner's rank with weak provenance.
  Labels, duplicate order and the terminal tail retain their identity. Rémy's
  shared-tail label-multiset safeguard still refuses impossible finite spines;
  a label against an empty row is an error, not implicit row extension.
- Kinds meet at union, refuse an empty meet and fix a single nullary head.
  Deep kinds constrain reachable types/rows. Instantiation preserves declared
  kinds and freshens all quantified sorts and cycle roots.
- `unify_producers` preserves `F^w A ⇒ A = Unit`, validating unsafe payload
  combinations before grades can alias. Grades classify producers, not effects.
- The unification depth ceiling is a resource bound, not a termination proof.

The architecture is intended to make accepted trees independent of constraint
visit order. Defaults are language choices, not a claim of principal
elaboration; diagnostic contents may depend on traversal.

- Recursive-row regressions vary equation order, orientation, allocation order
  and rank for the productive-record witness; each keeps its regular solution
  and residual sharing.
- Producer regressions preserve independently quantified instance grades over
  shared weak payloads, while structural lowering still reaches grades inside
  weak returned thunks. Failed payload equations do not fix another producer's
  grade. Diagnostics retain solved nested types and recursive-call provenance.
- These tests do not prove complete unifier confluence. The Agda elaborator
  does not mechanise this schedule.

See [[related/omnidirectional-type-inference|the suspended-match comparison]]
and `docs/SPEC.md` §§17.3–17.7.
