---
status: superseded
---

# Windows grant confinement scales with the working tree, not the projection

> Superseded (2026-07-30) by
> [[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]:
> the propagation cost is removed by rekeying fs grants from the container SID to
> a deterministic per-path capability SID stamped once ever and never reverted —
> a variant of option H that keeps default-deny reads, which the survey below
> judged impossible. The measurements, the tier findings, and the rejected
> options remain the record of the design space, and tiers A (BFS) and B
> (BaseContainer) remain the destination.

**The Windows DACL tier stamps an *inheritable* allow-ACE per fs-projection
prefix, and `SetNamedSecurityInfoW` propagates it synchronously to every
existing descendant — so confining one external command under a grant over
`cwd:` costs O(files under `cwd`), not O(prefixes). On a repository with a build
tree the confinement of a single `git` or `wc` takes minutes, and teardown pays
the same cost again reverting it.** The tree-size dependence is inherent to
giving a default-deny child access to an *existing* tree by ACL; the durable
fixes are the OS-native, ACE-free tiers MXC's detector prefers (BFS,
BaseContainer) — or, where neither exists, an honestly-surfaced write-only
reduction (the shape Codex ships). Not a cleverer DACL path, and not a smaller
grant set. **This page records the exact findings, re-verified against the code
and the live host on 2026-07-14; the choice of tier is deferred.**

## Symptom

Every exarch turn runs each external command under a grant, and the default base
([[design/grant|grant]]; `exarch/data/reasonable.exarch.ral` — `cwd:` appears in
both `fs.read` and `fs.write`, lines 58 and 122) grants `cwd:` read **and**
write — the whole project tree. Measured on the reporting host (Win 11 **25H2**,
build 26200 — now 26200.8655 — repo on NTFS; `target/` = 48,668 files at first
measurement, 48,701 files + 5,584 dirs on 2026-07-14):

| Case | Time |
| --- | --- |
| `git log --oneline -10`, no grant | 0.39 s |
| `git log` under a `cwd:`-scoped grant (confined) | **timed out > 120 s** |
| confined `git` scoped to a 20-file dir | 0.44 s |
| confined `git` scoped to an 8,000-file dir | 1.41 s |
| `icacls` inheritable grant: 20 files / 8,000 files | 0.17 s / 2.42 s |
| `icacls` revert (remove) that ACE on 8,000 files | 1.55 s |
| `glob 'core/**/*.rs'` under the same grant (no external spawn) | 0.14 s |
| plain metadata walk of `target/` (54k entries) | 1.0 s |

Two readings: cost is **linear in descendant count** (and Windows Defender
scanning each handle-open in the ral process inflates the raw `icacls` numbers to
the observed 120 s+); and the in-process fs gate is *not* the bottleneck — the
glob-only case, which exercises `check_fs_op` per match but spawns no confined
child, is fast. The blow-up is entirely in confining an **external child**. The
last row matters for the guardrail option below: merely *counting* the tree is
cheap.

The teardown side is symmetric but **clean-shutdown-specific**:
`session::teardown` → `DaclManager::restore` rebuilds each DACL without the
inheritable ACE, which re-propagates the removal across all ~48k descendants —
the "exarch hangs on shutdown after a short session" report. On an *unclean*
exit `teardown` never runs (the process-global's `Drop` is not guaranteed;
`session.rs:50-56`) and the same cost is instead paid by the next session's
`boot_recover` orphan sweep.

## Root cause

`core/src/sandbox/windows/dacl.rs` stamps each granted prefix with an
inheritable ACE: `apply_one` decides `inheritable` from `is_dir()`
(`dacl.rs:533-538`), and `apply_explicit_ace` (`dacl.rs:1133`) renders that as
`OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE` and calls `SetNamedSecurityInfoW`
under plain `DACL_SECURITY_INFORMATION` (`dacl.rs:1190`). As the module doc
states (`dacl.rs:39-42`), the engine *relies on* `SetNamedSecurityInfoW`'s
automatic propagation "for both the add and the remove, so there is no manual
descendant walk." That automatic propagation **is** a synchronous recursive
re-inheritance walk of the whole subtree. It is cheap on a small directory and
catastrophic on `cwd:` when `cwd` contains `target/`. There is no size guard,
no subtree exclusion, and no redirection anywhere in the path.

