---
status: active
generated_at_commit: 9290eb03
generated_at_date: 2026-11-13
verified_at_commit: 9290eb03
verified_at_date: 2026-11-13
anchors: [Val, CompKind::Builtin, Explicit, ValClos, VClosRef, VClosRef::sub, VClosRef::view, VClosRef::into_owned, VClosRef::peel_variants, VClosRef::residual, VClosRef::block, ValClos::close, ValClos::thunk, ValClos::list, ValClos::map, ValClos::variant, ListRef::cons, ListRef::snoc, ListRef::concat, ListRef::suffix, Env::place, Env::EMPTY, Node, IrData, admit_sorted_fields, BuiltinEntry::eta, BuiltinEntry::residual, Stack::settle, CompClosure, Handle::Withheld, Marshalled, outcome_value, ErrorRecord::claim]
---

# Runtime values are value closures

**A runtime value is a pair ⟨V, ρ⟩ of `ir::Val` syntax and the environment
closing it, and nothing else.** `ValClos(Val, Env)` is that pair; its root V is
never a variable, and ρ binds every *direct name* of V: an occurrence outside a
thunk. A computation closure ⟨M, ρ⟩ belongs to the machine, not to the value
grammar. `Val` has one form per kind of value; the runtime adds none.

## Decision

- **Constants are syntax.** Bytes, builtins and handles are `Val` forms the
  elaborator never writes. Scalars and runtime constants are ⟨V, ∅⟩; a thunk
  is ⟨thunk M, ρ⟩; a list, record or map literal is ⟨[V…], ρ⟩. Hosts build a
  builtin value with `ValClos::builtin`.
- **Restriction happens at formation.** `ValClos::close` restricts ρ to the
  occurrences of V and pulls each missing direct name's value from Σ into the
  capture. `ValClos::thunk` restricts to the body's occurrences; names in the
  body resolve ρ then Σ when it runs
  ([[260926_a-closure-keeps-only-what-it-mentions|a closure keeps only what it mentions]]).
- **A borrowed value is a pair of borrows.** `VClosRef(&Val, &Env)` has the
  same invariant. `VClosRef::sub(V′, ρ)` is the one way to read a component: a
  variable becomes its binding in one lookup (a bound value's root is never a
  variable), anything else is ⟨V′, ρ⟩ sharing ρ. No read restricts or consults
  Σ. Every observation (`view`, `shape`, `found`, `residual`, the decoders,
  equality, order, `Display`, the first-order walk) is defined once on
  `VClosRef`; `ValClos` forwards. `VClosRef::into_owned` keeps a value past its
  borrow: ⟨V′, ρ⟩ sharing ρ, except that a V′ naming nothing gets ∅.
- **`view()` reads the head constructor only.** `View` has one case per head;
  a `Variant`'s payload is a `VClosRef`, a bare tag's is `()`, and a list or
  map is a one-case `ListRef`/`MapRef` whose `get` and `iter` resolve elements
  through `sub`. Nothing below the head is formed or inspected until asked.
