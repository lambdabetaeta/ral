---
status: active
---

# Status is the outcome

A run's exit status is a pure function of its outcome, computed once and
nowhere stored: `status(Ok _) = 0`; an error's status is its `exit_code()`;
`exit n` is `n`; a stop by signal `s` is `128 + s`. `Shell::last_status`
(`$?`) is deleted, with every write to it, its wire mirror in `WireShell`,
and its slot in the run door's panic checkpoint — nothing is recorded,
nothing folds back, nothing resets on lambda entry.

`design/failure.md`'s "a `Bool` is data, not a verdict" now holds at the
process boundary too: a run that returns exits 0 whatever it returned, so a
script whose last statement is `false` exits 0. A successful command's audit
observation carries status 0 alongside its value. `try` and `?` write
nothing to reach this rule; they already only ever handled failure, not
`Bool`.

Rejected: a clause `status(Ok(Bool false)) = 1`, keeping the old exit codes.
It would have kept the one place where a `Bool` is again a verdict — the
asymmetry this decision exists to remove.

See also [[design/failure|failure]], [[map/core/shell-state|shell-state]],
[[map/core/evaluator|evaluator]].
