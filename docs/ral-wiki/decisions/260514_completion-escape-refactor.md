---
status: active
---

# Completion/escape refactor: EvalSignal retired

The evaluator's result surface was reworked so that ordinary completion and
non-local escape are represented by **distinct types rather than one tagged
signal**. Three changes:

- `Escape` and `BodyResult` were introduced;
- the evaluator surface was flipped to `Settled<Value>`, with the tail call
  absorbed inside the mobile-state swap;
- `EvalSignal` together with its transitional shims was deleted.

This is the structural basis on which the escape-propagation bugs
([[decisions/260514_escape-propagation-bugs|escape-propagation-bugs]]) stay fixed: with completion and
escape distinguished in the types, a `try` or `grant` boundary can no longer
silently conflate a genuine escape with a recoverable result.

The migration is complete — both phases landed on 2026-05-14.

See also [[design/control-operators|control-operators]], [[map/core|core]].
