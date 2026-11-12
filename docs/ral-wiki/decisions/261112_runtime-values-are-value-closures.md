---
status: active
generated_at_commit: c0905f5e
generated_at_date: 2026-11-12
verified_at_commit: c0905f5e
verified_at_date: 2026-11-12
anchors: [Val, CompKind::Builtin, Explicit, ValClos, ValClos::close, ValClos::thunk, ValClos::view, ValClos::peel_variants, ValClos::residual, ValClos::block, BuiltinEntry::eta, Env::EMPTY, Machine::settle, Handle::Withheld, SerialAtom]
---

# Runtime values are value closures

**There is one value syntax: a runtime value is a value closure ⟨V, ρ⟩ over
`ir::Val`, not a second calculus.** Its root is not a variable, and ρ binds
every *direct name*: an occurrence outside a thunk. A computation closure
⟨M, ρ⟩ belongs to the machine, not to the value grammar.

## Decision

- **Constants are syntax.** Bytes, builtins and handles are `Val` forms;
  the elaborator writes none. Scalar and runtime-constant constructors use
  ⟨V, ∅⟩; a thunk is ⟨thunk M, ρ⟩, and a literal is ⟨[V…], ρ⟩ (likewise records
  and maps). Hosts construct builtin values through public `ValClos::builtin`.
- **Restriction happens at formation, not observation.** `ValClos::close`
  restricts ρ to occurrences and pulls each missing direct name's value
  from Σ into the capture, rather than eagerly building the literal.
  `ValClos::thunk` restricts to the body's occurrences; names in its body
  resolve ρ-then-Σ when it runs.
- **A component shares ρ.** Reading V′ from ⟨V, ρ⟩ yields ⟨V′, ρ⟩;
  a variable yields its bound value. No component read restricts or consults
  Σ. `View` exposes one layer; a variant's absent payload means resolved
  runtime unit, including a variable bound to unit, detected without a
  recursive view.
- **Built data has implicit variables.** A built list is
  ⟨[xᵢ], {xᵢ ↦ vᵢ}⟩; a built map is ⟨[:, k: xₖ…], {xₖ ↦ vₖ}⟩.
  Persistent vectors and ordered maps represent their positional or keyed
  environments, retaining logarithmic updates without making keys lexical
  names. A variant shares its payload's syntax and environment, binding a
  built collection, builtin or handle payload to one fresh name, so a
  builtin or handle sits only at a closure's root.
- **Builtins force to η-expansions.** An arity-n entry forces to
  ⟨λa₀…λaₙ₋₁. δ(b; a⃗), {b ↦ c}⟩. β binds each argument; δ reads the
  entry and arguments from ρ. Partial application is an ordinary residual
  λ, boxed by a value-receiving frame. A call holding exactly the entry's
  arity runs it directly, without constructing that application chain.
- **Returns are fused.** The persistent machine state is ⟨M, ρ⟩ or
  `Err(e)`; `Machine::settle` immediately passes produced values down the
  continuation. Written `return V` closes V, but runtime results allocate
  no `return` node and occupy no persistent value register. `raise e`
  remains `Err(e)`, a closed zero-ary operation, rather than an IR form
  imposing equality and serialisation on runtime explanations.

## Why η, not argument counting

**A residual function must cross a provenance frame as a λ.** In
`let m = { |f| map $f }; m g $xs`, β binds `f`, then the body's spanned
application puts `CallSite` above `Arg xs`. Counting only adjacent `Arg`
frames lets `force map` see one argument: returning a thunk there makes it
meet `Arg xs` as a value, hence “not a function”. η leaves a λ, which crosses
`CallSite` by the same rule as every λ.

δ's entry comes from ρ, not Σ: an externally supplied host hook or override
keeps its own builtin identity. Residual inspection serves display and the
hook/handler validation doors: `ValClos::block` hands the checker only a
non-residual thunk, keeping runtime δ out of inference. A partial
builtin has type name `Lambda` and crosses the wire as a thunk; its display
still names the builtin and supplied argument count.

## Why withheld, not omitted

**Omitting a handle binding is not unbinding it.** Lookup falls through to
Σ: dropping `lines ↦ h` exposes Σ's `lines` instead. Removing a list element
also renumbers positions; removing a record field changes its shape.
`Handle::Withheld` retains the binding, position, title and type `Handle`;
observing the worker fails `withheld-handle`, directing the caller to await
it in its spawning shell and pass the result.

The bequest scrub remains necessary even though the wire encodes every
handle by title: `Fork::Park` adopts in process and shares memory with its
parent. The memoised environment-DAG walk replaces live handles with
withheld handles through data, captures and handler arms, sharing unchanged
subgraphs. **No live handle remains reachable in the bequest.**

## Representation boundaries

- **∅ is refcount-free.** `Env::EMPTY` has no allocation; `Env`'s `Small`
  representation uses `triomphe::ThinArc`. Compile-time bounds are
  `Env ≤ 16`, `ValClos ≤ 48`, `Frame ≤ 128` bytes. This avoids a shared
  refcount touched by every scalar clone; fused return avoids per-result
  syntax allocation. Larger values and η's binds remain real costs.
- **Consecutive tags are walked once.** Runtime ownership operations,
  semantic walks and value conversions handle tag spines iteratively;
  printing groups their labels before rendering one terminal child. Cloning
  a root syntax spine costs O(n), and a component read may copy its suffix.
- **Dropping is trampolined.** A dying value queues the capture, built
  vector or built map it alone owns, so a value of any nesting drops in
  constant stack. Other walks over built data, and raw IR owners, make no
  stack-safety claim.
- **The wire carries values, not runtime constants embedded in syntax.**
  `SerialValue = FOValue<SerialAtom>` adds thunks, builtin names and handle
  titles. Builtin names re-link against the receiving runtime's manifest;
  handles hydrate withheld. `Val::Builtin` and `Val::Handle` skip serde;
  δ carries names, not an entry. Built collections encode their contents.
- **Agda correspondence is a follow-up**, not a new mechanisation claim.
  This decision amends the representations in
  [[260826_the-evaluator-steps-closures|the evaluator steps closures]],
  [[260926_a-closure-keeps-only-what-it-mentions|a closure keeps only what it mentions]],
  [[261013_the-seed-rides-the-protocol-sealed|the seed rides the protocol, sealed]]
  and [[261102_the-checked-ir-is-the-calculus|the checked IR is the calculus]].

## Where

- `core/src/ir.rs`: `Val`, `CompKind::Builtin`, the `Explicit` phase; it
  names `crate::builtin` and `crate::handle`, nothing in `crate::shell`.
- `core/src/shell/runtime/{value,env,list,map}.rs`: `ValClos`, `View`,
  `ValClos::block`, one-layer `read`, the drop trampoline, `Env`, `ListRef`,
  `MapRef`; `core/src/builtin.rs`: `BuiltinEntry::eta`.
- `core/src/shell/evaluator/machine.rs`: `State`, `Next`, `Machine::settle`,
  `Machine::resume`, `Machine::unwind`.
- `core/src/handle.rs`: `Handle::Withheld`;
  `core/src/shell/runtime/state/scrub.rs`: `Shell::bequeath`;
  `core/src/seed/{table,json}.rs`: `SerialAtom`.
- `exarch/src/shell_eval/builtins/harness.rs`: `fork_then_enquire` chooses
  parked or sealed adoption of the same scrubbed bequest.
