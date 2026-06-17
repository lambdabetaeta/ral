---
status: fixed
---

# Escape propagation: try-swallows-exit and grant tail-call bypass

Two sibling bugs in how non-local escapes propagated through scope boundaries,
fixed together and pinned by regression tests in `core/tests/scope_escapes.rs`
(`3f042a4`, 2026-05-14).

**try swallows exit.** `try` caught more than it should: a genuine `exit`
escaping through a `try` body was absorbed as if it were a recoverable failure,
so the process did not terminate. An escape carrying program exit must travel
*through* `try`, not be unified into its error record.

**grant tail-call bypass.** A tail call made from inside a `grant` body could
land outside the grant's dynamic frame, evaluating with authority the grant was
supposed to have removed. Capability frames must stay in force across tail
landings, exactly as [[design/scoping|scoping]] requires.

Both are corrected; the regression tests are the contract. The escape surface
that makes this regular is recorded in
[[decisions/260514_completion-escape-refactor|completion-escape-refactor]].

See also [[design/control-operators|control-operators]], [[design/grant|grant]].
