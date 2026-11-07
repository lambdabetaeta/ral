# The terminal returns before the pumps

**A lone command's lent terminal returns the instant its tenant is dead, before
its pumps are joined.** `CollectState::settle_lone` reclaims the loan first,
then finishes the jail, then joins the pumps.

- The order does not depend on a cause: under a cause the later join only gains
  a kill of the owned group, and an unwind drops the collector, whose `loan`
  restores the terminal as it drops, with no join at all.
- A pump can outlast its tenant for as long as a descendant holds the pipe, and
  the shell must not sit in a background pgroup meanwhile: its next terminal
  read would fail.
- With no anchor, nobody reports a key. The loan hears it as the leader's death
  by a key signal (`TerminalLoan::reclaim(outcome.death())`, read through
  `gesture`) and strikes it on the run's node; the current scope, read once
  when the pumps are done, decides whether it ends the command or a shield masks it.
- A pipeline holds its loan in the collector, where the anchor reports each
  key as `Event::Heard`, and drops it with the collector at the end of `fold`.

See [[internals/cancellation|cancellation]],
[[internals/pipeline-execution|pipeline-execution]] and
[[internals/output-capture-and-detachment|output-capture-and-detachment]].
