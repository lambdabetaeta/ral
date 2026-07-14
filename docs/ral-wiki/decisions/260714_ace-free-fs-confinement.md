---
status: open
---

# Windows grant confinement scales with the working tree, not the projection

**The Windows DACL tier stamps an *inheritable* allow-ACE per fs-projection
prefix, and `SetNamedSecurityInfoW` propagates it synchronously to every
existing descendant — so confining one external command under a grant over
`cwd:` costs O(files under `cwd`), not O(prefixes). On a repository with a build
tree the confinement of a single `git` or `wc` takes minutes, and teardown pays
the same cost again reverting it.** The tree-size dependence is inherent to
giving a LowBox AppContainer child access to an *existing* tree by ACL; the
durable fixes are the OS-native, ACE-free tiers MXC prefers (BFS, BaseContainer),
not a cleverer DACL path or a smaller grant set. **This page records the exact
findings; the choice of tier is deferred.**

## Symptom

Every exarch turn runs each external command under a grant, and the default base
([[design/grant|grant]]; `exarch/data/reasonable.exarch.ral`) grants `cwd:`
read **and** write — the whole project tree. Measured on the reporting host
(Win 11 build 26200, repo on NTFS, `target/` = **48,668 files**):

| Case | Time |
| --- | --- |
| `git log --oneline -10`, no grant | 0.39 s |
| `git log` under a `cwd:`-scoped grant (confined) | **timed out > 120 s** |
| confined `git` scoped to a 20-file dir | 0.44 s |
| confined `git` scoped to an 8,000-file dir | 1.41 s |
| `icacls` inheritable grant: 20 files / 8,000 files | 0.17 s / 2.42 s |
| `icacls` revert (remove) that ACE on 8,000 files | 1.55 s |
| `glob 'core/**/*.rs'` under the same grant (no external spawn) | 0.14 s |

Two readings: cost is **linear in descendant count** (and Windows Defender
scanning each handle-open in the ral process inflates the raw `icacls` numbers to
the observed 120 s+); and the in-process fs gate is *not* the bottleneck — the
glob-only case, which exercises `check_fs_op` per match but spawns no confined
child, is fast. The blow-up is entirely in confining an **external child**.

The teardown side is symmetric: `session::teardown` → `DaclManager::restore`
re-applies the DACL without the inheritable ACE, which re-propagates the removal
across all ~48k descendants — the "exarch hangs on shutdown after a short
session" report.

## Root cause

`core/src/sandbox/windows/dacl.rs::apply_explicit_ace` stamps directories with
`OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE` under plain
`DACL_SECURITY_INFORMATION`, and — as its module doc states — *relies on*
`SetNamedSecurityInfoW`'s automatic propagation "for both the add and the remove,
so there is no manual descendant walk." That automatic propagation **is** a
synchronous recursive re-inheritance walk of the whole subtree. It is cheap on a
small directory and catastrophic on `cwd:` when `cwd` contains `target/`.

The stamping is already memoised per projection ([[decisions/260713_projection-keyed-appcontainer|projection-keyed-appcontainer]]),
so the cost is paid once per distinct projection per session — but "once" is
still a > 120 s wall on the first confined command of a session, plus the same at
teardown.

## Why upstream MXC does not hit this

ral's DACL tier is a close port of MXC's Tier-3 `processcontainer` backend
(`github.com/microsoft/mxc @ 0e7c3dd`). The engine is **identical** — same
inheritable ACE, same `SetNamedSecurityInfoW`, symmetric revert, **no size
guard, no exclusion, no redirection**. MXC's own perf note models the cost as
`O(N paths)`, N≈6–12, in *milliseconds*: it has no term for descendants because
it assumes small directories. Upstream avoids the pathology structurally, not
with a better DACL path:

- **Its policy builder never grants a large tree.** MXC stamps only small,
  curated directories (temp dir, PSReadLine history, tool-install dirs, `C:\`
  read-only-metadata via a *non-inheritable, directory-object-only* ACE). A
  whole-project readwrite grant is not something MXC composes.
- **On modern Windows it never reaches Tier 3.** It prefers two ACE-free tiers
  (below) and treats the DACL tier as a downlevel fallback.
- **For real project-tree isolation it uses a different backend** — the Windows
  Sandbox VM with mapped folders — not DACLs on the real tree.

ral ported only the fallback tier and then points it at `cwd:`. That is the
single divergence that produces the bug.

## The fundamental constraint

There is **no cheap DACL trick**. A LowBox AppContainer token is denied by
default; for the child to open an existing file, that file's own DACL must carry
an ACE for the container SID (NTFS access checks are per-object, not
ancestor-walking). Making *existing* descendants accessible therefore requires
propagating the ACE onto each — O(descendants) — however it is expressed
(inheritable ACE, `icacls /T`, a `PROTECTED_DACL` recompute). Excluding a
subtree (`target/`) is **whack-a-mole**: it does not generalise (a large source
tree, `node_modules`, `.git` objects, a monorepo), leaves new top-level
directories uncovered mid-session, and still mutates user ACLs. So the real
options are: an OS-native / brokered enforcement that does not touch per-file
ACLs, a weaker O(1) confinement tier, or a different isolation backend.

## Findings: the three MXC tiers and their availability on the reporting host

MXC selects a tier at runtime (`fallback_detector::detect`): **T1 → T2 → T3**.
ral ships only T3.

### Tier 3 — AppContainer + DACL (what ral ships)
The per-descendant ACE stamping above. Universal (works 23H2+, non-elevated),
correct, and pathological on large trees. Realised in
`core/src/sandbox/windows/{dacl,appcontainer,session}.rs`.

### Tier 2 — AppContainer + BFS (Brokered / Bind File System)
A pre-spawn shell-out to the in-box broker `bfscfg.exe` registers an allow-list
of paths against the AppContainer *identity*; the in-box `bindflt` filter driver
enforces the brokered view at runtime. **No per-file ACEs are touched.** One call
covers an entire subtree; cost is O(paths), independent of descendant count;
teardown is a single `--clearpolicy`.

- Verbs (`bfscfg.exe` usage on host): `--addpolicy --policybroker | --policybrokerreadonly --filename <p> --appid <id> [--containerinherit] [--entrytype file|directory] [--protected]`, `--clearpolicy --appid <id>`, `--querypolicy`, `--deletepolicy --filename <p>`.
- MXC drives it from `src/backends/appcontainer/common/src/filesystem_bfs.rs`, keyed by the AppContainer name (ral already derives its profile SID from a name, so `--appid` is that same name — it composes with ral's existing spawn).
- **MXC ships it disabled** — the `tier2_bfs` Cargo feature is off in every upstream build "because invoking `bfscfg.exe` can deadlock the host on 25H2." It also **silently fails to enforce on ReFS / Dev Drive** (MXC aborts launch when any policy path is on ReFS — `launch_diagnostics.rs::check_refs_volumes`).
- **Availability here:** `C:\Windows\System32\bfscfg.exe` present; `bindflt.sys` present and **RUNNING**; repo volume is **NTFS**; `bfscfg.exe` is invokable **unelevated** (prints usage, exit 87). The upstream deadlock caveat is **unverified on 26200** — it may be stale/specific to MXC's usage, or may still apply. Not yet spiked end-to-end (enforcement + timing + no-deadlock).

### Tier 1 — BaseContainer (`Experimental_CreateProcessInSandbox`)
The cleanest mechanism: a `CreateProcessW` superset exported from
`processmodel.dll` that takes a FlatBuffer `SandboxSpec` (path vectors
`fs_read_write` / `fs_read_only` / `fs_deny`); the OS establishes the projection
natively, no ACEs, no driver, no elevation, in-process (no VM). MXC:
`src/backends/appcontainer/common/src/base_container_runner.rs`.

- **Availability here:** the export `Experimental_CreateProcessInSandbox` is
  **PRESENT**, but the authoritative capability query
  `Experimental_QuerySandboxSupport` is **ABSENT** (the host's `processmodel.dll`
  is 10.0.26100.8521 — 24H2 servicing). MXC's fallback enablement probe
  (call the create API with all-null args, read the error) returns
  `ERROR_CALL_NOT_IMPLEMENTED` (120) → **the feature is OFF on this build**.
- **Timeline:** Microsoft published `CreateProcessInSandbox` on MS Learn ~May
  2026 ("headed for GA"), but it retains the `Experimental_` prefix and is gated
  per Windows release *and* servicing level. No firm public GA date; it rides
  cumulative updates. It may light up on this host later, but nothing can depend
  on it — which is why MXC runtime-probes and falls back. **Keep T1 as an
  auto-detected future tier, not part of the immediate fix.**

## Options (not yet decided)

- **A — Tiered detector, BFS-preferred, spike-gated.** Port MXC's
  `fallback_detector` + a `FileSystemBfsManager`. Prefer BFS on hosts where
  `bfscfg.exe` exists and the projection is on NTFS; fall back to the DACL tier
  otherwise. Gate adoption on a de-risk spike proving BFS on 26200 is
  unelevated, O(1), deadlock-free, and actually enforces. *Pro:* O(1) on modern
  Windows, no ACL mutation of user files (cleaner than T3), composes with the
  existing spawn boundary. *Con:* adopts the exact tier MXC disabled — inherits
  the deadlock-risk question and the ReFS restriction; the spike is load-bearing.
- **B — Tier-1 auto-detect (future).** Add the `QuerySandboxSupport` / invalid-
  args probe and the `Experimental_CreateProcessInSandbox` launch path so hosts
  where the feature turns on get the clean native tier for free. *Pro:* the
  nicest mechanism, no broker. *Con:* unusable on the reporting host today;
  requires the `windows` crate + `flatbuffers` (ral is `windows-sys`-only now)
  and replaces the `CreateProcessW` call rather than augmenting it.
- **C — Restricted-token + Job-Object reduced tier.** An O(1) fallback that
  strips privileges and write-confines without per-file ACLs, surfacing that
  external children run under *reduced* authority (no fs-read-confinement, no
  OS-level net-deny) — the fallback the capability layer already contemplates
  ([[decisions/260601_reduced-authority-witness|reduced-authority-witness]] §B).
  *Pro:* universal, O(1), no new OS dependency. *Con:* a real security reduction
  the projection must honestly advertise.
- **D — Prune large subtrees from the grant (whack-a-mole).** Grant `cwd`'s
  children minus build/VCS dirs. **Assessed and rejected** by the maintainer:
  does not generalise, leaves mid-session dirs uncovered, still mutates ACLs.
  Recorded for completeness.
- **E — Persist / reuse the AppContainer SID + ACEs across sessions.** Amortise
  the stamp to once-ever. *Con:* the first run is still a > 120 s wall, and the
  ACEs become permanent ACL pollution of user files — against the bounded-
  mutation guarantee the DACL engine is built around.
- **F — Different backend (Windows Sandbox VM / ProjFS overlay).** MXC's own
  answer for project-tree isolation. *Con:* wrong weight class for per-command
  spawn; large surface.

## Decision

**OPEN.** No tier chosen yet. Leading candidate is **A** (tiered detector,
BFS-preferred, spike-gated), with **B** as a future add and **C** as the honest
fallback where no ACE-free tier exists. The load-bearing unknown is whether BFS
is safe (no host deadlock) and correct (enforces) on a modern 26200 host; the
implementation plan below opens with the spike that settles it.

## Implementation plan (for candidate A, when adopted)

Follow the existing MXC-port conventions: MIT copyright header + per-unit
`// after mxc <file>::<fn> (0e7c3dd)` breadcrumbs (as in `dacl.rs` /
`appcontainer.rs`).

