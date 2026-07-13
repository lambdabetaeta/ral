---
status: active
---

# Session-scoped AppContainer, imitating MXC Tier 3

**The Windows OS sandbox is one AppContainer profile per shell session, with
filesystem authority expressed as grant/deny ACEs stamped for the session's
container SID — a close, attributed imitation of MXC's Tier-3 processcontainer
backend rather than a vendored dependency.** Recorded in
`dev/docs/260712_windows_port.md` §W1; realised in
`core/src/sandbox/windows/{appcontainer,dacl,session}.rs`.

## Context

The capability lattice promises a full projection: an fs *allow-list* covering
reads as well as writes, and `net: false` that actually closes sockets
([[design/grant|grant]], [[design/two-enforcers|two enforcers]]). The launcher
and entrypoint held that promise fail-closed on Windows until a backend could
express it. The design space as of mid-2026:

- **Restricted / write-restricted token + Low integrity + Job Object** — cheap
  (the Chrome-renderer model), but expresses only write-confinement and
  privilege stripping: no fs allow-list, no read or network denial.
- **Dedicated sandbox user account + ACLs + per-user firewall rules** — what
  Codex ships on Windows. Classic ACL semantics and a real net deny, but it
  needs elevated one-time provisioning (accounts, logon rights, firewall rules
  — machine-global state to create, repair, and remove), accepts broad reads
  by design, and leaves `Everyone`-writable directories writable.
- **AppContainer (LowBox token)** — deny-by-default for filesystem, registry,
  and network (no capability SID ⇒ no socket), needing no elevation: profile
  creation and LowBox spawn are per-user. Fs grants are ACEs for the container
  SID on granted directories; that ACL mutation of user data is the sore
  point and must be reverted reliably.
- **Vendoring MXC** (`github.com/microsoft/mxc`, MIT, surveyed @ `0e7c3dd`) —
  its retail-Windows Tier 3 is exactly the AppContainer design above,
  production-grade and in-process. But upstream is declared provisional with a
  pending engine-crate refactor, publishes nothing to crates.io, and owns its
  own `CreateProcessW` path — a vendored copy would fork ral's single spawn
  boundary.
- **Windows Sandbox / Hyper-V isolation, or WSL2-hosted execution** — wrong
  weight class for per-command spawn, and WSL2 would sandbox Linux commands,
  not Windows ones. Rejected.

## Decision

AppContainer is the only *shipping* primitive that expresses the full
projection — read+write allow-list, net fail-closed — without elevation,
machine-global state, or a Chromium-style broker. MXC's Tier 3 is a
production-grade, MIT-licensed, in-process Rust implementation of precisely
that, so ral **imitates it closely instead of vendoring it**:

- One AppContainer profile per shell session, created lazily at the first
  enforceable projection, deleted at teardown; the container id is
  per-session because concurrent stampers sharing one SID clobber each
  other's grants (a caveat carried over from upstream).
- Grants become inheritable allow-ACEs for the profile SID on the granted
  prefixes; nested `deny_paths` become explicit deny-ACEs, which canonical
  ACL ordering places ahead of any allow. Every stamp is recorded in a
  durably persisted ledger *before* the Win32 apply, under a per-path named
  mutex, with a boot-time orphan sweep repairing what a crashed session left
  behind — the `DaclManager` lifecycle ported closest to upstream, because
  every line of it earned its place.
- Confined children spawn with the LowBox `SECURITY_CAPABILITIES` through the
  existing `CreateProcessW` attribute-list boundary
  ([[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]]) — the
  parent's own spawn is the confinement point, never a re-exec child.
- Network is the capability SIDs: withheld unless the projection says
  `net: true`, so `net_enforced()` holds on Windows.
- Whatever the backend cannot express stays fail-closed, exactly as the
  Seatbelt/bwrap backends do.

Ground rules for the imitation:

- MXC @ `0e7c3dd` is the reference implementation, ported
  function-for-function where it fits ral's boundary, deviating only where
  composition with `process/launch.rs` demands it — and saying so at the
  deviation site (LPAC and the Win32k-syscall-disable mitigation are the two
  standing deviations: ral's children are arbitrary host tools, not known
  packaged binaries).
- Every derived file carries the MIT attribution notice, and every mirrored
  unit a `// after mxc <file>::<fn> (0e7c3dd)` breadcrumb. **The breadcrumbs
  are provenance markers with an ongoing job** — diffing against upstream for
  Tier-1 adoption and bug-fix uptake — deliberate and permanent, never to be
  stripped as archeology.

## Consequences

- **Confinement widens monotonically across a session.** The OS access check
  sees the union of ACEs ever stamped for the session SID, so a child spawned
  by a later command can open any path an earlier command's grant stamped,
  even when its own declared projection is narrower. The same persistence is
  attenuation-safe for denies: an explicit deny-ACE stamped once keeps a
  later command blocked on that path even where its own projection grants it.
  Both directions are accepted: per-command narrowing would require a fresh
  AppContainer SID — a profile create/delete round trip — on every spawn. The
  in-process gate still judges each command against its own stack, so the
  session union is a floor under spawned children, not a widening of what ral
  itself dispatches ([[internals/capability-enforcement|capability-enforcement]]).
- ACL mutation of user data is bounded by the crash-safety protocol: ledger
  before apply, retained-on-failure restore, orphan recovery at boot.
- A forward path is built in: when `CreateProcessInSandbox` ships broadly,
  MXC's Tier-1 shape replaces ACE stamping with OS-native enforcement behind
  the same projection interface.
- Fallback position if ACE stamping proves unacceptable on real user
  directories: restricted-token confinement as an explicitly *reduced* tier —
  with the capability layer surfacing the reduction, so a projection that
  promises read-confinement still refuses.

## See also

[[map/core/capabilities|capabilities]],
[[internals/capability-enforcement|capability-enforcement]],
[[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]],
[[decisions/260617_sandbox-external-children|sandbox-external-children]],
[[design/grant|grant]], [[design/two-enforcers|two-enforcers]].
