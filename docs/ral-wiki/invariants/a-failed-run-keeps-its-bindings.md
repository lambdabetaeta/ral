# A failed run keeps its bindings

**A top-level run installs each binding as it lands. When the run then fails,
nothing is undone: a `let` that completed before the failing statement stays
bound in the session, and so do the effects everything before it caused. There
is no rollback, and none is wanted.**

The law is normative in `docs/SPEC.md` §5.6, with its worked example: after a
failed run of `let before = 1` / `failing-command` / `let after = 2`, `before`
remains bound and `after` does not exist.

**The mechanism.** `run_phrases` (`core/src/evaluator.rs`) runs each top-level
phrase in order in one loop. Under `Mode::Session`, `run_phrase_define` writes
the landed binding through `shell.note_define` and back into `shell.env`
*before* the next phrase runs; a failure breaks the loop and touches neither.
`Ran::env` is therefore `env` as extended by the phrases that ran before the
halt, never rolled back.

**Every ending, not just the tidy ones.** A raise and a wall are two
classifications of the same `Err(Break::Error(_))` return: both leave `dispatch`
inside `RunReport::Ran` and so return through `Shell::enter`'s `Ok` arm, which
never touches the checkpoint. `mid_script_wall_keeps_the_bindings_that_landed`
(`core/src/run.rs`) pins this for the wall specifically, with the cancel landing
*mid-script* rather than before evaluation. Exarch pins the raise from its own
side in `tool_call_partial_effects_persist_on_error`
(`exarch/src/shell_eval.rs`), and `core/tests/top_level_vs_block.rs` is the
contract's home.

**The one rollback in the tree is not an exception to this.** `Shell::enter`
checkpoints `(env, context)` at entry and restores it in exactly one arm — the
`Err(payload)` arm of `catch_unwind`, which reports `run panicked: …`
(`panicking_run_reports_failed_and_rolls_back`). That is host-panic recovery,
reachable from no ral-level failure. A raise, a wall, an `exit`, a non-zero
command exit: none of them reach it.

**Transactionality is rejected, permanently.** A ral script's statements cause
real, external, irreversible effects — it spawns subprocesses, redirects into
files, and `detach`es workers that outlive the run — and there is no journal of
any of that anywhere in `core/src`. A transaction that reverted the `let`s
while the file the failed script wrote stayed on disk would be *worse* than the
honest semantics: the name vanishes and its effect persists, so the model can no
longer name what it caused. Partial rollback of a partially-reversible run is
not a weaker guarantee than none; it is a misleading one. It would also
introduce a rollback concept into the *language*, a notion ral has deliberately
never had, to serve a case that concept cannot cover.

**Why the prose matters as much as the mechanism.** Believing its bindings were
discarded, an agent recovering from a mid-script failure replays the script from
the top — and the earlier statements' effects were real, so replaying re-runs
them. A false claim of rollback therefore converts a recoverable failure into
duplicated side effects. This is why exarch's failure text
(`exarch/src/shell_eval/report.rs`), its system prompt (`exarch/data/ral.md`
§Failure), and [[map/exarch/shell-eval|shell-eval]] all state the truth
explicitly rather than leaving it to be inferred.

This is a hard rule. Do not make a submitted program transactional, do not
widen the panic checkpoint to cover a ral-level failure, and do not write
model-facing text that says a failed call's bindings are gone.
