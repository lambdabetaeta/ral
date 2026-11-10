# The terminal returns before the pumps

**Terminal responsibility returns once its tenants are dead, before output
drains are joined, for a lone command and a pipeline alike.**
`CollectState::settle_lone` consumes the opaque `TerminalHold` first, then
finishes the jail, then joins pumps. A pipeline's `fold` opens with
`CollectState::vacate`: it hears out every key the ended anchor reported,
reclaims the hold once no stage is live, and only then settles each stage.
An envelope's hold restores modes without handing foreground over or
attributing a key; callers never inspect the hold's representation.

- The order does not depend on a cause: under a cause the later join only gains
  a kill of the owned group. On unwind the collector kills unobserved stages,
  then its opaque hold restores on drop, with no pump join.
- A returned start failure has the same settlement obligation. For a refused
  handoff or refused report, `Launch::born` retains the request while killing
  an envelope's named payload group and any fresh group its applied `Step`
  leads, then killing/reaping the child. Only then
  does it restore state and release the jail. `Prepared::start` returns
  `StartFailure`, owning its reason and drains. A lone run settles before
  propagating the reason; a pipeline rolls back and joins, returning its
  terminal, before settling failed-start drains and reporting the error.
  Even a failed joined start may leave pipe-holding descendants for its owner
  to kill. The final settlement callback is empty: no group ID is killed
  after its pin has been reaped.
- A pump can outlast its tenant for as long as a descendant holds the pipe, and
  the shell must not sit in a background pgroup meanwhile: its next terminal
  read would fail, and a key pressed then would reach only the stragglers left
  in the tenant's group, never ral, so nothing could interrupt the join. Back
  in the foreground, ral hears the key and strikes a cause, and the join kills
  the group.
- The price is a straggler's: one that reads the terminal now gets `SIGTTIN`,
  and a cause still reaches it through the group kill.
- With no anchor, nobody reports a lone command's key. The loan hears it as the
  leader's death by a key signal (`TerminalHold::reclaim(outcome.death())`,
  read through `gesture`) and strikes it on the run's node; the current scope,
  read once when the pumps are done, decides whether it ends the command or a
  shield masks it.
- A pipeline's anchor reports each key as `Event::Heard`, struck as it is heard.
  The anchor ends before the fold, so `vacate` strikes the last of them before
  the terminal returns, and the reclaim itself hears nothing
  (`TerminalHold::reclaim(None)`). A collector left with a stage unobserved
  keeps its hold until it drops, after its forced end's kill.

See [[internals/cancellation|cancellation]],
[[internals/pipeline-execution|pipeline-execution]] and
[[internals/output-capture-and-detachment|output-capture-and-detachment]].
