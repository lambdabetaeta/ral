---
status: accepted
generated_at_commit: 9290eb03
generated_at_date: 2026-11-13
verified_at_commit: 9290eb03
verified_at_date: 2026-11-13
anchors: [ValClos::close, ValClos::thunk, Env::restrict, Node::new, Node::occ, ListRef, MapRef]
amended_by:
  - "[[261102_the-checked-ir-is-the-calculus|the checked IR is the calculus]]"
  - "[[261112_runtime-values-are-value-closures|runtime values are value closures]]"
---

# A closure keeps only what it mentions

> **Amended 2026-11-13:** a list's tail keeps its list's ρ, so `rest` in
> `[x, ...rest]` holds the bindings of the elements it dropped until it dies.
> This is the one exception to the law: restricting at every step would make a
> loop over a list quadratic. A node built at run time computes its occurrence
> set on first use, so only source literals pay for one at formation
> ([[261112_runtime-values-are-value-closures|runtime values are value closures]]).

> **Amended 2026-11-12 — Marshalling:**
> [[design/engine-protocol|the engine protocol]]
> writes each reached environment once, identified by allocation, in
> dependencies-first rows. A closure retains its syntax and row; shared
> captures stay shared. This replaces the
> transfer scrub and quadratic root scan, not formation-time restriction.

> **Amended 2026-11-12:**
> [[261112_runtime-values-are-value-closures|runtime values are value closures]]
> places restriction at value formation: `ValClos::thunk` and `ValClos::close`
> use occurrence sets, cached in each node. Closing data
> additionally pulls every missing direct name's value from Σ into ρ;
> names beneath a thunk still resolve ρ-then-Σ when it runs. Reading a
> component V′ of ⟨V, ρ⟩ shares ρ without restriction or Σ lookup, so a
> component may retain its siblings' names. Computation closures remain
> transient and unrestricted except at explicit `Rec` formation. Empty
> captures use refcount-free `Env::EMPTY`; nonempty restriction that drops
> nothing preserves identity. The constructor and per-capture walk claims
> below are historical, not requirements on observation.

> **Amended 2026-11-02:**
> [[261102_the-checked-ir-is-the-calculus|the checked IR is the calculus]]
> adds `Seq` beside `To` as a frame carrying a continuation environment;
> argument frames are `Arg`, one per argument. The occurrence-set invariant
> stands. Occurrence sets are cached by `Node::new` and read by
> `Closure::new`, not recomputed on each capture.

> Amended 2026-10-19 by [[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]: `service`
> is deleted, so the workers that start from their creator's session are `spawn`,
> `watch`, `par`/`all` children and a pipeline stage.

**A thunk value is ⟨M, ρ|occ(M)⟩: its computation over just the session
bindings its body mentions.** The law holds by construction: `Closure`'s
fields are private and `Closure::new` is its one constructor, which narrows
the environment it is handed (`Env::restrict`) to the names
`ir::referenced_names` finds in `M` — the lease harvest's walk, exhaustive
and binder-blind. By the standard lemma ⟦M⟧ρ = ⟦M⟧(ρ|occ(M)) nothing
observable changes; what changes is *residency*: a closure formed after
`let big = …` keeps `big` alive only if it mentions it.

## What was decided

- **occ, not FV.** The walk collects every name `M` mentions, bound or free:
  every variable, every bare command head, every member of a `Rec` group. The
  surplus over FV(M) — a session `x` that `M` rebinds before reading, a
  binding named like a `^name` head, a sibling's session namesake — is kept
  and never read: it costs bytes, never meaning. Only the session tier is
  narrowed; the natives and prelude tiers stay shared whole.
- **Narrowing that drops nothing is the identity, root included.**
  `Rec`'s siblings keep an already restricted environment's allocation.
  Marshalling identifies environments by that allocation; readback validates
  direct names without restricting the row. Every empty capture is `Env::EMPTY`.
- **The machine's ⟨M, ρ⟩ are computation closures, a type apart.**
  `Focus::Eval { comp, env }` and `Terminal::Lambda { comp, env }` hold the
  whole environment `M` was reached in. They are transient — never stored,
  sent, or kept alive by a binding — and stay unscrubbed: scrubbing them too
  is Clinger's S_sfs, a scrub per step, and not this decision.
- **U-β.** `Force(Thunk M)` puts `M` in focus under the current environment:
  a literal `!{ … }` closes nothing and walks nothing.
- **No native reads a lexical environment.** `To` alone holds an `Env`, the
  one frame with syntax to close over it; `Apply`, `Try` and `Guard` carry
  none, and `BuiltinBody::Scoped` is gone.
- **`explain` and `help` reflect on the session.** `explain name` answers for
  the session's namespace — `shell.env` and the shell's registries. A
  block-local or a lambda parameter is not a session name and never had a
  scheme there.
- **A worker starts from its creator's session and forces its thunk.**
  `spawn`, `watch`, `service` and a pipeline stage seed the child's
  `shell.env` from the spawning shell's session; the body's capture rides in
  its closure. `use` and `explain` inside a worker read the session it was
  born into, so `spawn { explain x }` and `!{ explain x }` agree.
- **Restriction and wire sharing are separate laws.** Captures keep only
  the names formation requires. Marshalling writes one row per reached
  environment, preserving shared allocations rather than copying them per
  closure. Code is written once per closure holding it.

## What it costs

At this decision, every thunk value walked its body, looked up each occurrence
and built a small map where it once bumped an `Arc`: every thunk literal
evaluated, every decoded and every scrubbed closure. Every recursive call
built one sibling per group member, each walking the whole group. The caching
amendment above qualifies those formation costs. Marshalling's identity table
takes O(rows) bookkeeping, with one expansion per allocation. No benchmark
gates it.

## Alternatives rejected

- `FV` cached on the `thunk` and `rec` nodes — a memo of this walk: cheaper
  closures, two annotated IR structs.
- Scrubbing at `close` alone — a law that holds by audit, not construction.
- `explain` as a special form.
- `Scoped` over `shell.env` — dynamic scope in lexical clothes.
- A worker whose session is its capture — session reads would see only what
  the body mentions.

Refines the [[map/core/evaluator|evaluator]]:
its O(1) capture is a scrub, and what is in focus is a computation closure, a
type apart from the thunk value.

Narrative:
[[internals/evaluator-machine|evaluator-machine]]; residency:
[[internals/binding-leases|binding-leases]].

## Where

- `core/src/ir/value.rs`: `ValClos::thunk`, `VClosRef::view`;
  `core/src/shell/runtime/close.rs`: `ValClos::close`; `ir/value/list.rs`
  and `map.rs`: `ListRef`, `MapRef`.
- `core/src/ir/env.rs`: `Env::restrict`, `Env::EMPTY`;
  `core/src/ir.rs`: `Node::occ` caches occurrences on first use.