The stamping is memoised per projection — `ProjectionSandbox.granted`, a
`(path, GrantKind)` set keyed by the `{read, write, deny}` prefix triple
(`session.rs:86-119`; [[decisions/260713_projection-keyed-appcontainer|projection-keyed-appcontainer]]) —
so the cost is paid once per distinct projection per session. But "once" is
still a > 120 s wall on the first confined command of a session, plus the same
at (clean) teardown.

## Why upstream MXC does not hit this

ral's DACL tier is a close port of the **AppContainer + DACL tier (T3)** of
MXC's `processcontainer` backend (`processcontainer` names the whole Windows
backend; its tiers are `BaseContainer` / `AppContainerBfs` /
`AppContainerDacl`, `fallback_detector.rs:26-33`). The port's breadcrumbs pin
`github.com/microsoft/mxc @ 0e7c3dd`; the `scratch/mxc` checkout has since moved
to `5c510d5` (~15 commits ahead — Phase 3a/3b backend-selection work and Windows
Sandbox rewrite plumbing), and the claims here are re-verified at that HEAD. The
DACL engine is **identical** — same inheritable ACE, same
`SetNamedSecurityInfoW`, symmetric revert, **no size guard, no exclusion, no
redirection** (its only guard is a 30 s timeout on *acquiring* the per-path
serialization mutex, `PATH_MUTEX_WAIT_MS`, not on the apply). MXC's perf note —
in the `dispatcher.rs` module doc, not the DACL engine — models the cost as
"roughly O(N) Win32 syscalls … per path", N ≈ 6–12, "tens of milliseconds …
hundreds of milliseconds" at larger N: it has no term for descendants because it
assumes small directories. Upstream avoids the pathology structurally, not with
a better DACL path:

