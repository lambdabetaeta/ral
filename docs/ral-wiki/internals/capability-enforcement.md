---
verified_at_commit: 0dc4a920
verified_at_date: 2026-10-09
anchors: [check_exec, Admitted, admit, Flag, Denial, refused, region, Region, Table, GrantStack, SandboxProjection, Widen, sandboxed_command, build_launch, projection_enforceable]
---

# Capability enforcement: one model, two enforcers

**A grant is data, and two enforcers read it: an in-process guard for what ral
does itself, and an OS sandbox for what a spawned program does on its own.**
Each is the authority exactly where the other is blind
([[design/two-enforcers|two enforcers]]). [[design/grant|The grant design]]
states the lattice; this page says how a check runs. The code lives in
`core/src/capability/`, `core/src/guard/` and `core/src/sandbox/`.

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
  rule.

## One model, read in two places

**The `capability` module decides authority; `guard` and `sandbox` only read
it** ([[decisions/261006_capability-is-data|capability-is-data]]). The
dependency order is `capability < sandbox < guard`. This is a module boundary,
not a typestate ([[decisions/260605_witness-collapse|witness-collapse]]).

**Every in-process yes or no is a `guard::check_*(&Context, …)`.** Each check
in `guard/enforce.rs` is a free function over a borrowed `Context`. It folds
the whole dynamic `GrantStack` (`ctx.grants`) before it answers, so a verdict
reflects every frame on the stack, not just the top one. The exec and fs checks
return a typed `Denial`, and `Shell::refused` (`guard/shell.rs`) records it on
the trail. The doors are:

- `check_exec`, which returns the `Admitted` token every launch needs. Only
  `GrantStack::admit` mints one.
- `Shell::check_fs_read`, a read *by name*: a predicate, a listing, a module
  load.
- `Shell::locate`, the door every open goes through. It judges the object it
  lands on with `check_fs_exact`.
- the boolean flags: one `Flag`, checked by one `Shell::check(Flag, who)`.
- on the sandbox's side, `SandboxProjection::of`, the part of the policy the OS
  can render, folded from the same tables.

## The fold keeps the layers apart

**A verdict combines what each layer says about *this* access. It never
flattens the layers into one frame.** A `Capabilities` is one layer, and the
`GrantStack` is their meet. Each verdict (`capability::exec::rules`,
`capability::fs::region`, `GrantStack::permits`) walks the stack.

There is no `Capabilities::meet`, because an authored `ExecGrant` is not closed
under intersection: a bare key means whatever file the host `PATH` finds at
check time ([[decisions/260906_object-not-name|object-not-name]]). So the fold
compiles first, then meets:

- `rules` compiles every layer into an `ExecRules` table for each question, and
  meets the tables.
- `region` re-freezes every layer's prefixes for one operation into a `Region`,
  and meets those.

Both are a `Table`, whose meet is the pointwise law `⟦a ∧ b⟧ = ⟦a⟧ ∧ ⟦b⟧`
([[design/authority-tables|authority-tables]]).
[[related/access-control-algebra|access-control-algebra]] reads restrict-as-meet
and extend-base-as-widen, both deny-overriding, against the literature.

**One place means a union, and it has its own rule.** `Widen` serves exarch's
`--extend-base`, which widens a single base layer. There, silence lifts no
veto.

Three rules govern how layers compose:

- A dimension a layer omits inherits the ambient authority.
- A dimension a layer states can only narrow.
- A deny is anti-monotonic. A later layer can add denies, but never reopens a
  denied region ([[design/scoping|dynamic frames]]).

Exec has three verdicts (`Verdict`: Allow / Only / Deny). The bundled coreutils
and the structured primitives go through the same chokepoint
([[internals/builtins-registry|builtins]]). That closes the bypass, and lets ral
stay a [[invariants/single-binary|single binary]].

## Who enforces what

**The guard covers what ral dispatches. The OS sandbox covers what a spawned
process does on its own.**

- **Exec.** The guard checks every platform. `check_exec` judges the program
  and its argv *before* the spawn and returns `Admitted`, which `build_launch`
  launches from. The kernel then holds the same rules against programs the
  child starts itself (`sh -c`, `find -exec`): Seatbelt on macOS, Landlock on
  Linux. Windows has no kernel exec filter
  ([[internals/exec-judgment|exec-judgment]]).
- **Filesystem.** The guard checks ral's own reads and writes
  (`check_fs_exact`). An OS sandbox confines the child's: Seatbelt on macOS,
  bwrap on Linux, an AppContainer LowBox token on Windows.
- **Network.** There is no guard, because ral performs no network operation
  itself. The OS sandbox is the only enforcer. On Windows it is the withheld
  network capability SIDs: a LowBox token without them cannot open a socket.

**For the filesystem, guard and sandbox cannot disagree, by construction.**
Both read one fold (`capability/fs.rs`). `region` meets each layer's `Region`
for the operation: the allows meet and the `deny_paths` join. A read then joins
`floor()`, the platform's system paths, after the meet, so no `read` narrows the
floor and only a `deny` removes part of it. No backend carries a system set of
its own. So the conservatism invariant needs no differential test. The only
difference is *when* the fold runs:

- the guard folds afresh on every check, and asks `verdict`, then `respelled`
  on a refusal;
- the sandbox folds once at spawn, when it writes the profile, and renders
  `live()` and `denies()` as surface strings.

**The projection never carries an allow beneath a deny (`live()`).** Under
deny-wins such an allow is dead. It must never reach a backend whose primitive
puts explicit allows before inherited denies, as a Windows ACL does.

## Confinement is per command

**The sandbox wraps each external command. It never re-runs the grant body.**
A `grant` is a *local* dynamic effect scope: its body evaluates in process, and
`transport::dispatch` runs it locally. Nested grants compose by intersection on
the `GrantStack`, which is not a process boundary ([[design/grant|grant]]).
Confinement happens one level down: when `build_launch` spawns an admitted
external or bundled child under a restrictive projection, it calls
`sandboxed_command`, which confines that one child
([[internals/confined-launch|confined-launch]]).

Two consequences follow:

- Each command is its own *launch*. Two commands in one grant body run in two
  envelopes, each rendered afresh from the filesystem as it then stands.
- Confinement engages only when a child is actually spawned. A
  `grant [net: false] { … }` that runs no external command does not fail
  closed. An offline request on a backend that cannot enforce it fails closed at
  the spawn (`projection_enforceable`).

[[design/exarch-architecture|exarch]] reuses this boundary unchanged: an agent
run is a host-pushed grant frame over the same stack.

See also [[design/capability-carriers|capability-carriers]] (why the rule, the
live judgment and the `SandboxProjection` are three things, not one); map
[[map/core/capabilities|capabilities]]; `docs/SPEC.md` §12.
