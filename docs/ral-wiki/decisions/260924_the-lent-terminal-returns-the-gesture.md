---
status: active
generated_at_commit: f71d64dd
---

# The lent terminal returns the gesture

**Only a terminal ral lent can report a key pressed on it.** This reverses
5803377b's law, under which a death by SIGINT, SIGQUIT or SIGHUP was that
gesture's cause "whoever sent it". That law minted a cancellation off no
scope, so it was reported but never raised: `attempt { sleep 100 }; echo
next` printed `next` after Ctrl-C. Made sticky as it stood, it would have
let a child defeat its caller's `try` by killing itself.

## Decision

- **Classification.** A death is a cancellation iff it is by one of
  `signals_of(cause)` for a cause in force: its grace signal, its key, the
  final kill. No death mints a cause.
- **The loan.** `ForegroundGuard` becomes `TerminalLoan`, one lending of the
  `TerminalLease` to a group for a run. It alone reads a signal as a key
  (`gesture` is private to it). A lone command's loan hears the key from
  the command's death at `reclaim`; a pipeline's hears it from the anchor's
  raw `Event::Heard`, heard out after the anchor ends. When the terminal
  returns the loan strikes what it heard on the run's frame, so the run is
  cancelled and `try` cannot recover it. A lent SIGQUIT or SIGHUP lands on
  the frame too: Ctrl-\ ends the job and the session lives.
- On Windows `TerminalLoan` is uninhabited: a console is never lent, and a
  `STATUS_CONTROL_C_EXIT` death is the interrupt's only while it is in force.

## Rejected shapes

- **ral keeps its own pgid for lone children**, so ral hears Ctrl-C itself.
  It would make ral the ear of a program that interprets Ctrl-C itself — an
  editor, a REPL, a `less` — and take the key from it.
- **A seat enum** recording where each child sits. Nobody holds that fact;
  each party that needs it holds the fact that answers it (the loan, the
  `Group`, the `PgidPolicy`).

## Consequences

- A child's own `kill -INT $$` is its own failure (`` `signaled 2 ``) unless
  the terminal is lent to it; a lent tenant's `kill -INT $$` is the
  documented way a program hands the key back.
- A command that catches the key and carries on, or exits, has handled it;
  a pipeline hears the key whatever its stages do with it.
- Deferred: a failing verdict still outranks the struck frame in what a
  pipeline reports, and the anchor's own death still reads
  `` `cancelled `terminated ``.

See [[internals/cancellation|cancellation]],
[[internals/pipeline-execution|pipeline-execution]],
[[decisions/260905_one-delivery-path|one-delivery-path]],
[[decisions/260619_terminal-lease|terminal-lease]].