- **A tag spine is read iteratively** (`VClosRef::peel_variants`). A variant
  `` `l v `` shares v's ρ; a builtin, handle or η-residual payload is bound to
  a fresh name, so those occur only at a closure's root, never beneath a tag.
- **Builtins force to η-expansions.** An arity-n entry forces to
  ⟨λa₀…λaₙ₋₁. δ(b; a⃗), {b ↦ c}⟩. β binds each argument; δ reads the entry and
  arguments from ρ. Partial application is an ordinary residual λ, boxed by a
  value-receiving frame. A call with exactly the entry's arity runs it
  directly.
- **Returns are fused.** The machine state is a `CompClosure`: ⟨M, ρ⟩ or
  `Err(e)`. `Stack::settle` hands a produced value down the continuation at
  once, so there is no value register and no `return` node at runtime.

## Built data is ordinary syntax

**A list or map built at run time is a value like any other: syntax over
variables bound in ρ.** A list is ⟨[x₀…xₙ], {xᵢ ↦ vᵢ}⟩, a map
⟨[:, k: xₖ…], {xₖ ↦ vₖ}⟩. There is no separate built representation.

- **Placement.** Putting v = ⟨W, σ⟩ into a node whose environment is ρ
  (`Env::place`): if W is an atom (unit, bool, int, float, string, bytes) it is
  inlined; anything else is bound in ρ under a fresh `%itemK` and the element
  is that variable. *Why:* syntax that names something never moves under an
  environment that could capture it, and a builtin, handle or η residual is
  never inline below a root. Inlining every name-free value would let
  `acc = [$acc]` in a loop build one deeply nested `Val`; with atoms only,
  runtime nesting always goes through ρ, so marshalling (an explicit-stack
  walk over environments) and drop stay iterative.
- **Fresh names are local.** A node built from scratch binds `%item0…%itemN`;
  adding to an existing ρ binds the first `%itemK`, K ≥ |ρ|, that ρ does not
  bind. There is no global counter, and the elaborator never writes the tag.
- **Operations** (`ListRef`, called by the evaluator and builtins; none
  reimplements them):
  - *cons, snoc* place one element in the list's own ρ: a persistent push and
    at most one persistent `Env` insert, O(log n).
  - *concat* appends the nodes when both sides share ρ (pointer-equal);
    otherwise the larger side keeps its ρ and each element of the smaller is
    placed in it, O(smaller side). Syntax is never renamed.
  - *tail* (`suffix`, the `[x, ...rest]` pattern) splits the node and keeps ρ,
    O(log n). **`rest` keeps the bindings of the elements it dropped until it
    dies.** This is the one accepted exception to
    [[260926_a-closure-keeps-only-what-it-mentions|a closure keeps only what it mentions]]:
    restricting at every step would make a loop over a list O(n²).
  - *maps* are built fresh (`ValClos::map`); the last pair wins.
- **Nodes are persistent, occurrences lazy.** A list node holds
  `imbl::Vector<Spanned<Val>>`; a record or map node holds
  `imbl::OrdMap<Name, Spanned<Val>>`, sorted and distinct by construction. A
  `Node`'s occurrence set is computed on first use (`OnceLock`): only source
  literals are restricted at formation, so a built node never pays for one.
- **Fields are an `IrData` associated type**, like `Bytes` and `Scheme`: the
  persistent map at run time, a pair sequence on the wire. The structural
  decoder stays free of semantic checks; `marshal/read/ir.rs`
  (`admit_sorted_fields`) refuses a repeated or unordered key with a typed
  `DuplicateField`/`UnorderedField` at its path, for records, maps, `within`
  and `grant` alike. The elaborator refuses a key written twice in a literal
  (T0022), since a node holds one value per key.

## Errors are not values

**An error is the 0-ary operation `error_r()`; it is never held.** In the
machine it is `Err(e)`. Its handler (`try`, `poll`, `audit`, a `guard` release)
turns it into the record `{frame, message, status, site}` (`ErrorFact::record`)
and the error no longer exists. A caught failure is that record as plain data:
`ValClos::from_datum`, closed syntax with ρ = ∅. No value form carries an
`ErrorFact`.

`raise r` therefore always decodes the record and raises it as the program's
own claim (`ErrorRecord::claim`): frame, message, status and site are kept; the
failure's internal typed reason is not. The record programs see is unchanged.
A worker's failure crosses as an `ErrorFact`, which `await` re-raises live; it
is never routed through a record value.

## Why η, not argument counting

**A residual function must cross a provenance frame as a λ.** In
`let m = { |f| map $f }; m g $xs`, β binds `f`, then the body's spanned
application puts `CallSite` above `Arg xs`. Counting adjacent `Arg` frames
lets `force map` see one argument; returning a thunk there makes it meet
`Arg xs` as a value: "not a function". η leaves a λ, which crosses `CallSite`
like every λ.

δ's entry comes from ρ, not Σ, so a host hook or override keeps its own
builtin identity. `VClosRef::block` hands the checker only a non-residual
thunk, which keeps δ out of inference. A residual has type name `Lambda`,
displays its builtin and supplied-argument count, and crosses as builtin
name, environment row and count; the receiver rebuilds its own η suffix.

## Why withheld, not omitted

**Omitting a handle binding is not unbinding it.** Lookup falls through to Σ:
dropping `lines ↦ h` exposes Σ's `lines`. Removing a list element renumbers
positions; removing a record field changes the shape. `Handle::Withheld`
keeps the binding, position, title and type `Handle`; observing it fails
`withheld-handle`, directing the caller to await the worker in its spawning
shell and pass the result.

Every child reads `Marshalled` bytes, which write each handle as its title
and read it back withheld, so **no live handle is reachable in a child's
inherited image** ([[map/core/transport|transport]],
[[design/engine-protocol|the engine protocol]]).

## Representation boundaries

- **∅ is refcount-free.** `Env::EMPTY` allocates nothing; small environments
  are `ThinArc`, large ones an `imbl` map, so an insert is O(log n) at any
  size. Compiled bounds: `Env ≤ 16`, `ValClos ≤ 48`, `Frame ≤ 128` bytes.
  Fused return avoids a per-result syntax allocation; larger values and η's
  binds remain real costs.
- **Dropping is trampolined.** A dying value queues each environment and each
  uniquely owned list, record or map node it alone owns, and a tag spine is
  walked in a loop, so a value of any nesting drops in constant stack. Raw IR
  owners claim no such safety.
- **The wire carries values as held.** One row kind remains, `Row::Env`. A
  value is `Value::Closure { tags, val, env }`, `Builtin` by name, `Residual`
  by name, row and count, or `Handle` by title; a built list or map is a
  closure like any other. `Val::Builtin`, `Val::Handle` and `CompKind::Builtin`
  skip serde; the reader rebuilds residuals with `BuiltinEntry::residual`,
  never decoded δ syntax.

This decision fixes the value representation the
[[map/core/evaluator|evaluator]] steps over and that
[[261102_the-checked-ir-is-the-calculus|the checked IR is the calculus]]
partitions.

## Where

- `core/src/ir.rs`: `Val`, `Node`, `IrData` and its `Fields`, `CompKind::Builtin`,
  the `Explicit` phase; it names `crate::builtin` and `crate::handle`, nothing
  in `crate::shell`.
- `core/src/ir/value.rs`: `ValClos`, `VClosRef`, `View`, the drop trampoline;
  `ir/value/list.rs`: `ListRef` and the list operations; `ir/value/map.rs`:
  `MapRef`, `ValClos::map`; `ir/value/first_order.rs`: `FOValue` conversions;
  `ir/env.rs`: `Env`, `Binding`, `Env::place`.
- `core/src/shell/runtime/close.rs`: `ValClos::close`;
  `shell/runtime/{compare,coerce}.rs`: equality, order and decoders on
  `VClosRef`; `core/src/builtin.rs`: `BuiltinEntry::eta`,
  `BuiltinEntry::residual`.
- `core/src/shell/evaluator/machine.rs`: `CompClosure`, `Next`,
  `Stack::settle`, `Stack::resume`, `Stack::unwind`;
  `shell/evaluator/assemble.rs`: cons, snoc and concat for literals and argv;
  `shell/runtime/error/record.rs`: `outcome_value`;
  `shell/builtins/misc.rs`: `raise`.
- `core/src/handle.rs`: `Handle::Withheld`; `core/src/marshal.rs`,
  `marshal/{wire,write,read}.rs`, `marshal/read/ir.rs`: `Marshalled`;
  `core/src/shell/runtime/state/construct.rs`: `Shell::marshal`,
  `Shell::child_of`; `exarch/src/shell_eval/builtins/harness.rs`: `` `start ``
  marshals the parent before enquiring.
