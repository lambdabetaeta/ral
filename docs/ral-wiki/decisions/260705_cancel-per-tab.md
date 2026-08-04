---
status: active
---

# Esc and Ctrl-C are a per-tab turn interrupt

**Esc and Ctrl-C unwind only the focused tab's current turn — they never cascade
to a subtree and never kill an agent.** The two keystrokes had fused two unrelated
gestures onto one press: *interrupting a turn* ("stop what this tab is doing now")
and *ending an agent* ("tear down this delegation tree"). A cancel key must only
ever do the first, and only to the tab under your eyes. Lifecycle death stays
explicit and separate — `/quit`, `/clear`, the reaper ceiling, and the
`agent_cancel` tool are the only ways an agent dies, and the subtree-owning ones
keep cascading ([[design/agents|agents]]).

This generalises [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]'s
rule — "an Esc cancels a turn, not the agent" — which the code delivered only for
a `Held` agent (the trunk), from that one node to *every* tab. A non-`Held` parked
agent used to die the instant it observed a tripped token, so interrupt was death
for exactly the agents a human was not watching.

## The cause on the token

The drive-loop `cancel::Token` was a bare flag; it now carries a `CancelCause`
discriminant (`0` = uncancelled) and is read two-valuedly at the park — interrupt
versus terminate ([[decisions/260706_signals-are-causes|signals-are-causes]] gives
the cause vocabulary, `Interrupt < … < Terminate`):

- `is_cancelled()` — true for *any* cause — still unwinds the in-flight turn at
  ral's next poll point, so both an interrupt and a terminate drop the current
  turn.
- `terminated()` — new: true for any cause **but** `Interrupt` — is what the park
  reads. A non-`Held` agent ends only on a terminate-cause cancel; an
  interrupt-cause cancel drops the turn and the agent re-parks.

So the kill that used to fire whenever a non-`Held` parked agent saw a tripped
token now fires only for a *lifecycle* terminate. The cascade terminators
(`agent_cancel`, the ceiling reaper, `/clear`) pass a terminate cause through
`cancel_entry`; `raise()` and the frontend interrupt store `Interrupt`. A real
SIGTERM/SIGHUP still escalates through core's separate handler, untouched.

## One entry, no walk

The `Cancel` arm is now trunk-versus-not, and both keys keep returning `Cancel`,
so the key layer needs no split:

- **Off the trunk** it routes to `AgentRegistry::interrupt(id)` — the single-entry,
  non-cascading turn cancel. It trips one entry's token and its reach's run
  scope with `Interrupt` — *no descendant walk, no deregistration, and
  `eval_root` never touched* — unwinding one turn and nothing else.
- **On the trunk** `raise_interrupt()` already *is* the trunk's single-turn
  interrupt (its published foreground slot plus the ral foreground child,
  [[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]), so the old
  unconditional `cancel(focused)` cascade beside it was simply deleted.

> **Superseded, in part.** Every registry entry now carries a reach, the trunk
> included — `AgentRegistry::interrupt(focused)` runs on the trunk too, and
> `raise_interrupt()` is no longer the trunk's *whole* interrupt, only what the
> registry path cannot reach: the SIGINT re-created for a foreground external
> child, and the ambient foreground stamp, which needs no dispatch handle to
> land. See [[internals/cancellation|cancellation]] and
> [[map/exarch/agent|agent]] for the current shape.

A focused agent parks `Held`, so the tab you just interrupted is always left alive
and steerable — the whole point. A defocused idle sub-agent you interrupt and then
`TAB` away from still quiesces and reaps, but that is the ordinary idle-reclaim
lifecycle, not the keystroke, and it is unchanged.

## Why an interrupt cannot orphan

An interrupt never removes or settles an entry, so it can never strand a settled
parent over live registered descendants — the failure the no-orphans invariant
forbids. That invariant keeps riding on the existing `reply`-path
`cancel_descendants` reap ([[design/agents|agents]]), untouched; the lifecycle
terminators already cascade, so none of them orphans either. No reap moved.

## See also

[[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
(the prior Esc semantics this generalises),
[[decisions/260706_signals-are-causes|signals-are-causes]] (the cause vocabulary
the token now carries),
[[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]] (the token +
eval-root layering an interrupt trips),
[[design/agents|agents]] (the no-orphans invariant and the surviving terminator
cascade). Maps: [[map/exarch/frontend|frontend]], [[map/exarch/agent|agent]].
