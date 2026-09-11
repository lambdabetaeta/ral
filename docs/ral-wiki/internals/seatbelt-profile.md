---
verified_at_commit: d0477d84
verified_at_date: 2026-09-11
anchors: [build_profile, emit_fs_restricted, emit_exec_rules, emit_ancestor_metadata, existing_system_paths, system_paths, withheld_doors, rendered_ancestors, pinned_dirs, open_search_dir]
---

# The Seatbelt profile: an object policy in a name language

Seatbelt judges **names**. ral judges **objects**
([[decisions/260906_object-not-name|object-not-name]]). Every line of
`core/src/sandbox/macos.rs` that is not a straight rendering of a grant is the
cost of saying the second in the first:

- **Two spellings per path.** `/tmp` is a firmlink to `/private/tmp`, and the
  kernel presents a lookup under whichever spelling the process used, so every
  rendered name (`render_paths`) carries both — denies included.
- **Ancestor chains.** `(allow file-read* (subpath P))` does not make `P`
  reachable: Seatbelt gates each directory lookup on the way, so every proper
  ancestor of every granted name gets `(allow file-read-metadata (literal …))`
  (`emit_ancestor_metadata`, over `rendered_ancestors`). Metadata suffices
  because *search* is all a resolver holds on an ancestor — the kernel's,
  `posix_spawn`'s, and ral's own walk, which opens its intermediates `O_SEARCH`
  for exactly this reason (`path/walk.rs::open_search_dir`). A read handle
  there would need `file-read-data`, and a readable ancestor is a listable
  one: `/Users`, a home directory's dotfile names.
- **Pins.** A deny names a path, so renaming its parent would carry the bytes
  to a name the deny does not cover; every proper ancestor of a deny within a
  write prefix is `(deny file-write-unlink (literal …))` — `literal`, never
  `subpath`, so its entries stay mutable
  ([[internals/capability-enforcement|capability-enforcement]]). Linux pays
  nothing here: a mount is anchored to an inode.
- **Order.** Seatbelt is last-match-wins, so every deny is emitted after every
  allow in the profile, not merely after its own.

The lens this gives: when the profile and ral disagree, ask first whether ral
is asking for more than the kernel's resolver would — the walk bug was exactly
that — before making the profile say more.

## What `build_profile` renders, rule by rule

**fs.** `Restricted` renders the host-existing `system_paths` (`Exec` implies
read) and the grant's read and write prefixes as `file-read*` subpaths with
their ancestor chains, the write prefixes as `file-write*` subpaths, and then
the denies. `Unrestricted` passes both through — `(allow file-read*)`,
`(allow file-write*)` — so an exec-only grant can enter the sandbox for exec
gating without clamping the agent's cwd or `HOME`.

`system_paths` is wholesale where cherry-picking breaks tools mysteriously:
`/private/etc` (gitconfig, paths.d, zshenv, nix.conf; nothing user-secret sits
there unprotected — `master.passwd` is 0600, and Seatbelt enforces inode
permissions atop the profile), `/private/var/select` (xcode-select's
`developer_dir` and `sh`; denied, build drivers misreport the EPERM as a broken
install), `/private/var/run` (`resolv.conf`'s target and the mDNSResponder
socket). User temp and workspace paths are absent by design: they arrive
through the grant.

**exec.** `Unrestricted` is `(allow process-exec)`, so an fs-only grant does
not attenuate exec at the OS layer. `Restricted` folds the grant's literals and
directories, the `Exec`-tagged system paths and ral's own binary into one
`(allow file-read* process-exec …)` — Seatbelt needs both operations to spawn
— because the system reads miss user toolchains (`~/.rustup/…/bin`), Apple's
compiler chain (`gcc → cc1 → as → ld`) lives under CommandLineTools and would
die at the first descendant exec when `[exec]` names only `/usr/bin`, and
bundled tools re-exec ral itself (`--ral-bundled-tool`; the per-tool gate is
`vet`). An operand-less form is an unconditional allow, so an empty admit set
emits nothing. This layer is deliberately coarser than the in-process gate —
`(subpath D)` admits every binary under `D` — because its job is the
interpreter-bypass class the gate never sees (`sh -c`, `env`, `xargs`,
`find -exec`): deny what lies *outside* the admitted set. Denies follow, both
operations each (read alone lets the exec through to fail later; exec alone
leaves the binary readable), and a bare-name veto is a final-component regex
over `process-exec` only, vetoing the name wherever it resolves without
denying reads of every same-named file.

**Freezing the admitted set.** Under `fs: Unrestricted` with a veto in force,
every admitted directory is also `(deny file-write*)`. Otherwise
`(allow file-write*)` makes every veto hollow: copy the denied binary under a
fresh name into an admitted directory — or drop anything at all into the
unconditionally admitted `/opt/homebrew/bin` — and run it from there.
Freezing contradicts no layer, none having asked to write there, and restores
the premise `capability::deputy` reasons from: that an unrestricted `fs` is
not "everything writable" — true of the folded grant, false of this backend
until here. A grant that *restricts* fs is never frozen, even where its write
set overlaps the admits, because there the overlap is a stance someone took
(`reasonable` admits `cwd:/` for exactly the scripts it lets the agent write),
and a name veto has never been more than a narrowing of the allow set.

