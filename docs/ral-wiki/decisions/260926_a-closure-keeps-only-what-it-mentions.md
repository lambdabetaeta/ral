---
status: accepted
generated_at_commit: 2ded530f
---

# A closure keeps only what it mentions

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
- **Narrowing that drops nothing is the identity, root included.** The wire
  decode, the fork's scrub and `Rec`'s siblings are each handed an environment
  already scrubbed for the same body, so each keeps the root the wire and the
  scrub intern by; every empty capture is the one empty map.
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
- **Rows shrink; values are not shared across them.** A seed's rows are per
  closure and hold what each mentions. A value two closures mention is
  written in both rows: deduplicating it is the sharing-preserving encoding's
  job, not this decision's.

## What it costs

Every thunk value walks its body, looks up each occurrence and builds a small
map where it once bumped an `Arc`: every thunk literal evaluated, every
decoded and every scrubbed closure. Every recursive call builds one sibling
per group member, each walking the whole group. The wire's root scan is
linear, so interning is O(roots²) with roots about the closures in a message.
No benchmark gates it.

## Alternatives rejected

- `FV` cached on the `thunk` and `rec` nodes — a memo of this walk: cheaper
  closures, two annotated IR structs.
- Scrubbing at `close` alone — a law that holds by audit, not construction.
- `explain` as a special form.
- `Scoped` over `shell.env` — dynamic scope in lexical clothes.
- A worker whose session is its capture — session reads would see only what
  the body mentions.

Refines [[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]]:
its O(1) capture is a scrub, and what is in focus is a computation closure, a
type apart from the thunk value.

Plan: `dev/docs/plans/260926_free_variable_capture.md`. Narrative:
[[internals/evaluator-machine|evaluator-machine]]; residency:
[[internals/binding-leases|binding-leases]].
