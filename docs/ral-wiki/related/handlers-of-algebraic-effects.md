---
verified_at_commit: 7ba500b
verified_at_date: 2026-06-17
against: [design/effects-handlers, design/syscalls-are-effects, design/pipelines]
---

# Handlers of algebraic effects — the calculus ral's handlers restrict

Plotkin, Pretnar, *Handlers of Algebraic Effects*, ESOP 2009.

**The founding handler calculus — built, like ral, on call-by-push-value — whose
motivating examples are shell operations: `proc > /dev/null` and `yes | proc`
are its §6.2.** An operation is an effect *constructor*; a handler is an effect
*deconstructor*; handling a computation is the unique homomorphism out of the
free model, induced by a programmer-supplied model of the effect theory. ral's
handlers ([[design/effects-handlers|effects-handlers]]) are the tail-resumptive
fragment of this construct, and ral's domain is the paper's example domain made
primary.

## Correspondences

- **Operation vs interpretation is the founding move.** The theory's operations
  are syntax; the free-model monad interprets `F σ`; a handler supplies another
  model. [[design/syscalls-are-effects|syscalls-are-effects]] is this picture
  with the OS as the ambient model: performing `git` is an operation of the
  free theory, the OS its default interpretation, `within [handlers: …]` a
  user-supplied remodelling.
- **Self-masking is the homomorphism's own scoping.** In the handling equation
  (§7), recursion passes only through the continuation variables `zᵢ`:
  operations occurring elsewhere in a clause body are interpreted by the
  *ambient* model, never by the handler being defined, while the continuation
  is re-wrapped (deep). ral's strip-and-restore handler stack
  ([[internals/handler-dispatch|handler-dispatch]]) is the operational
  realisation — deep + self-masking is not a deviation but the algebraic
  semantics run on a stack.
- **Unhandled means forwarded.** Their omitted clause defaults to
  `opₓ(xᵢ. zᵢ(xᵢ))` — re-perform and continue. ral's fall-through to the next
  frame, and ultimately to the OS.
- **Stream redirection is the shared example.** Their
  `{out_c(z) ↦ z, in(z) ↦ z(y)}` is `> /dev/null` plus `yes |` as one handler;
  ral's wrap-and-forward `within [handlers: [git: { |args| … }]]` idiom is the
  same shape.

## Divergences

- **ral keeps only tail resumption.** Their clauses hold continuations as
  variables to invoke zero, once, or many times — explicit nondeterminism
  collects both branches, rollback reverts state, timeout aborts — multi-shot
  is the point of several examples. A ral clause returns a value that *is* the
  implicit, exactly-once, tail resumption; first-class `resume`, the
  Benton–Kennedy value clause, and parameter passing are all absent. The cut is
  domain-honest: re-running a session against a world already perturbed is
  incoherent, and multi-shot earns its keep for pure effects ral does not model
  as commands ([[design/syscalls-are-effects|syscalls-are-effects]]).
- **No correctness obligation, because ral's theory is free.** The paper splits
  handler *definition* from handler *use* into two calculi so the meta-level
  can check each handler is a model — that its clauses respect the effect
  theory's equations — and flags thunks as the obstruction to merging the two.
  ral takes the single-language alternative their §9 sketches, and the
  obligation is vacuous by design: the external-command theory has **no
  equations** (the world validates none), every clause set is trivially a
  model, so handlers are ordinary runtime blocks installed by `within` — thunks
  included.
- **The pipe is not a handler — and could not be.** They cannot express
  `t₁ | t₂` ("the difficulty is very much like that with the CCS parallel
  combinator") and leave it as the paper's principal open question. ral agrees
  by architecture: the pipe is a primitive computation combinator over the byte
  channel — a process group or a fold, selected by the edge type
  ([[design/pipelines|pipelines]]) — orthogonal to the handler stack, not a
  deconstruction of one computation.

## What ral could borrow

- **Parameter-passing handlers (§6.5), which need no continuations.** A handler
  threading a parameter across interceptions — their
  suppress-output-after-*N*-characters, timeout, input-redirection examples —
  degenerates, in the tail-resumptive fragment, to a clause of shape
  `(args, param) → (result, param′)`: a stateful fold over the operation
  stream. ral's clauses are stateless between interceptions; this would add
  expressiveness (rate-limiting, replay-from-script mocks) without touching
  the no-first-class-`resume` line. Proposed as
  [[decisions/260603_stateful-handlers|stateful-handlers]].

Cite: Plotkin & Pretnar, *Handlers of Algebraic Effects*, ESOP 2009,
[doi:10.1007/978-3-642-00590-9_7](https://doi.org/10.1007/978-3-642-00590-9_7).
Zotero `QIBEF77V`. ral side:
[[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]];
`docs/SPEC.md` §3.2.