- **Its policy builder grants only small, curated directories**
  (`mxc_engine/src/policy.rs`): temp dir and PSReadLine history read-write;
  `PATH`/known-env tool dirs and `%LOCALAPPDATA%\Programs` subdirs read-only.
  Two precision notes. First, the non-inheritable, directory-object-only
  metadata ACE on `C:\` is **not** part of policy composition — it is a
  separate, *elevated* host-prep step (`wxc-host-prep prepare-system-drive`,
  stat-mask `0x0012_0088` for the well-known AppContainer SIDs), which
  `wxc-exec` only recommends via warnings. Second, the builder *does* compose
  one large-tree grant — `<SystemDrive>\` read-only whenever `pwsh.exe` is on
  `PATH` — which under T3 would be an ordinary inheritable stamp of the entire
  system volume, the same pathology at larger scale (unelevated it should fail
  at the root for lack of `WRITE_DAC`; untested upstream). A whole-project
  readwrite grant is not something MXC composes.
- **T3 is MXC's *effective* production tier today** — the earlier draft of this
  page had this backwards. The ACE-free tiers exist but are dark in shipping
  builds: T2 is compiled out everywhere and T1 is velocity-key-gated off (see
  below). Upstream survives on the DACL tier *only because* its grants are
  small.
- **The project-tree isolation backend is roadmap, not shipping.** The
  `windows_sandbox` backend (Windows Sandbox VM, `.wsb` `<MappedFolders>`) is an
  in-progress rewrite whose daemon currently maps only its own guest-agent,
  rendezvous, and python dirs — not an arbitrary project tree.

ral ported the tier MXC actually runs — and then points it at `cwd:`. That is
the single divergence that produces the bug.

## What Codex ships on Windows (the other production data point)

Codex CLI (`../codex @ 2b0b37abb7`, crate
`codex-rs/windows-sandbox-rs`) faces the same problem and ships the *opposite*
trade: **no AppContainer, no LowBox, no default-deny reads — a
`CreateRestrictedToken` write-only fence.** Three levels: `Disabled` (default),
`RestrictedToken`, `Elevated`.

- **Unelevated (`RestrictedToken`):** duplicates the caller's token with
  `DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED`; the restricting-SID
  list is `[per-workspace capability SIDs, logon SID, Everyone]`
  (`token.rs:427-483`). `WRITE_RESTRICTED` makes the kernel consult restricting
  SIDs **only for writes** — reads are simply the invoking user's, and the
  backend *refuses to run* any profile without full-disk read
  (`lib.rs:530-539`). Writes are enabled by stamping **one inheritable OI|CI
  allow-ACE per writable root** for a random, persisted capability SID
  (`S-1-5-21-…`, kept in `CODEX_HOME/cap_sid` keyed per canonical cwd) — with a
  skip-if-mask-already-present check and **no teardown**: ACLs are intentionally
  left in place, reconciled by a per-principal state file. So the propagation
  wall is paid **once ever per workspace**, not per session. A time/count-bounded
  audit (2 s, 50k entries) stamps capability-SID *deny-write* ACEs on
  `Everyone`-writable dirs to close the hole that `Everyone`-as-restricting-SID
  opens. Network "deny" at this level is soft: env vars only (proxies to port
  9, `CARGO_NET_OFFLINE`, stub `ssh` shims on `PATH`).
- **Elevated:** one-time UAC setup creates dedicated local users
  (`CodexSandboxOffline`/`CodexSandboxOnline`) with DPAPI-stored passwords, and
  installs per-account-SID **Firewall COM rules plus persistent WFP filters**
  for real kernel net-deny; children run via `CreateProcessWithLogonW` under a
  job with `KILL_ON_JOB_CLOSE`. Deny-*read* ACEs exist only here and only on a
  small named-secrets set (`.env`, `.git`, `~/.ssh`-class paths), inheritable,
  denies-ordered-first, reverted on failure. A junction under
  `~/.codex/.sandbox/cwd/<hash>` dodges ancestor traverse-ACE stamping.

Two lessons transfer. Codex is a shipping instance of options C+E below —
write-only confinement, reads open, once-ever amortised ACLs, permanent host
mutation by design, and explicit refusal messages for everything the backend
cannot enforce. And it **corroborates the fundamental constraint**: even with a
completely different mechanism, Codex could not build unelevated default-deny
reads over an existing tree — it defined the problem away instead.

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
ACLs, a weaker O(1)-per-session confinement tier with the reduction surfaced, or
a different isolation backend.

## Findings: the three MXC tiers and their availability on the reporting host

MXC selects a tier at runtime (`fallback_detector::detect`): **T1 → T2 → T3**.
ral ships only T3.

### Tier 3 — AppContainer + DACL (what ral ships)

The per-descendant ACE stamping above. Universal (non-elevated), correct, and
pathological on large trees. Realised in
`core/src/sandbox/windows/{dacl,appcontainer,session}.rs`; ral's crash-safety
ledger + `boot_recover` sweep is the port of MXC's state files +
`recover_orphaned_state`.

### Tier 2 — AppContainer + BFS (Brokered File System)

A pre-spawn shell-out to the in-box broker `bfscfg.exe` registers an allow-list
of paths against the AppContainer *identity*; an in-box filter driver enforces
the brokered view at runtime. **No per-file ACEs are touched.** One call covers
an entire subtree; cost is O(paths), independent of descendant count; teardown
is a single `--clearpolicy`.

- **Which driver:** evidently `bfs.sys` ("Bfs Filter Driver" — the Brokering
  File System minifilter, an active security-servicing surface:
  CVE-2025-29970, EoP, patched 2025), **not** `bindflt.sys` ("Windows Bind
  Filter Driver"), which is a distinct in-box filter — an earlier draft of this
  page named the wrong driver. MXC's own docs blur the names ("Bind Filter
  (BFS)"). Both drivers are present and RUNNING on the host; the spike should
  pin the enforcer behaviourally.
- **Verbs MXC drives** (`filesystem_bfs.rs`, keyed by `--appid
  <appcontainer-name>`): `--addpolicy --policybroker | --policybrokerreadonly
  --filename <p> --appid <id>` plus `--containerinherit` for every path *except*
  drive roots; `--clearpolicy --appid <id>`. That is the whole set — MXC does
  not drive `--querypolicy`, `--deletepolicy`, `--entrytype`, or `--protected`
  (those, plus `--modifypolicy`, exist in `bfscfg.exe`'s own usage string,
  verified on-host 2026-07-14). Removal is `--clearpolicy`, not
  `--deletepolicy`. `bfscfg` is resolved to an absolute path under
  `GetWindowsDirectoryW()\System32` and driven with a 10 s timeout
  (`BFSCFG_TIMEOUT_MS`).
- **Deny paths still cost DACL:** BFS cannot express a deny inside an allowed
  parent, so MXC's T2 composes a deny-only `DaclManager` for `denied_paths` —
  deny subtrees pay inheritable-ACE propagation over their own descendants.
- **MXC ships it disabled.** The `tier2_bfs` Cargo feature is off in every
  upstream build. The exact caveat (`docs/process-container/os-version-support.md`):
  "invoking `bfscfg.exe` can **deadlock the host on 25H2**. Treat T2 as
  unavailable." CI is blunter: "tier2_bfs must NOT ship on Win 11 25H2
  (bfscfg.exe hangs the host)." A velocity key (`61714527`, "BFS deadlock fix")
  gates upstream's BFS e2e tests — i.e. Microsoft knows, and a staged OS fix
  exists; whether it is enabled on a given servicing level is unknowable from
  outside.
- **The caveat names this host's exact release.** Build 26200 *is* 25H2 (the
  earlier draft implied the caveat might be stale on 26200 — it is not; it is
  addressed to precisely this OS). The open question is narrower: whether the
  staged fix is live on current servicing (26200.8655). This makes the spike
  both more load-bearing and more dangerous — it must run on a disposable VM.
- **ReFS / Dev Drive:** BFS "does not work correctly" there, and MXC aborts
  launch pre-flight (`launch_diagnostics.rs::check_refs_volumes`,
  `refs_volume_unsupported`) — checking non-system drive letters referenced by
  read-only/readwrite paths. Its failure framing is **fail-closed** ("sandboxed
  processes may not be able to access files on those paths"), i.e. grants not
  taking effect — not a silent enforcement bypass as the earlier draft implied.
- **No orphan recovery upstream:** MXC has no persisted BFS state or startup
  reconciliation (its AppContainer cleanup at HEAD is an explicit stub), so
  ral's ledger extension in Phase 2 below is new design, not a port.
- **Availability here (verified 2026-07-14):** `bfscfg.exe` present; `bfs.sys`
  and `bindflt.sys` both RUNNING; repo volume NTFS; `bfscfg.exe` invokable
  unelevated — and the broker *answers* unelevated: `--querypolicy --appid <x>`
  returns "There's no policy for this app" (exit 2) with no elevation prompt.
  Whether `--addpolicy` also works unelevated is untested (MXC neither
  documents nor exercises an elevation contract; the whole path is compiled
  out). Not yet spiked end-to-end (enforcement + timing + no-deadlock).

### Tier 1 — BaseContainer (`Experimental_CreateProcessInSandbox`)

The cleanest surface: a `CreateProcessW`-shaped export from `processmodel.dll`
taking a FlatBuffer `SandboxSpec` (file identifier `"SBOX"`, version `"0.1.0"`;
schema `external/windows-sdk/BaseContainerSpecification.fbs` in MXC, with path
vectors `fs_read_write` / `fs_read_only` / `fs_deny`); the OS establishes the
projection natively — no ACEs stamped by the caller, no broker child, no
elevation, no VM. MXC: `base_container_runner.rs`.

- **T1 is BFS underneath.** Microsoft's own documentation (MS Learn, "Create
  Process In Sandbox APIs", published 2026-06-01) states the `fs_read_write` /
  `fs_read_only` grants are applied "via Bound File System (BFS) policies".
  Consequences: BFS is the substrate Microsoft is committed to (investing in a
  `bind_spec` → BFS mapping is forward-compatible with T1); BFS driver defects
  — including the deadlock — plausibly implicate T1; and T1 presumably inherits
  the ReFS caveat.
- **Doc facts** (same page): two exports, `Experimental_CreateProcessInSandbox`
  and `Experimental_CreateProcessAsUserInSandbox`; "experimental and subject to
  change"; minimum client "Windows 11 (experimental)"; no public header (load
  via `GetProcAddress`); no GA date stated. `identity` **is the AppContainer
  profile name** — it maps directly onto ral's existing per-session profile
  names (`ral.sandbox.s{pid}.p{index}`); same identity shares a profile;
  identities colliding with installed MSIX package family names are rejected;
  a drive-root readwrite grant is deliberately non-recursive (matching MXC's
  `--containerinherit` drive-root exception in T2); AppContainer processes may
  not call it (no nesting). MXC notes the `fs_deny` capability "has not yet
  shipped" (assumed capability bit 1).
- **Availability here (verified 2026-07-14):** both exports **PRESENT**; the
  authoritative query `Experimental_QuerySandboxSupport` (bit 0 =
  `SANDBOX_CAP_CREATE_PROCESS_IN_SANDBOX` per MXC) is **ABSENT**
  (`processmodel.dll` 10.0.26100.8521). MXC's fallback probe — call the create
  API with all-null args — returns BOOL `FALSE` with `GetLastError` =
  `ERROR_CALL_NOT_IMPLEMENTED` (120); MXC treats 120 or `E_NOTIMPL` as
  disabled → **the feature is OFF on this build** (re-confirmed on 26200.8655).
  Upstream gates it behind Windows velocity keys (`61389575`, `61155944`); its
  own compat doc still claims `processmodel.dll` is absent on 24H2/25H2, which
  this host contradicts — the doc trails servicing reality, in the promising
  direction. It may light up on this host later, but nothing can depend on it —
  which is why MXC runtime-probes and falls back. **Keep T1 as an auto-detected
  future tier, not part of the immediate fix.**

## Options

Grouped by horizon. Letters A–F are stable from the first draft; G and H are
new.

### Strategic — bound the damage, hold the line, wait

- **G — descendant-count guardrail + honest refusal (new; recommended
  regardless of tier choice).** Before stamping, walk the projection's prefixes
  and count descendants (~1 s per 50k entries, measured above); over a
  threshold, do not stamp. Either refuse confinement with an actionable
  diagnostic (fail-closed, naming the narrow-grant escape hatch) or — once a
  reduced tier exists — degrade to it with the reduction surfaced. Symmetric on
  teardown by construction (nothing stamped, nothing to revert). *Pro:* ships
  immediately; converts a silent 120 s wall + shutdown hang into an explicit,
  explained choice; independent of every other decision. *Con:* on big trees
  the default grant then yields no confined externals until a better tier
  lands — that is the honest statement of today's reality.
- **Wait, instrumented.** T1 is now publicly documented and rides cumulative
  updates; Microsoft stages a BFS deadlock fix behind a velocity key. Run the
  tier probes each session and log what the host offers, so the moment either
  tier lights up is visible in the field. Waiting *without* G is untenable —
  the wall is present-tense. Note the earlier draft's claim that the capability
  layer "already contemplates" a reduced-authority fallback was a mis-citation:
  [[decisions/260601_reduced-authority-witness|reduced-authority-witness]] is a
  superseded page about a typestate, and today's layer **fails closed**
  (`projection_enforceable` rejects an unenforceable net-deny outright). Any
  degradation path is new surface design.

### Ideal — the destination

- **B — Tier-1 BaseContainer, auto-detected.** One OS API that consumes almost
  exactly ral's `bind_spec` (rw/ro/deny path vectors, capability-named net
  policy, job/UI limits, win32k filtering) with identity = ral's existing
  profile name. The probe is trivial (GetProcAddress + null-args →
  `GetLastError`); the launch path replaces `CreateProcessW` and needs
  `flatbuffers` (or vendored generated code). Unusable on this host today —
  gated off — so B is the tier the detector *prefers*, not the fix. Wiring the
  detector early is nearly free and makes adoption automatic when the OS
  flips the key.
- **A — Tier-2 BFS, spike-gated.** The only ACE-free tier that exists on this
  host *now*: O(paths) apply, O(1) clear, no ACL mutation of user files,
  composes with the existing LowBox spawn (the token half of `confine` is
  unchanged; only fs enforcement swaps). Being BFS, it is the same substrate T1
  consumes — the plumbing survives a later move to B. *Con:* adopts the exact
  tier MXC disabled; the deadlock caveat names this host's own OS release, so
  the de-risk spike is load-bearing **and must not run on a development
  machine**; ReFS/Dev Drive fails closed; deny paths still cost bounded DACL
  propagation.

### Practical — orderable today

- **G first** (above), in front of any tier.
- **C — restricted-token reduced tier**, now concretised by Codex's shipping
  design. Two shapes with very different costs:
  - **C1 — read-only projections, zero mutation.** A
    `WRITE_RESTRICTED`-restricted token whose restricting SIDs are only
    `{logon, Everyone}` denies writes essentially everywhere that matters (user
    files grant the *user* SID, not Everyone) while leaving reads at the
    user's ambient authority. For a projection with **no write prefixes**, this
    is a real write-fence with **no ACL stamping at all** — O(1), unelevated,
    reversible. What it does not give: read confinement, or OS-level net-deny
    (Codex's unelevated answer is env-var soft-block only).
  - **H — write grants, Codex-shape (new; subsumes E).** Add a persisted
    per-projection capability SID to the restricting list and stamp **one
    inheritable ACE per write root** for it, skip-if-present, **no per-session
    teardown** — the propagation wall is paid once ever per workspace, then
    amortised to zero. Real write-fence; reads open; net-deny soft. *Con:*
    permanent ACL mutation of user files — exactly what the DACL engine's
    bounded-mutation guarantee exists to prevent — and the once-ever first
    stamp is still the >120 s wall on this repo. This is Codex's production
    trade, recorded here as the honest endpoint of the C/E line, not a
    recommendation.
  Either shape runs external children under *reduced* authority (no fs-read
  confinement, no kernel net-deny) — the projection must surface that as a new,
  explicit reduction surface, in the spirit of the fail-closed precedent.
- **D — Prune large subtrees from the grant (whack-a-mole).** Grant `cwd`'s
  children minus build/VCS dirs. **Assessed and rejected** by the maintainer:
  does not generalise, leaves mid-session dirs uncovered, still mutates ACLs.
  Recorded for completeness.
- **E — Persist / reuse the AppContainer SID + ACEs across sessions.** Folded
  into H: Codex demonstrates the workable version (persisted SIDs,
  skip-if-present, reconciliation state file instead of teardown). Kept as a
  separate letter only to record why the *LowBox* variant stays rejected: a
  LowBox child needs read ACEs too, so the ACL pollution covers every file, and
  the first run still pays the full wall. Note the tempting micro-variant —
  "skip teardown to fix the shutdown hang" — does not save anything alone: the
  ledger survives and `boot_recover` pays the same removal at the next session.
- **F — Different backend (Windows Sandbox VM / ProjFS overlay).** Correction:
  this is MXC's *roadmap*, not its shipping answer — the `windows_sandbox`
  backend is an in-progress rewrite that does not yet map arbitrary project
  trees. Wrong weight class for per-command spawn; large surface. Unchanged.

## Decision

**OPEN.** No tier chosen yet, but the ordering has sharpened:

1. **G ships first, unconditionally** — it is symptom relief and honesty,
   orthogonal to every tier.
2. **A remains the leading candidate**, gated on the Phase-0 spike — which the
   25H2 finding upgrades from "confirm a possibly-stale caveat" to "determine
   whether the staged OS fix for a documented host-hang on exactly this release
   is live"; it runs on a disposable VM.
3. **B's detector is wired early** (cheap), so T1 adoption is automatic when
   the OS enables it; T1-is-BFS-underneath means A's plumbing is not throwaway.
4. **C1 (and only reluctantly H) is the degraded tier** where no ACE-free tier
   exists and the tree is large — pending a decision on how much reduction the
   projection surface can honestly advertise, which is new design work.

## Implementation plan (for candidate A, when adopted)

Follow the existing MXC-port conventions: MIT copyright header + per-unit
`// after mxc <file>::<fn> (0e7c3dd)` breadcrumbs (as in `dacl.rs` /
`appcontainer.rs`); note the scratch checkout has advanced to `5c510d5` — new
ports should cite the commit they are actually taken from.

