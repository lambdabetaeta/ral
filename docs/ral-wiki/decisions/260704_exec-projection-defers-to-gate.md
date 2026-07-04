---
status: active
---

# The exec projection defers admission to the gate

**The OS exec projection's *admit* decision is the in-process gate's verdict,
queried once per nameable command — not a second, parallel derivation of it.**
Two enforcers ([[internals/capability-enforcement|capability-enforcement]]) mean
two folds ([[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]):
the check fold `evaluate_exec` threads a subject through the stack to a yes/no,
and the projection fold `reduce_exec` materialises the closed region set macOS
Seatbelt tests subjects against. They are dual, and stay two. But the projection
half had been re-deriving admission on its own, and the derivation was lossy.

## The silo fold lost cross-shape admission

A layer admits a command two ways — a literal key (`git`, `/usr/bin/git`) or a
covering allow-dir (`/usr/bin/`) — so admission inside one layer is *literal OR
dir*. `reduce_exec` composed the stack by intersecting the two shapes in separate
silos: `literals ∩ literals`, `dirs ∩ dirs`. That loses any admission that
*crosses* shapes. With layer A = `{git: Allow}` (literal) and layer B =
`{/usr/bin: Allow}` (dir), asked about `/usr/bin/git`:

- the check fold says **admit** — A via the literal, B via the dir, met;
- the silo projection collapsed both halves to empty → `Restricted { allow_paths:
  [], allow_dirs: [] }` → Seatbelt **denied** a spawn the in-process gate had
  already run.

Fail-closed (the stricter surface won, no escalation), but a real functional
inconsistency: a nested `grant { exec: [git: allow] }` under a session that
granted `/usr/bin` by directory ran in-ral and died under the macOS sandbox. The
structural lattice laws were all tested; this semantic law — *enforcing the
projection agrees with enforcing the stack* — was neither stated nor tested,
which is why it slipped.

## The fix: enumerate the names, ask the gate

`admitted_literal_paths` (`core/src/capability/sandbox.rs`) draws candidates from
the **raw union** of every layer's literal keys — not a meet-folded map — resolves
each to the absolute path the OS names, and keeps it only where `evaluate_exec`
admits its natural invocation (allow-narrow = key + resolved path; deny-broad adds
the resolved basename, so a sibling `git: Deny` still vetoes). The dir dimension
still materialises as intersected subpaths. So the projection admits a path only
where the gate does, *by construction*.

This does not collapse the two folds — 260602's duct-tape objection stands: there
is no shared subject-free intermediate, and `Subcommands` still flattens onto the
coarser path-level lattice (`Allowed(_)` → list the path; the OS layer cannot gate
argv). The materialisation fold simply *invokes* the query fold over the finite
candidate set instead of paraphrasing it. `meet_literal_exec` had no other caller
and is now private.

## Bare-name denies project by basename, not by resolved path

A bare `git: Deny` is basename-scoped in the gate: it vetoes the name *wherever*
it resolves. Projecting it as one PATH-resolved literal left the deny reachable
through a covering allow-dir by an interpreter the gate never sees (`sh -c git`),
and lost entirely when the name resolved somewhere other than its runtime path.
Denied literals now split by shape:

- absolute (`/usr/bin/git: Deny`) → `deny_paths`, a path literal;
- bare (`git: Deny`) → `deny_basenames`, rendered as a `(deny process-exec (regex
  #"/git$"))` final-path-component match — path-agnostic, no PATH dependence,
  exec-scoped only (the gate's basename veto never denies reads).

The three carve dimensions — `deny_paths` / `deny_dirs` / `deny_basenames` — now
mirror the three shapes the exec veto takes. This retires the older framing that
per-name denies inside an admitted dir were solely the in-process gate's job; the
OS layer enforces them too, closing that slice of the interpreter-bypass class.

## What stays approximate, safely

`Capabilities::meet` — the pre-mash that folds several profiles into one session
ceiling before any command exists ([[design/exarch-architecture|exarch]]) — keeps
the silo fold and stays fail-closed. Its exactness would need to resolve a name
against a covering dir, and it has no resolver and no subject; no representation
makes `git ∧ /usr/bin` exact without one. So a name meant to survive a
directory-only ceiling is written as its absolute path there. This is the one
place the surfaces still differ, and only ever in the safe direction.

The safe direction is now a **CI-enforced invariant**: a differential
conservatism test (`grant_policy.rs`) asserts the projection never admits a
resolved path the gate denies, over adversarial two-layer stacks — including a
nested-basename case a single PATH-resolved literal would have missed. A
functional regression test asserts the cross-shape literal reaches `allow_paths`
and the gate agrees.

See also [[internals/capability-enforcement|capability-enforcement]],
[[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]],
[[design/capability-carriers|capability-carriers]],
[[design/two-enforcers|two-enforcers]]; map [[map/core/capabilities|capabilities]].
`docs/SPEC.md` §11.