**Phase 0 — de-risk spike (throwaway; gates the rest).** On the 26200 host:
create an AppContainer profile (reuse `appcontainer::AppContainerProfile`);
`bfscfg --addpolicy --policybroker --filename <cwd> --appid <profile>
--containerinherit --entrytype directory` and time it over the 48k-file tree;
spawn a LowBox child via the existing `Launch::security_capabilities` boundary
and assert it can read a granted path, is denied outside it, and that
`--clearpolicy` is O(1); confirm no elevation and no host deadlock. Outcome
decides A vs C.

**Phase 1 — tier detector.** Port `fallback_detector::detect` →
`core/src/sandbox/windows/tier.rs`: probe T1 (`QuerySandboxSupport` bit 0, else
the invalid-args fallback), then T2 (`bfscfg.exe` resolvable under
`%SystemRoot%\System32` + all projection paths on NTFS), else T3. Compute once
per session; cache on the `SessionSandbox`. Refuse T2 on ReFS (mirror
`check_refs_volumes`).

**Phase 2 — BFS backend.** Add `core/src/sandbox/windows/bfs.rs` (port of
`filesystem_bfs.rs`): `configure(projection, appid)` iterating
`bind_spec().{write_prefixes → --policybroker, read_prefixes → --policybrokerreadonly,
deny_paths}`, and `clear(appid)`. Drive `bfscfg.exe` by **absolute path** with a
10 s timeout (mirror MXC's `BFSCFG_TIMEOUT_MS`); on timeout or failure, fall
back to the DACL tier for that projection and surface a diagnostic.

**Phase 3 — wire into `confine` + teardown.** In
`core/src/sandbox/windows/session.rs::confine` (the sole Windows confinement
locus, reached via `sandbox/launch.rs::windows_sandboxed_command`), branch on the
selected tier: BFS path configures the broker and still calls
`launch.security_capabilities(profile_sid, caps)` (the LowBox token is unchanged;
only the fs-enforcement half swaps). `session::teardown` clears BFS policy per
profile (O(1)) instead of `dacl.restore()` when BFS was used. Net stays the
capability-SID array either way. Extend the ledger/`boot_recover` sweep to
`--clearpolicy` orphaned appids.

**Phase 4 — tests + CI.** Close the standing coverage gap: there is currently
**no end-to-end confined-child spawn test on Windows** (only macOS has them; the
Windows suite exercises DACL/SID/profile primitives, not `confine → CreateProcessW
→ denied child`). Add, under `#[cfg(windows)]` on `windows-latest` CI
(`.github/workflows/windows.yml` already builds+tests ral-core/ral/exarch): a
BFS configure/enforce/clear round trip, a tier-detector probe test, and a
first real confined-spawn enforcement test (positive control + denial). Keep
them non-elevated (GitHub runners are non-admin) and NTFS-only.

**Cargo.** BFS needs no new crate (child-process broker via existing plumbing).
T1 (Phase B, later) would add `flatbuffers` and likely the `windows` crate
alongside `windows-sys`.

## Consequences

- On modern Windows the common case (a grant over a build-artifact-heavy repo)
  becomes O(paths) on both apply and teardown; the DACL tier survives as the
  downlevel fallback with its cost bounded to hosts that have no better tier.
- ral takes a dependency on an in-box broker (`bfscfg.exe`) and driver
  (`bindflt`) whose behaviour on each Windows servicing level must be probed, and
  inherits MXC's ReFS restriction and (until the spike disproves it on 26200) its
  deadlock caveat.
- A new documented reduction path (C) may be needed where neither BFS nor T1 is
  available and a large tree is granted, rather than a multi-minute stall.

## See also

[[design/grant|grant]],
[[design/two-enforcers|two-enforcers]],
[[internals/capability-enforcement|capability-enforcement]],
[[map/core/capabilities|capabilities]],
[[decisions/260713_projection-keyed-appcontainer|projection-keyed-appcontainer]],
[[decisions/260712_session-scoped-appcontainer|session-scoped-appcontainer]],
[[decisions/260617_sandbox-external-children|sandbox-external-children]],
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]],
[[decisions/260530_linux-exec-confinement|linux-exec-confinement]].

Cite: `core/src/sandbox/windows/{dacl,appcontainer,session}.rs`;
`core/src/sandbox/launch.rs`; `core/src/process/launch.rs`;
MXC `@ 0e7c3dd` — `src/core/wxc_common/src/filesystem_dacl.rs`,
`src/backends/appcontainer/common/src/{filesystem_bfs,base_container_runner,fallback_detector,dispatcher}.rs`.
