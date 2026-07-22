---
status: active
---

# Synod is a second product over exarch-as-a-library

**Synod is its own binary and its own crate, depending on exarch the way exarch
depends on [[map/core|ral-core]] — not a fork of exarch, and not a `--profile`
inside it.** An office-work agent for non-programmers and a coding agent for
programmers want the same machinery and different descriptions; the split falls
exactly there. Synod supplies a grant, a prompt, and a session assembly; exarch
supplies everything that runs them.

## The decision

`synod/` declares `exarch = { path = "../exarch" }` and reuses, unchanged:

- the re-exec trampoline `dispatch_pre_main`;
- the provider layer — transport, streaming, retry, caching, usage — and its
  shared `Engine` ([[decisions/260625_shared-transport|shared-transport]]);
- `exarch::agent::Agent` and its turn driver ([[map/exarch/agent|agent]]);
- the card bus and the frontends that draw it ([[map/exarch/cards|cards]]);
- `bootstrap::Scratch`;
- `prompt::render` and `prompt::host_section`.

It owns its grant (one folder → `Capabilities`), its persona and toolbox
(`synod/data/*.md`), its CLI, and its session assembly. It owns no turn loop, no
provider code, and no renderer. → [[map/synod|synod]].

## Why not the alternatives

**A shared `agent-core` crate, with exarch and synod both above it.** This is the
tidy shape, and it is the one to reach for the moment a *third* product appears.
Taken now it would be speculative surgery: the boundary between "agent
machinery" and "exarch's own description" is not yet known from experience, only
guessed, and a factoring pass performed on a guess pins the guess into a crate
graph. Synod is the experiment that produces the evidence — every symbol it
reaches for is a datum about where the line really is. Extracting the crate
afterwards is a mechanical move over a known list; extracting it beforehand is
design by imagination.

**A `--profile synod` flag inside exarch.** One binary, one crate, a branch at
startup. It is cheaper by a crate and worse by everything else. A product is not
a flag: synod has a different audience, a different vocabulary, a different
failure surface, its own release cadence, and — once the workspace lands — its
own security model. Folding it into exarch puts two products' conditionals in one
turn loop, where the pressure is permanent and always toward one more branch; and
it makes exarch's own surface — every flag, every profile, every prompt section —
answerable to a user who does not program. A flag also cannot be *not shipped*:
an exarch release would carry synod's half-built workspace history, and a synod
release would carry exarch's coding persona.

## The cost

**Three symbols in exarch are promoted from private to `pub` to make this work**
— `prompt::render`, `prompt::host_section`, and `Agent::root`. That is the whole
price, and it is small; it is not free.

- These three are now a **reuse boundary under maintenance obligation**, not
  incidental visibility. A refactor that changes `Agent::root`'s launch
  arguments, or the shape `render` expects a section list to have, is a change to
  synod as well, and must land with it.
- `prompt::host_section` is the one that will strain first. It renders the
  environment snapshot *and* the live grant using exarch's own vocabulary — a
  developer's cwd, git state, and capability bullets. Synod borrows it because a
  grant is a grant, but synod's audience should not be reading git state at all.
  The likely future is that synod owns a plain-language host section and this
  promotion is withdrawn, leaving two.
- The obligation is one-directional: exarch must not grow a dependency on synod.
  The library relation is the whole reason the split is cheap, and it survives
  only while it stays acyclic.

## Consequences

- The workspace gains three members (`synod`, `vm-manager`, `ral-daemon`), and
  `vm-manager` is written for *both* agents: exarch's own VM plan
  ([[decisions/260715_vm-workspaces-cross-by-copy|vm-workspaces-cross-by-copy]])
  boots through the same `Hypervisor` trait.
- [[invariants/single-binary|single-binary]] is unoffended: it forbids sibling
  helper executables behind *one* product's back, not a second product. Synod is
  one executable, as ral and exarch each are, and it re-execs itself for the
  sandbox child exactly as they do.
- What v1 actually contains — the work-in-place safety net and desktop shell
  running under `Boundary::None`, and a VM stack built but not yet joined by the
  design's frame protocol — is recorded in `dev/docs/VM/SYNOD-v1.md` and
  summarised on [[map/synod|synod]]. This ADR decides the *shape of the reuse*,
  and takes no position on those.

## See also

[[map/synod|synod]] (where it lives),
[[map/exarch|exarch]] (the library),
[[design/exarch-architecture|exarch-architecture]] (the loop both products run),
[[decisions/260610_host-embedding-api|host-embedding-api]] (the same move one
layer down: hosting a `Shell` deduplicated instead of forked),
[[decisions/260715_vm-workspaces-cross-by-copy|vm-workspaces-cross-by-copy]] (the
workspace position `vm-manager` is seamed toward).
