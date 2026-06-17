---
status: superseded
---

# env_overrides / scope overlap

`HOME`, `USER`, and `PATH` are seeded into **both** `env_overrides` and the
`scope` of the shell state. Holding the same keys in two places means a later
mutation of one can drift from the other, leaving the two views inconsistent.

This was deferred from the Shell-state refactor (which renamed `Mobile::env` to
`scope`, `f33997c`, 2026-05-17) as a separate language-semantics call: deciding
the single authoritative home for these keys is a question about what env
overlays *mean* under [[design/scoping|dynamic scoping]], not a mechanical
cleanup. It is recorded here as an open thread, not a resolved decision.

Resolved by [[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]: the
environment lives in `env_overrides` alone and is read as `$env[KEY]`; the
lexical-scope copy is gone, so there is nothing left to drift.

See also [[map/core|core]].
