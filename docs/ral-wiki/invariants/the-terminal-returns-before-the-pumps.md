# The terminal returns before the pumps

**A lent terminal returns the instant its tenants are dead, before any pump is
joined, for a lone command and a pipeline alike.** `CollectState::settle_lone`
reclaims a lone command's loan first, then finishes the jail, then joins the
pumps. A pipeline's `fold` opens with `CollectState::vacate`: it hears out every
key the ended anchor reported, reclaims the loan once no stage is live, and only
then settles each stage, joining its pumps.

- The order does not depend on a cause: under a cause the later join only gains
  a kill of the owned group, and an unwind drops the collector, whose `loan`
  restores the terminal as it drops, with no join at all.
- A pump can outlast its tenant for as long as a descendant holds the pipe, and
  the shell must not sit in a background pgroup meanwhile: its next terminal
  read would fail, and a key pressed then would reach only the stragglers left
  in the tenant's group, never ral, so nothing could interrupt the join. Back
  in the foreground, ral hears the key and strikes a cause, and the join kills
  the group.
- The price is a straggler's: one that reads the terminal now gets `SIGTTIN`,
  and a cause still reaches it through the group kill.
- With no anchor, nobody reports a lone command's key. The loan hears it as the
  leader's death by a key signal (`TerminalLoan::reclaim(outcome.death())`,
  read through `gesture`) and strikes it on the run's node; the current scope,
  read once when the pumps are done, decides whether it ends the command or a
  shield masks it.
- A pipeline's anchor reports each key as `Event::Heard`, struck as it is heard.
  The anchor ends before the fold, so `vacate` strikes the last of them before
  the terminal returns, and the reclaim itself hears nothing
  (`TerminalLoan::reclaim(None)`). A collector left with a stage unobserved
  keeps its loan until it drops, after its forced end's kill.

See [[internals/cancellation|cancellation]],
[[internals/pipeline-execution|pipeline-execution]] and
[[internals/output-capture-and-detachment|output-capture-and-detachment]].
