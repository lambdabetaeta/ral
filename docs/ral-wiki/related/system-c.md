---
verified_at_commit: 7ba500b
verified_at_date: 2026-06-17
against: [design/grant, design/effects-handlers, design/cbpv, design/syscalls-are-effects, design/scoping]
---

# System C — effects and capabilities reconciled

Brachthäuser, Schuster, Lee, Boruch-Gruszecki, *Effects, capabilities, and
boxes*, OOPSLA 2022.

**System C is the type-theoretic account of the choice [[design/grant|grant]]
makes dynamically: a capability is permission over the effect set, and
*mentioning* that authority is itself an effect.** It and ral meet on one
identity — in call-by-push-value terms, *boxing is thunking and unboxing is
forcing the effect of mentioning a capability* (System C §6.8, and the thesis on
p.3). Where ral resolves authority on the dynamic context and checks it at
runtime, System C lifts the same structure into types, toggled term-by-term.

## The shared thesis

- **Mentioning authority is an effect; the thunk delays it.** ral's value
  language is *inert* — `$`-deref, builtins, and the prelude perform nothing
  ([[design/cbpv|cbpv]], [[design/syscalls-are-effects|syscalls-are-effects]]).
  System C makes the same move one level down: a *block* (second-class function)
  may close over capabilities, but doing so is the effect, suspended by `box` and
  discharged by `unbox`. ral is full CBPV and already has the thunk; System C
  shows the same thunk can carry a *capability set*.

## Correspondences

- **Scope-based ↔ type-based is exactly ral's dynamic-authority choice.** System
  C's default (its base System Ξ) is *scope-based*: authority is whatever is in
  scope, with no effect types — the ergonomic pole. That is
  [[design/scoping|scoping]]'s "dynamic scope for ambient authority; a caller
  cannot lexically know which operations a callee performs." `box`/`unbox` is the
  *selective* crossing into types; ral simply never crosses.
- **Self-masking is capability-set subtraction, with a soundness theorem.** ral's
  handlers are deep, self-masking, tail-resumptive
  ([[design/effects-handlers|effects-handlers]]). System C's `try` rule reads
  `𝒞 = (𝒞₁ \ {f}) ∪ 𝒞₂` — the `\ {f}` *is* self-masking, and the
  ill-formedness of `σ at {f}` outside its delimiter is what forbids a capability
  escaping its handler (Theorem 3, Corollary 9).
- **grant is the *restriction* half of the judgment; ral has no *requirement*
  half.** System C's `𝒞` is read two ways: as input it is a context *restriction*
  (what is available) — grant's meet-narrowed authority; as output it is a
  context *requirement* (what the body actually uses). ral tracks only the
  restriction. Nothing in ral computes which operations a body will perform.
- **Capability sets form a lattice; grant's meet is its dual.** System C orders
  sets by inclusion (subeffecting). grant narrows authority by intersection with
  anti-monotone denies ([[design/grant|grant]]) — the same lattice, read on
  permission rather than on use.
- **Regions are `within`.** System C's scoped state (§5.2) needs no type ceremony
  while a handle is used second-class, and surfaces in the type only when *boxed*
  to escape. ral's `within [dir: …]` is morally a region; the pattern generalises
  to any scoped resource.

## Divergences (deliberate)

- **Open external names vs bound capabilities.** ral operations are *open
  external names* resolved through `PATH`
  ([[design/name-resolution|name-resolution]]); System C capabilities are
  lexically *bound* term variables introduced by handlers — a strict
  object-capability discipline. This is precisely grant's recorded concession
  ("bare command names are ambient … not a strict object-capability"). System C
  is the principled fix, at the cost of the ergonomics ral keeps.
- **ral is full CBPV; System C is fine-grain CBV (§6.8).** System C cannot
  abstract over computations — a block must be boxed to be returned. ral's thunks
  return computations directly, so ral is *more* expressive on that axis; System
  C's `box` is its workaround for what ral has natively.
- **No first-class `resume` is vindicated.** System C's handlers reify multi-shot
  continuations (its cooperative-multitasking example resumes twice). That forces
  a *cyclic* capability-set constraint on the continuation binder and fixed-point
  inference — the authors' hardest open problem (§5.3, §5.6), which they note
  "only arises since we offer support for first-class continuations." ral's
  tail-resumptive restriction
  ([[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]])
  buys exactly that inference simplicity.

## What ral could borrow

- **Selective boxing as a static-guarantee escape hatch — only where wanted.**
  ral stays scope-based by default. The case for crossing into types is a
  boundary that wants a *static* bound on the effect set rather than only the
  runtime grant chokepoint: the [[design/exarch-architecture|exarch]] sandbox, or
  a `spawn`'d thunk whose carried authority one wants to read off its type.
  System C is the recipe; adopting it broadly would contradict
  [[design/syscalls-are-effects|syscalls-are-effects]]'s dynamic-authority
  commitment, so it stays a hatch, not a turn. Rejected as a language feature
  on review
  ([[decisions/260603_related-borrowables-rejected|related-borrowables-rejected]]).

Cite: Brachthäuser, Schuster, Lee, Boruch-Gruszecki, *Effects, capabilities, and
boxes: from scope-based reasoning to type-based reasoning and back*, PACMPL 6
(OOPSLA 2022), [doi:10.1145/3527320](https://doi.org/10.1145/3527320). Zotero
`4CDKYAI4`.
