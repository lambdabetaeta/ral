---
generated_at_commit: 1da6d2f91f3bffd25a41d4a75c0a662055955dd1
verified_at_commit: 1da6d2f91f3bffd25a41d4a75c0a662055955dd1
verified_at_date: 2026-11-04
anchors: [check_exec, check_exec_name, failure_trace, Admitted, admit, admit_name, Flag, ExecDenial, Refused, Refusal, Denial, Withheld, refused, region, Table, Evidence, GrantStack, SandboxProjection, Fragment, decode_capability_map, Shell::freeze_ctx, sandboxed_command, build_launch, GuestJail::plan, Launch::apply_guest_jail, install_pre_exec, net_veto]
---

# Capability enforcement: one model, two enforcers

**A grant is data, and two enforcers read it: an in-process guard for what ral
does itself, and an OS sandbox for what a spawned program does on its own.**
Host projections and the [[#Guest jail|guest jail]] enforce different boundaries.
Each is the authority exactly where the other is blind
([[design/two-enforcers|two enforcers]]). [[design/grant|The grant design]]
states the lattice; this page says how a check runs. The domain lives in
`core/src/authority/`, split into `authority::{policy, grant, guard, sandbox}`;
`core/src/shell/` owns its runtime adapters.

The detail is split by mechanism:

- [[internals/path-judgment|path-judgment]] — how ral judges a filesystem path,
  and the one walk every open goes through;
- [[internals/exec-judgment|exec-judgment]] — how ral judges which program may
  run, and the limit of a name veto;
- [[internals/holding-a-deny|holding-a-deny]] — how a `deny` holds against a
  child that moves, links or creates files, and the one case Linux cannot hold;
- [[internals/linux-envelope|linux-envelope]] — the bwrap envelope: what it
  mounts, and how it copes with hosts that refuse it;
- [[internals/confined-launch|confined-launch]] — how one external command is
  confined on each platform, and how it fails closed;
- [[internals/seatbelt-profile|seatbelt-profile]] — the macOS profile, rule by
  rule, and what no rule closes on the host
  ([[internals/seatbelt-profile#Declared limits|declared limits]]).

## One model, read in two places

**`authority::policy` owns the resolved model and its judgments;
`authority::grant` constructs it; `authority::guard` and `authority::sandbox`
enforce it** ([[decisions/261006_capability-is-data|capability-is-data]]).
`authority::grant` owns substantive freezing, selectors, authored resolution
and host system-read discovery; `shell::grant` decodes runtime values and
adapts spawn grants (`shell/grant/{decode,spawn}.rs`). `Shell::freeze_ctx`
supplies the environment, logical cwd and scratch to `FreezeCtx::at`.
`authority::policy` performs no authored resolution or host discovery. These
are module boundaries, not a typestate
([[decisions/260605_witness-collapse|witness-collapse]]).
The runtime supplies a `Baseline`: resolved system read paths, ral's image for
bundled-tool reads, and boot-pinned identities protected from writes.
`authority.rs` assembles it; policy judgments never query the sandbox for
those facts.

**The guard reads a `GrantStack` and returns domain data, never runtime control
flow.** `authority/guard/enforce.rs`'s free `check_*(&GrantStack, …)` functions
fold every layer, not just the top one. The model's refusals (`ExecDenial`,
`Refused`) become a `Denial` — a `fact::Check` beside a `Refusal`'s message and
hint. `shell/runtime/state/checks.rs` alone adds runtime status and control
flow: `Shell::refused` records the check and raises an `Error`, preserving its
wording. Flag and device refusals cross the same conversion without a check
fact. The doors are:

- `authority::guard::check_exec`, which returns the `Admitted` token every launch needs. Only
  `GrantStack::admit` mints one.
- `Shell::locate` and its siblings (`locate_dir`, `locate_existing`,
  `locate_if_admitted`), the doors every read and open goes through. Each walks
  the name, then `GrantStack::judge` takes the transcript and returns the handle
  vouched for the operation; a probe of a name the walk found nothing at is
  judged on the name, so a denied absent name reads as denied. Heads are judged
  the same way: each candidate's object, absent leaf or failed-walk evidence
  (`failure_trace`) meets the exec table and what running reads before its
  kind or error is disclosed ([[internals/path-judgment|path-judgment]],
  [[internals/exec-judgment|exec-judgment]]).
- the verbs of `editor` and `shell`: one `Flag`, checked by one `Shell::check(Flag, who)`.
- on the sandbox's side, `SandboxProjection::of`, the part of the policy the OS
  can render, folded from the same tables.

## The fold keeps the layers apart

**A judgment combines what each layer says about *this* access. It never
flattens the layers into one frame.** A `GrantLayer` is a layer, and the
`GrantStack` is their meet. Every judgment is evidence, a pair of Booleans
(positive, negative), and `↓` closes it to permit or refuse once, at the
boundary that enforces ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).
The stack is an ordered elaboration of layers, then the pointwise meet: a
layer reads its own prefixes and the run places of the exec meet over itself
and the layers beneath it, so pushing a layer appends a conjunct and changes
none beneath it.

There is no meet of two layers into a layer, because authored exec keys are not
closed under intersection: a bare key means whatever file the decoding env's `PATH` finds
([[decisions/260906_object-not-name|object-not-name]]). So a layer is resolved
where it is decoded, and the fold meets what it holds. A `GrantLayer` keeps its
rules and nothing beside them: every spelling was walked as the layer was made
(`shell::grant::decode_capability_map`, the one front-end), an allowance
holding the position its walk reached (`Extent`: everywhere, or under a
position), an exclusion the objects its walk held (`Excluded`: the name and,
where it was a link, its target), a bare name the file the decoding env's `PATH`
found and the tool this platform bundles. Nothing judges a spelling again:

- `rules` meets every layer's `ExecRules` table for each question. `every` is
  `ExecAllow::Any`.
- `region` folds every layer's tables for one operation into a
  `Table<FsAllow, FsDeny>` and meets those. A read takes each layer's read and
  write allowances (writing reads) and, through `GrantStack::read`, its run
  places, then joins the system reads supplied by `Baseline`.

Both are a `Table`, whose meet is the pointwise law `⟦a ∧ b⟧ = ⟦a⟧ ∧ ⟦b⟧`
([[design/authority-tables|authority-tables]]).
[[related/access-control-algebra|access-control-algebra]] reads the design
against Bruns and Huth's PBel.

**Extension happens once, before the first push.** `GrantLayer::extend` takes
a `Fragment`, the loader's result for exarch's `--extend` file, in which an
omitted axis states nothing; in a policy an omitted axis is `t`. A `Fragment`
can never be pushed, so extension cannot follow restriction.

Three rules govern how layers compose:

- A dimension a layer omits restricts nothing.
- A dimension a layer states can only narrow.
- An exclusion beats every allowance. A later layer can add exclusions, but
  never reopens an excluded region ([[design/scoping|dynamic frames]]).

**Activation is denotational.** A layer restricts exactly where some axis does
not denote `t` (`is_top`): `every` with no exclusion, or an fs that writes the
root and excludes nothing, activates nothing. Launch hardening, the projection
and exarch's credential layer all read `GrantStack::is_restrictive`.

Exec is judged over a target and never its arguments, in two parts: the table
(`ExecDenial::Run`) and what running reads (`ExecDenial::Read`). The bundled
coreutils are admitted as command heads, then re-executed in children; structured
primitives perform ral-owned effects through the evaluator's guard doors
([[internals/builtins-registry|builtins]]). Bundling preserves a
[[invariants/single-binary|single binary]], not execution in the evaluator.

## Who enforces what

**The guard covers what ral dispatches. Host sandbox projections cover what a
spawned process does on its own, within the platform's declared limits.**
The [[#Guest jail|guest jail]] is a separate launch branch.

- **Exec.** The guard checks every platform. `authority::guard::check_exec`
  judges the program *before* the spawn and returns `Admitted`. `build_launch`
  takes the caller's argv separately. The kernel then holds the same rules
  against programs the child starts itself (`sh -c`, `find -exec`): Seatbelt on
  macOS, Landlock on Linux hosts. Windows has no kernel exec filter
  ([[internals/exec-judgment|exec-judgment]]).
- **Filesystem.** The guard checks ral's own reads and writes
  (`GrantStack::refusal_fs`, through `Shell::locate`). An OS sandbox confines the child's: Seatbelt on macOS,
  bwrap on Linux hosts, an AppContainer LowBox token on Windows.
- **Network.** There is no guard, because ral performs no network operation
  itself. The OS sandbox is the only enforcer. On Windows it is the withheld
  network capability SIDs: a LowBox token without them cannot open a socket.
- **GPU.** No guard, as for the network: the OS sandbox is the only enforcer.
  Seatbelt on macOS, by absent IOKit, Metal and cache rules; on Linux hosts what the
  bwrap mounts show, since the Landlock ruleset handles only `Execute` and
  `Refer` and so governs no device node
  ([[internals/linux-envelope|linux-envelope]]). Windows cannot withhold it: an
  AppContainer reaches Direct3D regardless, so a withheld GPU projects as left
  on there (`WITHHOLDS_GPU`) and the grant exarch shows its agent says so.

**For host filesystem projections, guard and sandbox read one fold and judge
it on different evidence.** Both read one fold (`authority/policy/fs.rs`). `region` meets each layer's table
for the operation: the allows meet and the exclusions join. A read then joins
the system reads in `Baseline`, the platform's system paths, after the meet:
only a `deny` removes them. They are host state, resolved once by
`authority::grant` where the `Runtime` is built (a host whose system read is
malformed or cannot be walked refuses to boot, naming the variable that chose
it, with the `PolicyError`'s hint) and passed to every reader; no stack holds
them and no
wire carries them. No backend carries a system set of
its own. They differ in *when* the fold runs, and in what an exclusion is
matched against: ral's own readers match it by position or by the object it was
fixed to; the kernel renderers by position alone
([[design/two-enforcers|two-enforcers]]).

- the guard folds the stack's resolved layers on every check, and asks
  `withheld`, which cites an exclusion that applies or a missing allowance;
- the sandbox folds once at spawn, when it writes the profile, and renders
  the read region (`read: Vec<FsAllow>`, run places and live support among
  them), the write prefixes and the exclusions. Every allowance renders as its
  resolved real position and as nothing else; an exclusion renders from the
  resolved real paths its walk held, never from the text as written. A backend refuses a launch whose frozen allowance has
  been redirected rather than follow it.

**The projection never carries an allow beneath an exclusion (`live()`).** Such
an allow is dead. It must never reach a backend whose primitive puts explicit
allows before inherited denies, as a Windows ACL does. A launch whose fs region
holds the universal prefix (`every` path) is refused on Windows before anything
is stamped.

## Confinement is per command

**Confinement is applied to each external command, never by re-running the grant body.**
A `grant` is a *local* dynamic effect scope: its body evaluates in process, and
`transport::dispatch` runs it locally. Nested grants compose by intersection on
the `GrantStack`, which is not a process boundary ([[design/grant|grant]]).
Confinement happens one level down: when `build_launch` spawns an admitted
external or bundled child on a host under a restrictive projection, it calls
`sandboxed_command`, which confines that one child
([[internals/confined-launch|confined-launch]]).

Two consequences follow:

- Each command is its own *launch*. On a host, two commands under an active
  projection run in two envelopes, each rendered afresh from the filesystem as
  it then stands.
- Confinement engages only when a child is actually spawned. A
  ``grant [net: `offline] { … }`` that runs no external command does not fail
  closed.

## Guest jail

**A Linux guest engine checks ral-owned effects and direct command admission,
but does not project per-grant fs or exec rules into its children.**
`build_launch` selects the process-global `GuestJail` instead of
`SandboxProjection::of` and `sandboxed_command`, for host-file and bundled-tool
heads alike.

- The admitted utility's own reads, writes and internal execs, as well as its
  descendants', are not checked by the evaluator. A narrowed `fs` or `exec`
  grant supplies no additional kernel boundary for those operations.
- Guest-visible mounts and topology, read-only mounts and Unix ownership/modes
  remain the filesystem perimeter, not arbitrary access to the host tree.
- `GuestJail::plan` assigns a fresh uid/gid and resource-limited cgroup
  membership: kept commands own transient `engine-<pid>/exec-<seq>` leaves;
  all survivors share `detached` and one command's allowance. Only kept
  launches acquire a teardown guard, so failed preparation, failed exec and
  abandoned launches never kill shared survivors. Engine teardown spares the
  shared leaf; machine halt reaches it.
  `Launch::apply_guest_jail` prepares the envelope's seccomp deny-set;
  the pre-exec hook enters the cgroup, isolates an offline network, drops
  supplementary groups and credentials, sets `NO_NEW_PRIVS`, marks descriptors
  above stderr close-on-exec, then installs seccomp. Setup failure refuses launch.
- The stack's `net_veto` does reach the jail: ``net: `offline`` creates a fresh
  network namespace with loopback up. IP egress and outside abstract Unix
  sockets are isolated; local loopback and pathname Unix sockets remain, and a
  reachable pathname socket can relay outside the command. The host
  [[design/egress|NetPolicy destination gate]] still governs guest egress.

See `docs/SPEC.md` §12.11 and [[map/core/io-process|IO & process]] for the jail's
implementation.

[[design/exarch-architecture|exarch]] reuses this boundary unchanged: an agent's
engine holds host-installed session layers on the same stack.

See also [[design/capability-carriers|capability-carriers]] (why the rule, the
live judgment and the `SandboxProjection` are three things, not one); map
[[map/core/capabilities|capabilities]]; `docs/SPEC.md` §12.
