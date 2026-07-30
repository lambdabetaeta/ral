---
status: active
---

# Fs grants are path-derived capability SIDs, stamped once ever

**A Windows fs grant is keyed by the *path* it covers, not by the container that
consumes it: each `(canonical path, kind)` with kind ∈ {rw, ro, deny} derives a
deterministic capability SID whose ACE is stamped once, ever, and never
reverted — so a spawn's kernel-enforced reach is the set of capability SIDs its
token carries, and the tree walk is paid once per path rather than once per
projection per session.** Supersedes
[[decisions/260714_ace-free-fs-confinement|ace-free-fs-confinement]] (which left
the tier choice open) and the fs-authority half of
[[decisions/260713_projection-keyed-appcontainer|projection-keyed-appcontainer]];
realised in `core/src/sandbox/windows/{dacl,session,appcontainer}.rs`.

## Context

Keying fs authority to the AppContainer SID makes the *stamp set* a function of
the container, so every new container re-walks the tree:

- `SetNamedSecurityInfoW` with an inheritable (`OI|CI`) ACE on a directory
  propagates that ACE into every existing descendant **before it returns**. NTFS
  access checks consult only each file's own security descriptor, so the walk is
  load-bearing, not an optimisation.
- On a build tree (100k+ files) that is tens of seconds to minutes — paid per new
  projection, per session, and paid **again** at teardown to restore. The
  measurements, the tier survey, and the rejected alternatives are recorded in
  [[decisions/260714_ace-free-fs-confinement|ace-free-fs-confinement]].

The container SID is the wrong key. A path's ACE says what that path admits; a
container's identity says which authority a process holds. Fusing them makes the
first quantity per-container and therefore repeated.

## Decision

Split the two: derive the SID from the path, and let the token select.

- **Name.** `dacl::fs_capability_name` maps a grant to
  `ral.fs.<kind>.<128-bit truncation of SHA-256 of the canonical path>`, and
  `DeriveCapabilitySidsFromName` turns that name into a capability SID.
  Deterministic, so any session on the host computes the same SID for the same
  path — no registry, no shared state, no coordination.
- **Identity is the exact canonical spelling.** The path is hashed as-is, *not*
  case-folded. Canonicalisation already yields one spelling per object, so
  folding buys nothing; and in an NTFS case-sensitive directory it would merge
  two genuinely distinct names into one authority. The hash therefore
  distinguishes what the filesystem distinguishes.
- **Stamp once, never revert.** `dacl::ensure_fs_grant` stamps that SID's ACE
  and leaves it. Persistence is safe because a capability SID is evaluated only
  in the *AppContainer pass* of the Windows access check, whose result
  **intersects** the normal user pass: an ACE that no live token names is inert,
  and can never widen any process's reach beyond the owning user's own ambient
  access. This is the pattern Chromium uses for its LPAC install-directory ACLs.
- **Two witnesses gate a skip.** A grow-only stamp store (`stamps.json` beside
  the ledgers, atomic tmp+rename writes, per-path named-mutex merge) records
  completed propagations; a probe of the root's own DACL confirms the tree was
  not deleted and recreated under the same name. Ordering is
  **apply-then-record**: a crash mid-propagation leaves no witness, so the next
  grant re-stamps (idempotent) and a child that runs in the interim fails
  closed.
- **The token is the projection.** A spawn's kernel-enforced reach is exactly
  the capability SIDs `session::confine` mints into its
  `SECURITY_CAPABILITIES`, alongside the network capabilities as before.
  Attenuation still shrinks-only: a narrowed projection's token simply lacks the
  wider paths' capabilities. A `deny_path` becomes a per-path *deny* capability
  the token opts into, so projection-specific denies now coexist on a shared
  path instead of one projection's deny bleeding onto another's.
- **Profiles stay, stripped of fs authority.** The per-projection AppContainer
  profile remains for what only it provides — the deny-by-default LowBox token
  and named-object namespace separation. `DaclManager` shrinks to the profile
  ledger; teardown no longer restores ACEs, so exit no longer hangs. The boot
  sweep still restores *legacy* pre-capability ledgers' per-session ACEs, since
  a recycled pid could resurrect a profile name those ACEs still reference.

## Consequences

- **Cost model, scoped honestly.** On the **no-drift fast path** — the store
  witnesses the stamp and the root probe agrees — `confine` is one store lookup
  plus one root-DACL probe, milliseconds, for any projection in any session,
  forever; the propagation walk is paid once per `(path, kind)` for the life of
  the host, and teardown is free. That figure covers exactly this path. It does
  *not* include drift repair, which is O(N) reads over the tree and is
  deliberately **not** run per session (see below) — running it every session
  would reinstate the very cost this design removes.