**Phase G — guardrail (first, independent of the spike).** Pre-scan descendant
counts for the projection's stamp set with a hard threshold; over it, skip
stamping and fail closed with a diagnostic that names the narrow-grant escape
hatch (later: degrade to C1 where the projection permits). Tests for both sides
of the threshold.

**Phase 0 — de-risk spike (throwaway; gates A vs C).** **On a disposable 25H2
VM — not the dev host** (upstream: "bfscfg.exe hangs the host" on this exact
release): create an AppContainer profile (reuse
`appcontainer::AppContainerProfile`); `bfscfg --addpolicy --policybroker
--filename <cwd> --appid <profile> --containerinherit` and time it over a
48k-file tree; spawn a LowBox child via the existing
`Launch::security_capabilities` boundary and assert it can read a granted path
and is denied outside it; identify the enforcing driver (`bfs.sys` vs
`bindflt.sys`) behaviourally or via `fltmc instances` (elevated); confirm
`--addpolicy` works unelevated (only `--querypolicy` is verified unelevated so
far) and that `--clearpolicy` is O(1); watch for the deadlock. Outcome decides
A vs C.

**Phase 1 — tier detector.** Port `fallback_detector::detect` →
`core/src/sandbox/windows/tier.rs`: probe T1 (`Experimental_QuerySandboxSupport`
bit 0 when exported; else the null-args call reading `GetLastError` ∈
{`ERROR_CALL_NOT_IMPLEMENTED`, `E_NOTIMPL`} as disabled), then T2 (`bfscfg.exe`
under `GetWindowsDirectoryW()\System32` + all projection paths on NTFS), else
T3. Compute once per session; cache on the `SessionSandbox`; log the result.
Refuse T2 on ReFS (mirror `check_refs_volumes`, which checks non-system drive
letters referenced by read/write paths).

