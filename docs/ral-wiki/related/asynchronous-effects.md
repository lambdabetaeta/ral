---
verified_at_commit: 62f8f2a9
verified_at_date: 2026-10-19
against: [design/engine-protocol, design/effects-handlers, design/agents]
---

# Asynchronous effects — the calculus of the engine protocol

Ahman, Pretnar, *Asynchronous Effects*, POPL 2021; and its sequel,
*Higher-Order Asynchronous Effects*, LMCS 2024. The language is Æff.

**Æff decouples an algebraic operation's call from its answer: a computation
issues outgoing *signals* and receives incoming *interrupts*, each one-way, and
recovers a synchronous call as a signal, a promise of the reply, and an
`await`. ral's engine protocol ([[design/engine-protocol|engine-protocol]]) is
this picture with the engine and its host as the two processes — except that ral
keeps the synchronous call primitive, gives interrupts only to the runtime, and
adds a scope at which signals wait.**

## Correspondences

- **Signals are `Surface`.** `↑op(V, M)` issues `op` and continues as `M`
  without waiting; `surface` hands the host a value and continues.
- **Interrupts are `Control`.** `↓op(V, M)` arrives at any point of `M`, from
  outside it; `Interrupt`, `Cancel`, `Terminate` and `Abort` arrive at any
  point of a run, out of band.
- **Promises are handles.** Æff's `promise … as p` yields a promise `p` and
  `await p until ⟨x⟩ in M` blocks on it; `spawn` yields a handle and `await`
  blocks on it. The sequel adds dynamic process creation, which is `spawn`
  proper.
- **The synchronous call is derived.** In Æff a call that wants an answer is a
  signal out, an interrupt handler for the reply, and an `await`. `enquire` is
  exactly that sequence, built in.
- **Parallel composition is the carrier.** Æff's `P ∥ Q` turns one process's
  signals into the other's interrupts; the carrier does the same for engine and
  host, in one address space or across a socket.

## Divergences

- **No program-level interrupt handlers.** In Æff a computation installs
  handlers for the interrupts it expects. In ral only the runtime handles
  `Control` (`Scopes::apply`); the nearest a program comes is a `guard`
  cleanup, which runs on a stop but cannot resume the computation.
- **The synchronous call stays primitive.** Decomposed, an enquiry's reply could
  arrive after its dispatch's `Report`; fused, it cannot, which is what keeps a
  transcript ordered. A worker, which outlives its `Report`, is given no desk.
- **Signals wait for their scope.** An Æff signal propagates at once. A detached
  worker's `surface` is handled at session scope and rendered at the host's next
  boundary ([[decisions/260622_surface-delivers-itself|surface-delivers-itself]]),
  so the session is a handler Æff does not have: a buffer emptied at a boundary.
- **Point to point, not broadcast.** Æff broadcasts a signal to every parallel
  process. An engine speaks only to its host; a host fans out by its own
  machinery ([[design/agents|agents]]).
- **First-order payloads, not modal ones.** The sequel types code that may cross
  between processes with a modality. ral admits only first-order `FOValue`s at an
  open payload position, and carries a forked session as opaque sealed bytes.

## The same shape elsewhere

- **A Unix process and its kernel.** A write is a signal, a blocking system call
  a synchronous operation, a signal an interrupt — and the kernel makes a
  pending signal visible only on the return to user mode, as the session scope
  renders only at a boundary.
- **JSON-RPC and LSP.** Requests with ids and matched responses, notifications
  without, and cancellation as a notification (`$/cancelRequest`). The engine
  protocol's comparison table sets it beside them.

## What ral could borrow

- **Interrupt handlers for long-lived workers.** A long-lived worker that must
  react to its host mid-flight now has no way to hear it; an Æff-style handler is the
  principled shape, should that need arrive.
- **The session scope as a handler in the calculus.** Æff gives signals and
  interrupts an operational semantics; a boundary-flushed handler would give the
  same to ral's deferred channel.