- **Permanent ACL mutation of user files** is now the design, not a bounded
  window. The safety argument above is what buys it: the ACE grants a principal
  no token can name unless ral mints it, inside a pass whose result intersects
  the user's own. What the old bounded-mutation protocol protected against is
  paid for by the intersection semantics instead.
- **Denies are compositional.** Because a deny is its own capability rather than
  a stamp on a shared path, two projections may grant and deny the same path
  concurrently without one clobbering the other.
- **DACL growth on a hot root is bounded by construction:** at most three
  capability ACEs per distinct prefix, ever — one per kind — however many
  projections or sessions consume it.
- **The tier survey stays live as the destination.** This is the shipped
  *interim*: it removes the propagation cost while keeping default-deny reads,
  and forecloses nothing. The ACE-free tiers remain where the design is headed
  ([[decisions/260714_ace-free-fs-confinement|ace-free-fs-confinement]] options A
  — BFS — and B — BaseContainer); what changes is that they are no longer urgent.

## Authority is object-sticky; grants are path-based

**An ACE lives on the NTFS object, and Windows never re-inherits on a
same-volume rename — so stamped authority follows the *object*, while a grant
rule names a *path*.** The two agree only while the tree is still. This is the
load-bearing caveat of the design, accepted explicitly:

- **Fail-closed drift.** A file moved *into* a granted tree keeps its old
  security descriptor and carries no capability ACE, so it stays dark to
  confined children until a restamp. Harmless in kind: a grant that should
  admit, refuses.
- **Fail-open drift.** A file moved *out of* an rw-granted tree carries the
  inherited capability ACE with it, and stays writable by any future token
  holding that tree's capability. The sharp instance: a file moved from
  rw-tree A into ro-tree B is still writable through `cap(A)`, though B grants
  only reads. A hard link across differently-granted prefixes is the same fact
  spelled differently — one object, several names, one descriptor.
- **What changed is duration, not kind.** Both drifts exist under the
  session-scoped design too — the rename semantics are the OS's, not ral's — but
  restore-at-teardown bounded the fail-open one to a single session. Persistence
  extends it indefinitely. That extension is the trade this page accepts.
- **Mitigations weighed, none shipped.** Restamp/collect tooling
  (`ral sandbox restamp`, `ral sandbox gc`) is the likely answer: explicit,
  cheap when idle, and it names the cost at the point the operator pays it. A
  background re-verify pass is O(N) reads and therefore opt-in only — run every
  session it contradicts the millisecond steady state above. USN-journal
  tracking is **rejected**: journal wrap and crash-resume make it a durable
  state machine of its own, more complexity than the drift it closes.

`dacl.rs`'s module header documents this on the code side and points at this
page.

## Open work — validation before this is called settled

The enforcement claims above rest on Windows access-check behaviour that is
asserted, not yet exercised end to end. Until this matrix passes, treat the
design as shipped-but-unvalidated:

- an end-to-end LowBox spawn with privately derived capability SIDs, in all
  three arms — no capability, ro capability, rw capability;
- a deny capability overriding an enclosing allow, observed from inside the
  child;
- attenuation: a wide token and a narrowed token over the same tree;
- a LowBox child spawning a descendant with a capability it does not itself
  hold. Nested AppContainer capabilities are *expected* to be subset-only —
  verify it rather than assume it, since the whole shrinks-only argument leans
  on it;
- reparse points (junctions, symlinks) across granted prefixes;
- `SE_DACL_PROTECTED` descendants: MARTA propagation skips them, so such a
  subtree is a hole — fail-closed, exactly as under the old design, but worth a
  test that says so;
- DACL growth on hot roots (expected ≤3 capability ACEs per distinct prefix);
- token capability-array size limits, on a projection with many prefixes.

## See also

[[map/core/capabilities|capabilities]],
[[internals/capability-enforcement|capability-enforcement]],
[[decisions/260714_ace-free-fs-confinement|ace-free-fs-confinement]],
[[decisions/260713_projection-keyed-appcontainer|projection-keyed-appcontainer]],
[[decisions/260712_session-scoped-appcontainer|session-scoped-appcontainer]],
[[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]],
[[design/grant|grant]], [[design/two-enforcers|two-enforcers]].

Cite: `core/src/sandbox/windows/{dacl,session,appcontainer}.rs`
(`fs_capability_name`, `ensure_fs_grant`, `confine`, `teardown`,
`boot_recover`); `core/src/sandbox/launch.rs`.
