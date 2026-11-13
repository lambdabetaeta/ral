---
verified_at_commit: f9ac94f8
verified_at_date: 2026-11-12
anchors: [Generator, Solver, Solution, Readback, Inferring, BindHole, CallHole, Mode, propagate, close_region, file_options, check_options, check_option, generalize, instantiate, settle_weak, reseed_weak, unify_producers, SessionSchemes, Closed]
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

- The solver owns the private unifier. Instantiation returns unknowns;
  generation does not inspect their solved structure.
  Stored schemes are inspected only for cross-unit admissibility. Syntax and
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
  value builtin, base frame, external. A base-frame call has result `Command`
  without consulting the row's scheme and uses the in-shell gate; every other
  command name takes the external gate, handled or not. Handled-name sets
  serve diagnostics only. A binding is required to be a thunk by equation, with named-head provenance for T0072; an unbound `$name`
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
carry a `CallHole`. In `Explicit`, the phase the evaluator runs, those annotations are
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
- **A stored name is unusable in a later unit if a weak residual is reachable
  beneath a thunk or handle in its scheme.** This covers arguments as well as
  results, and type, computation and row residuals. Existing first-order data
  is instead readmitted against the later unit's solved type before it runs.
  A decided function beside such data is allowed; a quantified parameter is
  not an undecided shape.
- This reachability follows μ-equations from the scheme's body, preserving
  data or delayed context. A recursive root shared between the two contexts
  must be judged in both; an unreachable equation is irrelevant to this rule.
- There is no value restriction and no environment free-variable scan.

## Unit closure precedes every export

A unit is `typecheck` or one of the one-shot sessions `arm_stands_in`,
`binding_value_scheme`. A REPL phrase starts with its
predecessors' closed schemes.

- Seeding occurs at rank zero from `SessionSchemes`: binding schemes, handled
  names for diagnostics and the host's builtin `Manifest`. No handler scheme
  is persisted or seeded. A builtin declaration caches one closed
  scheme and its curry spine; it never receives the live unifier. An argv row
  has no type rule: its scheme is always `U([String] → Command)`.
- Every session binding has a `Closed` scheme: a checked top-level `let`
  stores it with the value, and a host's `set_var` requires it explicitly.
  Captured block locals may lack schemes; closure re-inference at hook or
  handler installation gives each a fresh rank-zero monomorphic type, shared
  by all its uses in that unit, with provenance `WeakSource::Host`.
- A one-shot enters a region before checking its body and closes the unit
  before returning. A lexical binding's scheme is generalised after region
  exit and settled only after successful closure. An install-time arm check
  exports only a verdict, never a scheme.
- A top-level `Run` introduces no region. After generation, `Solver::close`
  closes rank zero and retires every elaboration match, including on refusal.
  It judges final-head argv gates, attaches `()` notes and returns a `Solution`
  only without errors; otherwise the errors, sorted stably by source span.
- A `Solution` is read-only: decisions (`mode`, `splits`), resolution, sites
  and scheme settlement. Readback freezes sites, schemes and readmissions
  only after closure succeeds. Only tests may copy the type store.
- Gates refuse known `Unit` at base-frame argv boundaries and the declared
  refused shapes at external heads, including handled ones. An external gate
  retains whether its head was handled where written, so its diagnostic can
  explain why a handler does not widen admission. A still-free head remains
  a runtime question; spread elements are not statically gated. The runtime distinction
  for arbitrary handlers is documented in `docs/SPEC.md` §6.6, not closed by
  this inference architecture.
- A unit with errors is discarded before readback or scheme export.

## A host file's options are uses during generation

**Generate → close → read back is one inference.** A declared table is
consulted key by written key, never unified as a record type (SPEC §15.3).

- `file_options` follows the elaborated tail through binds and sequences,
  preserving their environment. A written record enters `check_options`;
  a variable, spread or computed result is T0077. Field values remain arbitrary
  expressions. A top-level trailing `let` is T0077; an empty file reads as `()`.
- `check_options` checks each key with `check_option` during generation,
  diagnosing unknown, refused and duplicate keys and all missing required
  keys (one T0081 per table, without calling optional keys required).
  `Holds::At` uses `equate_held`: its equations may constrain
  binding modes and call splits and reach decoder sites before they freeze.
- `Holds::Table` recurses immediately on a record literal. Any other value,
  including `()`, is T0079. These equations constrain the nested field values
  in the same solver as their parent.
- All twelve rc keys are required, and its `limits:` and `theme:` tables
  are total too. `` `inherit `` and `` `set v `` make retention or override
  explicit. Missing nested settings have pasteable `` `inherit `` hints,
  just as missing top-level keys have declared empty values.
  A manifest requires `name`; a grant profile requires no key,
  so only the latter accepts empty options.
- A manifest factory is a written one-parameter block whose body's tail is
  checked by the same rule with factories disabled; refusals name the factory
  body rather than the file's tail. Its parameter is fresh
  and monomorphic; the host passes an options map at runtime, without
  checking its contents against that parameter.
- `Holds::Handlers` checks a written map arm by arm through `install_arm`,
  each in its own region with duplicates refused, in rc, manifest and `within`
  alike. Any other value meets `Map(U([String] → Command))` as a whole,
  with `Reason::Handlers`; a record is refused with the map spelling.
  Collection keys are data, not nested-table syntax.
- Only a written `within` handler map contributes names to the body's
  diagnostics. The arm rule equates the slot's parameter shape, then its
  fresh result with `Command`, independently of the name and Σ.
  `Holds::CatchAll` uses the catch-all arm rule without a second equation.
  Scoped handlers need no runtime vet; persistent handlers retain installation
  checks. Imported maps meet the same arm type through their closed module
  schemes.
- Syntax guarantees rc and manifest result shape and keyset. Their decoders
  retain value-shape and domain checks, plus installation vetting. Grant's
  decoder also accepts grants received as data, so its runtime shape and
  keyset checks remain.
- Rc `prompt:` and `startup:` and manifest `hooks:` and `keybindings:` are
  decoded options, not static hook-type obligations. Installation still
  infers closures and checks hook arity; even the host's default prompt is
  a thunk, not a declared native. Handler installation retains its inference
  check too, including for arbitrary values supplied by a host.

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
  snapshotted under the unit's `UnitId`. Every registered boundary result
  constrains type variables to `data` and row variables to be deep, excluding
  functions and handles at every depth.
- Sites preserve sharing, cycles, kinds and canonical free identities in all
  four sorts, not necessarily ground types. `Toplevel.admits` records imported
  residual names' first-use types for readmission before execution. Such a
  site may contain a decided function beside undecided data; checking that
  function's head suffices because its own unit checked its body. Residual
  occurrences remain plain variables, with no delayed runtime contract.
- Readback settles each definition's schemes into `Closed`. The prelude bake,
  live scope and marshalled environment rows carry those same schemes.
  Structural child decoding uses raw `Arc<Scheme>` leaves. The reader checks
  environment schemes with `Closed::new`; IR admission checks every definition
  scheme in every recursively linked module before closure construction.
  Both refuse an open scheme at its complete JSON path. The IR representation axis does
  not change checking phases: linked modules remain `Explicit` in every
  outer phase ([[map/core/transport|transport]]).
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
- These tests do not prove complete unifier confluence.

See [[related/omnidirectional-type-inference|the suspended-match comparison]]
and `docs/SPEC.md` §§17.3–17.7.
