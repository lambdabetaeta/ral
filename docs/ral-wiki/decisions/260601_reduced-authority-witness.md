---
status: superseded
---

# A reduced-authority witness makes the capability chokepoint a type

Three changes to the capability layer, all preserving enforcement semantics
exactly. The sandbox suite (`core/tests/sandbox_*.rs`) and `grant_policy.rs`
pass unchanged before and after — that is the correctness guard.

## B5 — the `Reduced` witness

[[internals/capability-enforcement|Capability enforcement]] documented a single
front door, `EffectiveGrant`, but the door was prose. The decision primitives
(`evaluate_exec`, `check_grant_bool`, `check_exec_args_impl`, the fs check) took
a bare `&Context` and walked `context.grants` directly, so any code in the
`capability` module holding a `&Context` could decide authority without folding
the stack. The "single decision front door" was a convention, not a type.

`EffectiveGrant` is now an unreduced borrow whose only operation is `reduce`,
which yields a `Reduced` — the witness that an authority decision is made from
the meet-fold of the *whole* dynamic stack. `Reduced` is constructible only by
`EffectiveGrant::reduce`. Every decision (exec, fs read/write, editor, shell,
and the OS-renderable `SandboxProjection`) is a method on `Reduced`, and the
stack-walk primitives in `check.rs` / `exec.rs` take a `&Reduced`. There is no
longer any function that decides authority from a bare `&Context`; the chokepoint
is a visibility fact.

**Scope chosen: items 1+2, not item 3.** The witness is the required input to the
decision points and the projection producer; the primitives are non-bypassable.
The in-ral exec and fs checks still re-walk the layers from the witness rather
than deciding against the materialised `SandboxProjection`. Fully unifying the
two folds was rejected because the in-ral fold carries three things the
projection — tuned for the OS profile renderer — does not:

- the three-valued exec verdict (`Allow` / `Subcommands` / `Deny`, which the
  projection flattens into `allow_paths`);
- the per-layer short-circuit on the first denying layer;
- the sandbox-vs-host resolver distinction (`Context::resolver_for_check` is
  `LexicalOnly` inside a sandboxed child, `Lenient` outside, and re-resolves each
  prefix per access — the projection canonicalises prefixes once at fold time).

Collapsing them would change resolution behaviour and lose diagnostics, so the
witness *gates* the walk, it does not replace it. The meet remains spelled in the
lexical (`intersect_prefix_strings`), symlink-aware (`GrantPathSet`'s `Meet`), and
per-layer (`path_within`) sites; reducing those to one is left as separate work.

## B6 — not done: the host baseline *is* the spoof guard

`CommandIdentity::policy_names` (`core/src/evaluator/command/identity.rs`)
computes the bare-name spoof guard from the host process `PATH`
(`std::env::var("PATH")`), while `walk_path` resolves `self.resolved` from the
shell's effective `PATH` (`ctx.env_overrides().get_or_host("PATH")`). The guard
drops the bare surface name from the policy-key candidates iff
`host-resolution(name) != self.resolved` — i.e. iff a dynamic `within [shell:
[PATH: …]]` override redirected the name to a *different* binary than the host
PATH would reach. That asymmetry — baseline on the host PATH, `self.resolved` on
the effective PATH — is the guard: it is exactly what detects a scoped PATH
redirect to a spoofed binary, so an outer grant keyed by the bare name does not
silently admit it.

Switching the baseline to the effective PATH (`get_or_host`) makes
`baseline == self.resolved` by construction (both resolve the same name on the
same PATH), so the guard never fires and a `within [shell: [PATH: spoofdir]]`
override would re-admit a spoofed binary under a bare-name grant key. The
characterisation test `policy_names_drop_bare_when_scoped_path_diverges` fails
under that change. The overarching constraint forbids weakening enforcement, so
the change was reverted and left undone; `policy_names` keeps the host baseline.

The genuine inconsistency in this area is elsewhere and out of B6's stated scope:
`resolve_exec_names` (`capability/effective.rs`) resolves
grant exec names against `env_overrides().get("PATH")` — the override only, no
host fallback — so with no override active it cannot pin a bare name into the
OS allow-list, whereas `walk_path` resolves it via the host fallback. That gap
fails closed (the OS sandbox denies the unpinned name) and changing it would
alter the projection, hence the enforcement behaviour B5 must preserve; it is
recorded here as a follow-up, not changed.

## B7 — grant axes are independent (documented, not changed)

The grant axes — exec, fs, net, editor, shell — are independent. An axis a grant
does not mention is unconstrained: `None` is the `Meet` identity (⊤), so an
omitted axis inherits ambient authority. Restricting one axis (e.g. fs) does not
restrict another (e.g. net). In particular `net` has no in-process gate — a
net-deny is enforced *only* by the OS sandbox, which a net or fs restriction
engages. This is by design, not a bug: preserving `None = top` keeps the lattice
clean and the surface symmetric with the toml/inline projections of one
`RawCapabilities`. Recorded in `docs/SPEC.md` §11 and [[design/grant|grant]] so
the mental model is explicit; no enforcement behaviour changed.

Superseded by [[decisions/260605_witness-collapse|witness-collapse]]: the B5
witness is removed because `reduce` folded nothing and the seal guarded an
identity — the check functions inherently fold the whole stack, so the
chokepoint is now a module boundary rather than a typestate. B6 (host-baseline
spoof guard) and B7 (axis independence) stand unchanged.

See also [[map/core/capabilities|capabilities]], [[design/grant|grant]],
[[internals/capability-enforcement|capability-enforcement]].