**Phase 2 — BFS backend.** Add `core/src/sandbox/windows/bfs.rs` (after
`filesystem_bfs.rs`): `configure(projection, appid)` iterating
`bind_spec().{write_prefixes → --policybroker, read_prefixes →
--policybrokerreadonly}` with `--containerinherit` except at drive roots, and
`clear(appid)` via `--clearpolicy`. Deny paths keep DACL deny-ACEs (mirror
MXC's deny-only `DaclManager` composition; bounded by the deny subtree's size).
Drive `bfscfg.exe` by absolute path with a 10 s timeout (mirror
`BFSCFG_TIMEOUT_MS`); on timeout or failure, fall back to the DACL tier for
that projection and surface a diagnostic. Extend ral's ledger with registered
appids so `boot_recover` can `--clearpolicy` orphans — this is new design, not
a port: upstream has no BFS recovery (its cleanup is a stub at HEAD).

**Phase 3 — wire into `confine` + teardown.** In
`core/src/sandbox/windows/session.rs::confine` (the sole Windows confinement
locus, reached via `sandbox/launch.rs::windows_sandboxed_command`), branch on
the selected tier: the BFS path configures the broker and still calls
`launch.security_capabilities(profile_sid, caps)` (the LowBox token is
unchanged; only the fs-enforcement half swaps). `session::teardown` clears BFS
policy per profile (O(1)) instead of `dacl.restore()` when BFS was used. Net
stays the capability-SID array either way. ral's per-session profile names
(`ral.sandbox.s{pid}.p{index}`) already avoid MXC's known concurrent-runs
hazard (same container id ⇒ one run's revoke wipes the other's live grants).

