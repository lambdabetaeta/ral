---
status: active
generated_at_commit: f9ac94f8
generated_at_date: 2026-11-12
verified_at_commit: f9ac94f8
verified_at_date: 2026-11-12
anchors: [Val, CompKind::Builtin, Explicit, ValClos, ValClos::close, ValClos::thunk, ValClos::view, ValClos::peel_variants, ValClos::residual, ValClos::block, BuiltinEntry::eta, Env::EMPTY, Machine::settle, Handle::Withheld, Marshalled, BuiltinEntry::residual]
---

# Runtime values are value closures

> **Amended 2026-11-12 — Marshalling:**
> [[261013_a-child-starts-from-its-parent-marshalled|a child starts from its parent, marshalled]]
> gives both seats one byte route. Writing handles as titles withholds them
> in place; no separate scrub runs. Residuals cross by builtin, row and count;
> built collections cross as shared rows, and caught records keep their
> failure witness. The value-closure and η laws stand.

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
  built collection, builtin, handle or η residual payload to one fresh name,
  so a builtin, handle or η residual sits only at a closure's root.
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
builtin has type name `Lambda` and crosses as its builtin name, environment
row and supplied argument count; the receiver rebuilds its own η suffix.
Its display still names the builtin and supplied argument count.

## Why withheld, not omitted

**Omitting a handle binding is not unbinding it.** Lookup falls through to
Σ: dropping `lines ↦ h` exposes Σ's `lines` instead. Removing a list element
also renumbers positions; removing a record field changes its shape.
`Handle::Withheld` retains the binding, position, title and type `Handle`;
observing the worker fails `withheld-handle`, directing the caller to await
it in its spawning shell and pass the result.

Every child reads `Marshalled` bytes. The writer visits environments and
built collections once, writing every handle as its title; the reader
constructs withheld handles through data, captures and handler arms.
**No live handle remains reachable in the child's inherited image.**

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
- **The wire carries values as held.** Closures keep their syntax and
  environment row; built lists and maps name shared rows. Builtins cross by
  name, handles by title, residuals by name, row and count, and caught failure
  records by their fact. Tag spines cross flat. `Val::Builtin`, `Val::Handle`
  and `CompKind::Builtin` skip serde in both directions; the reader rebuilds
  residuals with `BuiltinEntry::residual`, never decoded δ syntax.
- **Agda correspondence is a follow-up**, not a new mechanisation claim.
  This decision amends the representations in
  [[260826_the-evaluator-steps-closures|the evaluator steps closures]],
  [[260926_a-closure-keeps-only-what-it-mentions|a closure keeps only what it mentions]],
  [[261013_a-child-starts-from-its-parent-marshalled|a child starts from its parent, marshalled]]
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
  `core/src/marshal.rs`, `marshal/{wire,write,read}.rs`: `Marshalled`;
  `core/src/builtin.rs`: `BuiltinEntry::residual`.
- `core/src/shell/runtime/state/construct.rs`: `Shell::marshal`,
  `Shell::child_of`; `exarch/src/shell_eval/builtins/harness.rs`:
  `fork_then_enquire` marshals before enquiring.
