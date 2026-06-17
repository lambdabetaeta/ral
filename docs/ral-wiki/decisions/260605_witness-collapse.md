---
status: active
---

# The reduced-authority witness collapses to free functions

[[decisions/260601_reduced-authority-witness|The witness]] made the capability
chokepoint a type: `EffectiveGrant::reduce` yielded a `Reduced`, every decision
hung off it, and so "a verdict is weighed against the whole folded stack" was a
visibility fact rather than a convention. **The witness is removed, because it
sealed an identity, not a fold.** `reduce` was identity on data — both
`EffectiveGrant` and `Reduced` wrapped exactly `&Context` — and the property it
was meant to guard already lives in the check bodies, each of which iterates
`ctx.grants` and meets across every layer. A type that adds a construction step
without adding a guarantee earns its removal.

The decisions are now free `pub(crate) fn`s over `&Context`:

- `admits_head`, the editor/shell bool gates, and the audit-bearing exec/fs
  checks `check_exec_args` and `check_fs_op` in `enforce.rs`;
- `sandbox_projection` and the projection builder in `sandbox.rs`;
- `evaluate_exec` and `canonical_grant_paths` take `&Context`.

What the witness *named* survives unchanged as what the functions *do*: judging
is live and folds the whole stack. The B5 concern that motivated the witness —
no decision should read a single un-intersected frame — is now met by structure
rather than typestate: one module, `capability`, whose `check_*(&Context, …)`
entry points each fold `ctx.grants`, and nothing outside it decides authority.
A module boundary, not a witness.

The scope the witness deliberately left undone is untouched: the in-ral fold and
the OS-renderable `SandboxProjection` stay two folds, not one
([[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]),
because the in-ral fold keeps the three-valued exec verdict, the per-layer
short-circuit, and the host-vs-sandbox resolver distinction
(`Context::resolver_for_check`) the projection does not carry.

See also [[design/capability-carriers|capability-carriers]],
[[internals/capability-enforcement|capability-enforcement]],
[[map/core/capabilities|capabilities]].