**Phase 4 — tests + CI.** Close the standing coverage gap: there is currently
**no end-to-end confined-child spawn test on Windows** — the Windows suites
exercise DACL/SID/profile primitives only, while macOS has real
confined-spawn denial tests (`sandbox/launch.rs`, `core/tests/denied_fs_children.rs`,
`core/tests/sandbox_fail_closed.rs`). Add, under `#[cfg(windows)]` on
`windows-latest` CI (`.github/workflows/windows.yml` already builds+tests
ral-core/ral/exarch): a BFS configure/enforce/clear round trip, a tier-detector
probe test, Phase-G threshold tests, and a first real confined-spawn
enforcement test (positive control + denial). Keep them non-elevated (GitHub
runners) and NTFS-only, and note the runner's OS release decides which tiers
are exercisable.

**Cargo.** BFS needs no new crate (child-process broker via existing plumbing).
T1 (option B, later) would add `flatbuffers` — or vendor the generated
`SandboxSpec` code — and likely the `windows` crate alongside `windows-sys`
(ral's own crates are `windows-sys`-only today; `windows`/`flatbuffers` appear
in the repo only inside the non-workspace `scratch/mxc` checkout).

## Consequences

- On hosts where the spike passes, the common case (a grant over a
  build-artifact-heavy repo) becomes O(paths) on both apply and teardown; the
  DACL tier survives as the fallback with its cost bounded by Phase G rather
  than by hope.
