---
status: active
---

# Handlers are deep and self-masking

ral's effect handlers are **deep**, not shallow, and they are **self-masking**.
These are two independent axes, and the implementation and docs have at times
mislabelled the shape; this page fixes the intended semantics.

**Deep.** A handler stays installed across the continuation it resumes. When a
handled computation performs an operation, the handler runs and the continuation
it invokes is again wrapped by the same handler — the handler persists for the
whole remaining computation rather than peeling off after one operation (which
would be shallow).

**Self-masking.** While a handler's own clause body runs, that handler's frame
is stripped from the effect context. An operation performed *inside* a handler
clause is therefore not caught by the same handler; it escapes to an outer one.
This prevents a handler from accidentally intercepting its own effects and
looping.

ral has **no `resume`** primitive. Because there is no first-class resumption
operator, the semantically live question is the masking discipline — exactly
which handler is in scope at each point — rather than how a captured
continuation is re-entered.

See [[design/effects-handlers|effects-handlers]] for the underlying theory and `docs/SPEC.md`
§9.6 for the formal rules.
