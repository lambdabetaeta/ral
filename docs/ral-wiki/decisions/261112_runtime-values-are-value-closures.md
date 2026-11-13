---
status: active
generated_at_commit: d50aa681
generated_at_date: 2026-11-13
verified_at_commit: d50aa681
verified_at_date: 2026-11-13
anchors: [Val, CompKind::Builtin, Explicit, ValClos, ValClos::close, ValClos::thunk, ValClos::view, ValClos::peel_variants, ValClos::residual, ValClos::block, BuiltinEntry::eta, Env::EMPTY, Stack::settle, CompClosure, Handle::Withheld, Marshalled, BuiltinEntry::residual]
---

# Runtime values are value closures

**There is one value syntax: a runtime value is a value closure ⟨V, ρ⟩ over
`ir::Val`, not a second calculus.** Its root is not a variable, and ρ binds
every *direct name*: an occurrence outside a thunk. A computation closure
⟨M, ρ⟩ belongs to the machine, not to the value grammar.

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
- **A component shares ρ.** Reading V′ from ⟨V, ρ⟩ yields ⟨V′, ρ⟩; a variable
  yields its bound value. No read restricts or consults Σ. `View` exposes one
  layer; a variant's absent payload means resolved unit, including a variable
  bound to unit.
- **Built data has implicit variables.** A built list is ⟨[xᵢ], {xᵢ ↦ vᵢ}⟩, a
  built map ⟨[:, k: xₖ…], {xₖ ↦ vₖ}⟩, held as persistent vectors and ordered
  maps: logarithmic updates, and keys that are not lexical names. A variant
  shares its payload's syntax and ρ and binds a built collection, builtin,
  handle or η residual payload to one fresh name, so those occur only at a
  closure's root.
- **Builtins force to η-expansions.** An arity-n entry forces to
  ⟨λa₀…λaₙ₋₁. δ(b; a⃗), {b ↦ c}⟩. β binds each argument; δ reads the entry and
  arguments from ρ. Partial application is an ordinary residual λ, boxed by a
  value-receiving frame. A call with exactly the entry's arity runs it
  directly.
- **Returns are fused.** The machine state is a `CompClosure`: ⟨M, ρ⟩ or
  `Err(e)`. `Stack::settle` hands a produced value down the continuation at
  once, so there is no value register and no `return` node at runtime.
  `raise e` is `Err(e)`, not an IR form, so runtime explanations need no
  equality or serialisation in the IR.

## Why η, not argument counting

**A residual function must cross a provenance frame as a λ.** In
`let m = { |f| map $f }; m g $xs`, β binds `f`, then the body's spanned
application puts `CallSite` above `Arg xs`. Counting adjacent `Arg` frames
lets `force map` see one argument; returning a thunk there makes it meet
`Arg xs` as a value: "not a function". η leaves a λ, which crosses `CallSite`
like every λ.

δ's entry comes from ρ, not Σ, so a host hook or override keeps its own
builtin identity. `ValClos::block` hands the checker only a non-residual
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
  are `ThinArc`. Compiled bounds: `Env ≤ 16`, `ValClos ≤ 48`, `Frame ≤ 128`
  bytes. Fused return avoids a per-result syntax allocation; larger values and
  η's binds remain real costs.
- **Tag spines are walked once.** Ownership operations, semantic walks and
  value conversions treat a spine iteratively (`ValClos::peel_variants`).
- **Dropping is trampolined.** A dying value queues the capture, built vector
  or built map it alone owns, so a value of any nesting drops in constant
  stack. Other walks over built data, and raw IR owners, claim no stack
  safety.
- **The wire carries values as held.** Closures keep syntax and environment
  row; built lists and maps name shared rows; builtins cross by name, handles
  by title, residuals by name, row and count, caught failures by their fact.
  `Val::Builtin`, `Val::Handle` and `CompKind::Builtin` skip serde; the reader
  rebuilds residuals with `BuiltinEntry::residual`, never decoded δ syntax.

This decision fixes the value representation the
[[map/core/evaluator|evaluator]] steps over and that
[[261102_the-checked-ir-is-the-calculus|the checked IR is the calculus]]
partitions.

## Where

- `core/src/ir.rs`: `Val`, `CompKind::Builtin`, the `Explicit` phase; it names
  `crate::builtin` and `crate::handle`, nothing in `crate::shell`.
- `core/src/shell/runtime/{value,env,list,map}.rs`: `ValClos`, `View`,
  `ValClos::block`, one-layer `read`, the drop trampoline, `Env`, `ListRef`,
  `MapRef`; `core/src/builtin.rs`: `BuiltinEntry::eta`,
  `BuiltinEntry::residual`.
- `core/src/shell/evaluator/machine.rs`: `CompClosure`, `Next`,
  `Stack::settle`, `Stack::resume`, `Stack::unwind`.
- `core/src/handle.rs`: `Handle::Withheld`; `core/src/marshal.rs`,
  `marshal/{wire,write,read}.rs`: `Marshalled`.
- `core/src/shell/runtime/state/construct.rs`: `Shell::marshal`,
  `Shell::child_of`; `exarch/src/shell_eval/builtins/harness.rs`: `` `start ``
  marshals the parent before enquiring.