- ral takes a dependency on an in-box broker (`bfscfg.exe`) and the BFS
  minifilter (`bfs.sys` — an actively security-serviced surface, cf.
  CVE-2025-29970) whose behaviour per Windows servicing level must be probed;
  and it inherits MXC's ReFS restriction and, until the spike settles it, the
  25H2 deadlock caveat — which names this host's own release.
- **Dev Drive is a trajectory risk for BFS:** Microsoft steers dev repos toward
  ReFS Dev Drives, where BFS fails closed — so A's coverage may shrink over
  time, keeping the reduced tier (C1/H) load-bearing rather than vestigial.
- A documented reduction surface is new design the capability layer does not
  yet have (today it only fails closed); it is required before C1/H can exist,
  and is worth designing once for all platforms.

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

Cite: `core/src/sandbox/windows/{dacl,appcontainer,session}.rs` (`apply_one`,
`apply_explicit_ace`, `restore_one`/`replace_explicit_aces_for_sid`,
`recover_orphaned_state`, `ProjectionSandbox`, `confine`, `teardown`,
`boot_recover`); `core/src/sandbox/launch.rs`; `core/src/process/launch.rs`;
`exarch/data/reasonable.exarch.ral:58,122`;
MXC `@ 5c510d5` (port pinned `0e7c3dd`) —
`src/core/wxc_common/src/filesystem_dacl.rs`,
`src/backends/appcontainer/common/src/{filesystem_bfs,base_container_runner,fallback_detector,dispatcher,launch_diagnostics}.rs`,
`src/core/mxc_engine/src/policy.rs`, `src/host/wxc_host_prep/src/system_drive/`,
`docs/process-container/os-version-support.md`,
`external/windows-sdk/BaseContainerSpecification.fbs`;
Codex `@ 2b0b37abb7` — `codex-rs/windows-sandbox-rs/src/{token,acl,cap,audit,
deny_read_acl,spawn_prep,lib}.rs`, `bin/{setup_main,command_runner}/win*`;
MS Learn "Create Process In Sandbox APIs"
(`learn.microsoft.com/en-us/windows/win32/secauthz/createprocessinsandbox`,
2026-06-01); CVE-2025-29970 (BFS EoP).