**net.** Enforced by absence. The base admits `network-outbound` for nothing;
only `net: true` appends `macos-net.sbpl` — the wholesale `(allow network*)`,
Seatbelt having no per-address rules, and the Mach doors that *are* the
network: configd (proxies, DNS servers), the dnssd port, trustd. That silence
is what closes DNS: `getaddrinfo` reaches mDNSResponder over the UNIX socket
`/private/var/run/mDNSResponder`, which Seatbelt gates as `network-outbound`,
not `mach-lookup` (measured on Darwin 25.5.0: denying `mach-lookup` outright
still resolves names; admitting that socket alone resolves them with
`mach-lookup` denied). Admitting the socket for any local reason would make a
hostname an egress channel — an attacker-chosen query label leaves through a
daemon outside the sandbox — while `net: false` still read as closed.
`mac_profile_denies_network_when_disabled` asserts this over every rendered
rule, with a `net: true` positive control so it cannot pass vacuously.

## The base, `macos-base.sbpl`

Policy-independent: true of every entry, whatever the grant. Its shape is
adapted from BrianSwift/macOSSandboxBuild's `confined.sb` — deny-default plus
selective allows, the folded `file-read* process-exec` idiom, `process-fork`
beside `process-exec` since Seatbelt gates the two separately — with paths
inlined at build time rather than passed as scalar `(param …)`s, a grant's
admits being variable-length.

- **Mach doors are named, never wholesale.** A Mach name is a door to a daemon
  that acts on your behalf *outside* the profile. Named: `com.apple.dyld`
  (dyld locates the shared cache through launchd; without it the child aborts
  before `main`), the notification center (libnotify, on the first
  `notify_register_*`), opendirectoryd's libinfo and membership (`getpwuid`,
  `~`, `whoami`, `id`), and the loggers (`os_log`; a denial is survivable, but
  every libSystem process hits it and would drown the denial log
  `sandbox::diag` reads). Withheld, each with its reason as *data* in
  `withheld_doors` so the denial hint can quote it — a comment cannot reach the
  agent holding the EPERM, and a deliberate `no` read as an accident is chased
  instead of worked around: `launchservicesd` (and `lsd.*`) would spawn
  whatever `/usr/bin/open` names as an unconfined child of launchd — `open -a
  Terminal ./payload.command` is a full escape, `open https://…` egress under
  `net: false`; the pasteboard is a clipboard no grant mentions;
  `SecurityServer` hands out keychain items (`security
  find-generic-password`). Withholding securityd has one measured cost: cargo's
  bundled libgit2 speaks TLS through SecureTransport, whose handshake reaches
  securityd and, denied, fails as `ssl handshake -9808` and blames a missing
  revision; Apple's `curl` and `git` reach trust through trustd alone. So
  exarch tells cargo to fetch through the `git` binary
  (`bootstrap::CONFINED_TOOL_SETTINGS`) rather than admit a door that hands the
  agent the login keychain. `mac_profile_names_every_mach_service` holds the
  shape.
- **`(allow signal (target same-sandbox))`** binds sending only: a timeout's
  kill *into* the sandbox lands, `kill -STOP` back out at the host ral does
  not. Descendants share the instance; two per-command children of one grant
  do not, so signalling between sibling jobs is denied with everything else.
- **`sysctl-read`**: libdispatch and libmalloc probe on init and abort if denied.
- **`(allow file-read* (literal "/"))`**: dyld reads the root inode before
  `main`. ral's walk no longer needs it, and its root too would be a search
  handle once cap-primitives spells `O_SEARCH` in `open_ambient_dir`.
- **Device writes** — `/dev/null`, `/dev/zero`, `/dev/tty`, `/dev/dtracehelper`
  (and its ioctl): `2>/dev/null` and libc paths open these for write even
  though `/dev` is readable.
- **`/private/var/db/timezone/icutz`**: libc's first `localtime_r` reads the
  ICU timezone database; it lies outside every grant prefix, and rustc, cc and
  ld SIGABRT in `tzsetwall_basic` without it.
- **`ipc-posix-shm`**, and the notification center by its POSIX shared-memory
  name rather than a Mach one: CoreFoundation and libdispatch use both during
  framework init.
- **`(allow file-ioctl)`** unfiltered, as in Xcode's, Bazel's and Chromium's
  build sandboxes: an ioctl acts on a descriptor already held and grants no new
  open; the dangerous ones (format, eject, raw-socket reconfiguration) need a
  device node this profile never lets one open. The open-time gate is the
  boundary. It is also the known hole `/dev/tty` sits in: the projection
  carries fs, net and exec but not terminal-loan state, so the profile cannot
  deny tty ioctls to an ordinary Denied-terminal child while admitting them to
  a full-screen one.

## Diagnosis

`RAL_DUMP_SANDBOX_PROFILE` prints the rendered profile. `sandbox::diag` turns a
kernel denial into a hint per operand class — a path to `read`, `write` or
`exec`, a network operand to the `net:` bit, a Mach or IPC operand to nothing,
since a door is the base's to decide and no grant widens one
([[map/core/capabilities|capabilities]]).
