---
status: active
---

# Projection-keyed AppContainer SIDs

**The Windows OS sandbox mints one AppContainer SID per *distinct fs
projection* a session confines, so a child's kernel-checked filesystem
authority is exactly the projection its command declared — never the union
of what other commands in the session stamped.** Supersedes
[[decisions/260712_session-scoped-appcontainer|session-scoped-appcontainer]];
realised in `core/src/sandbox/windows/session.rs`.

## Context

Under the session-scoped SID, ACEs accumulated across a session's commands:
a later command could open any path an earlier command's projection granted
(and stayed blocked on any earlier `deny_path`), because the OS access check
saw the union of ACEs ever stamped for the one SID. That defeated exactly
the constructs that promise attenuation — a nested `grant [fs: …] { … }`
running an untrusted tool, and an exarch subagent forked with narrowed
`permissions` (`parent ⊓ base`): their in-process narrowing held, but any
permitted *external* child escaped to the session union at the kernel. On
macOS/Linux the fresh per-command sandbox has no such gap.

## Decision

Key the SID by the projection's *content*, not by session or by frame
instance:

- The key is [[map/core/capabilities|`bind_spec`]] identity — deduplicated,
  sorted read/write/deny prefix lists, so structural equality is projection
  identity. `net` is excluded: network authority rides per-spawn capability
  SIDs, never disk state.
- A new projection mints a profile named `ral.sandbox.s<pid>.p<n>` — unique
  by a monotonic counter, never by content hash: a name collision would
  silently merge two projections' authority under one derived SID, so it
  must be impossible rather than unlikely.
- Commands sharing one frozen projection share one SID and one set of
  stamps. Every turn of an agent session under one policy is the same key,
  so the profile-create and ACE-stamp cost is paid once per *distinct*
  projection, not per spawn — content-keying preserves the amortisation a
  per-frame SID would have forfeited, with identical enforcement (identical
  authority under one SID is not a security distinction).
- Profile registrations are ledgered *before* the OS-level create — the same
  ledger-before-mutation ordering the DACL engine protects for ACEs — and
  the boot sweep deletes a crashed session's profiles alongside restoring
  its ACEs.
- ACEs still revert at session teardown, not at grant-frame exit: a detached
  worker spawned under a SID legitimately outlives its frame with the
  authority it was born with (revoking ACEs would break its opens without
  revoking its token), and a SID with no live child is inert — no other
  token carries it, and only the session's own spawns can mint one.

## Consequences

- Within-session attenuation holds at the kernel: a narrowed `grant` or
  subagent spawns under a SID the wider paths were never granted to, and an
  earlier projection's `deny_path` no longer bleeds into later commands
  whose projections grant that path.
- The per-projection profiles accumulate only until teardown, each carrying
  only its own projection's ACEs; a pathological many-projection session
  costs registry entries and stamps linear in *distinct* projections.
- One residue inside a single SID: the program-image read-only grant is
  memoised per projection, so two different binaries run under the same
  projection each leave an RO stamp on the other's image path — trivial
  surface (read+execute on executables), accepted as before.
- Deviation from MXC Tier 3 (which scopes its DACL guard to one execution):
  ral holds one session ledger and keys authority by projection instead —
  noted here rather than at a breadcrumb site since it is a lifecycle
  choice, not a ported unit.

## See also

[[map/core/capabilities|capabilities]],
[[internals/capability-enforcement|capability-enforcement]],
[[decisions/260712_session-scoped-appcontainer|session-scoped-appcontainer]],
[[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]],
[[design/grant|grant]], [[design/two-enforcers|two-enforcers]].
