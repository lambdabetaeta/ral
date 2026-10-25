---
generated_at_commit: 773daa1062bb84e64c799cf502a0a5288c173f20
verified_at_commit: f0b81686
verified_at_date: 2026-10-17
anchors: [apply_profile, build_profile, Profile, emit_fs_restricted, emit_exec_rules, Clause, Filter, emit_ancestor_metadata, withheld_services, rendered_ancestors, pinned_dirs, step]
---

# The Seatbelt profile: an object policy in a name language

Seatbelt judges **names**. ral judges **objects**
([[decisions/260906_object-not-name|object-not-name]]). Every line of
`core/src/authority/sandbox/macos.rs` that is not a straight rendering of a grant is the
cost of saying the second in the first:

- **Two spellings per path.** `/tmp` is a firmlink to `/private/tmp`, and the
  kernel presents a lookup under whichever spelling the process used, so every
  rendered name (`render_paths`) carries both — denies included.
- **No case or normalisation spellings.** Measured (macOS 26.5), `literal`,
  `subpath` and `regex` all match modulo canonical caseless equivalence, for
  absent names as for existing ones, on case-insensitive and case-sensitive
  APFS alike: a deny on `Secrets` refuses `mkdir secrets`, and an NFC deny an
  NFD create. On APFS that is all the guard's deny relation holds
  ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]),
  the classes it adds for ZFS (ı ≡ I, compatibility forms) being distinct
  objects there, so a deny is rendered once. The residual is on the allow side: on a
  case-sensitive volume an allow on `Work` also admits the distinct `work` to
  a child, which the guard refuses; SBPL has no exact match to say otherwise.
- **Ancestor chains.** `(allow file-read* (subpath P))` does not make `P`
  reachable: Seatbelt gates each directory lookup on the way, so every proper
  ancestor of every granted name gets `(allow file-read-metadata (literal …))`
  (`emit_ancestor_metadata`, over `rendered_ancestors`). Metadata suffices
  because *search* is all a resolver holds on an ancestor — the kernel's,
  `posix_spawn`'s, and ral's own walk, which opens its intermediates `O_SEARCH`
  for exactly this reason (the directory step of `path/walk/macos.rs`). A read handle
  there would need `file-read-data`, and a readable ancestor is a listable
  one: `/Users`, a home directory's dotfile names.
- **Pins.** A deny names a path, so renaming its parent would carry the bytes
  to a name the deny does not cover; every proper ancestor of a deny within a
  write prefix is `(deny file-write-unlink (literal …))` — `literal`, never
  `subpath`, so its entries stay mutable
  ([[internals/holding-a-deny|holding-a-deny]]). Linux pays
  nothing here: a mount is anchored to an inode.
- **Order.** Seatbelt is last-match-wins, so every deny is emitted after every
  allow in the profile, not merely after its own. `Profile` is a record whose
  field order *is* that precedence — fs allows, exec rules, exec ancestors,
  fs denies, pins, net — and its `Display` writes the sections in it.
- **ral's own file.** A sandboxed command must not swap the ral that confines
  its children, so in every profile ral's file is write-denied and its
  ancestors unlink-denied, whatever the grant.

The lens this gives: when the profile and ral disagree, ask first whether ral
is asking for more than the kernel's resolver would — the walk bug was exactly
that — before making the profile say more.

## What `build_profile` renders, rule by rule

**fs.** `Restricted` renders the grant's read prefixes — which already carry
the read `floor`, joined by the fold — and its write prefixes as `file-read*` subpaths with
their ancestor chains, the write prefixes as `file-write*` subpaths, and then
the denies: each exclusion denies reading, writing, linking and running, and
`(deny process-exec (require-not (require-any (subpath …) …)))` over the read
region refuses every program the fs does not read (plain `(deny process-exec)`
when it reads nothing, which SBPL cannot say as an empty `require-any`).
Seatbelt itself starts a program it may not read: measured on Darwin 25.5, a
Mach-O, and a script's interpreter, need `process-exec` alone. So *running
reads* is a clause here, not a kernel property; without it a descendant ran
anything `every` admits, and an fs exclusion stopped nothing running. `Unrestricted` passes both through — `(allow file-read*)`,
`(allow file-write*)` — so an exec-only grant can enter the sandbox for exec
confinement without clamping the agent's cwd or `HOME`.

The macOS `floor` is wholesale where cherry-picking breaks tools mysteriously:
`/private/etc` (gitconfig, paths.d, zshenv, nix.conf; nothing user-secret sits
there unprotected — `master.passwd` is 0600, and Seatbelt enforces inode
permissions atop the profile), `/private/var/select` (xcode-select's
`developer_dir` and `sh`; denied, build drivers misreport the EPERM as a broken
install), `/private/var/run` (`resolv.conf`'s target and the mDNSResponder
socket). User temp and workspace paths are absent by design: they arrive
through the grant.

**exec.** `Unrestricted` is `(allow process-exec)`, so an fs-only grant does
not attenuate exec at the OS layer; `Any` (`every`) renders the same way, with
the table's exclusions after it, and the fs section's read bound below them.
The backend admits nothing of its own: an Intel program under Rosetta needs
`process-exec` on itself alone (measured), so `/Library/Apple/usr` is only read,
through the floor. ral's own binary is *not* admitted:
it is an ordinary file, allowed where a policy clause allows it and reached by
the denies after the positives. Then every positive exec clause, then every
negative one. Positive clauses are `process-exec` alone, `(allow process-exec
(subpath|literal …))`; reads, run places and support among them are the fs
section's, which renders them whether exec is restricted or not. Support
(carriers, [[decisions/261004_exec-carriers|exec-carriers]]) arrives among the
positives as `File` allows. A negative clause is `(deny process-exec …)`, and a
veto is `(deny process-exec (regex #"/name$"))`, the name wherever it resolves;
an entry with both bits emits both clauses. Seatbelt matches each form
caselessly, as the guard reads an exclusion. **Ordering within each group is
for stable output only; depth grants no exception**, so last-match-wins puts
every exclusion over every allowance, which is the guard's rule
([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).
**Exec exclusions deny no reads**: reading is fs's to decide, and a veto would
otherwise hide every same-named file. Allowed files and directories get their
ancestor chains, computed from `live()` so that a suppressed scope never becomes
an admitted root. Bundled tools run in the confined child, so no profile
clause admits ral for them; they read ral's binary through fs. This layer still
cannot see argv; its job is the interpreter-bypass class the guard never sees
(`sh -c`, `env`, `xargs`, `find -exec`). A program that runs only through
`every`, or with exec omitted, starts only where the fs reads it, by the read
bound above. The confined child applies its profile after it has started, so its own
image is already mapped when the profile takes force.

**Allowances render resolved.** Every read, write and run allowance is rendered as
its resolved real position, with its firmlink twins, and as nothing else: a surface
spelling is never a predicate of its own. Seatbelt checks the resolved path of
each access against it, so a symlink put in an allowance's place reaches
nothing. It does gate the `lstat` of each link a lookup passes, so what the walk
of an allowance's spelling passed is stat-able and no more: `Object::walk` is
that walk's `Trace::passed` — each link it followed, a chain's middle ones
included, and each directory a `..` in a target climbed out of — each named
under its parent's real path and admitted as a `file-read-metadata` literal,
with its proper ancestors, which is how the directories an absolute target
abandons are covered. The spelling reaches the object while it leads
there, and nothing beneath it once retargeted. Exclusions are rendered from the resolved real paths their walks held, never from the text as written; Seatbelt judges names, so the identity reading is ral's own readers'.

**There is no write freeze.** An admitted directory a broad write covers stays
writable, with or without a veto, under any fs; a name veto there is advisory to
descendants, and `authority::policy::deputy` names such folders
([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).

**net.** Enforced by absence. The base admits `network-outbound` for nothing;
only a grant that does not say ``net: `offline`` appends `macos-net.sbpl` — the wholesale `(allow network*)`,
Seatbelt having no per-address rules, and the Mach services that *are* the
network: configd (proxies, DNS servers), the dnssd port, trustd. That silence
is what closes DNS: `getaddrinfo` reaches mDNSResponder over the UNIX socket
`/private/var/run/mDNSResponder`, which Seatbelt gates as `network-outbound`,
not `mach-lookup` (measured on Darwin 25.5.0: denying `mach-lookup` outright
still resolves names; admitting that socket alone resolves them with
`mach-lookup` denied). Admitting the socket for any local reason would make a
hostname an egress channel — an attacker-chosen query label leaves through a
daemon outside the sandbox — while ``net: `offline`` still read as closed.
`mac_profile_denies_network_when_disabled` asserts this over every rendered
rule, with an open-network positive control so it cannot pass vacuously.

## The base, `macos-base.sbpl`

Policy-independent: true of every entry, whatever the grant. Its shape is
adapted from BrianSwift/macOSSandboxBuild's `confined.sb` — deny-default plus
selective allows, the folded `file-read* process-exec` idiom, `process-fork`
beside `process-exec` since Seatbelt gates the two separately — with paths
inlined at build time rather than passed as scalar `(param …)`s, a grant's
admits being variable-length.

- **Mach services are named, never wholesale.** A Mach name reaches a daemon
  that acts on your behalf *outside* the profile. Named: `com.apple.dyld`
  (dyld locates the shared cache through launchd; without it the child aborts
  before `main`), the notification center (libnotify, on the first
  `notify_register_*`), opendirectoryd's libinfo and membership (`getpwuid`,
  `~`, `whoami`, `id`), and the loggers (`os_log`; a denial is survivable, but
  every libSystem process hits it and would drown the denial log
  `authority::sandbox::diag` reads). Withheld, each with its reason as *data* in
  `withheld_services` so the denial hint can quote it — a comment cannot reach the
  agent holding the EPERM, and a deliberate `no` read as an accident is chased
  instead of worked around: `launchservicesd` (and `lsd.*`) would spawn
  whatever `/usr/bin/open` names as an unconfined child of launchd — `open -a
  Terminal ./payload.command` is a full escape, `open https://…` egress under
  ``net: `offline``; the pasteboard is a clipboard no grant mentions;
  `SecurityServer` hands out keychain items (`security
  find-generic-password`). Withholding securityd has one measured cost: cargo's
  bundled libgit2 speaks TLS through SecureTransport, whose handshake reaches
  securityd and, denied, fails as `ssl handshake -9808` and blames a missing
  revision; Apple's `curl` and `git` reach trust through trustd alone. So
  exarch tells cargo to fetch through the `git` binary
  (`app::CONFINED_TOOL_SETTINGS`) rather than admit a service that hands the
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
- **`(allow file-ioctl)`**, unfiltered as in Xcode's, Bazel's and Chromium's
  build sandboxes, then **`(deny file-ioctl (ioctl-command TIOCSTI))`**. The
  allow is there for `tcsetattr`: without it a full-screen child cannot raise
  raw mode (`tcgetattr` and `TIOCGWINSZ` pass either way). It is also the
  known hole `/dev/tty` sits in: the projection carries fs, net and exec but
  not terminal-loan state, so the profile cannot deny tty ioctls to an
  ordinary Denied-terminal child while admitting them to a full-screen one.
  The open-time gate does not cover it, since an inherited terminal needs no
  open, and `ioctl(fd, TIOCSTI, …)` would type into the unconfined parent's
  input. Measured on macOS 26, Seatbelt refuses that under `hid-control`, not
  `file-ioctl`: a `(deny default)` profile that never allows `hid-control`
  returns `EPERM` whatever it says about `file-ioctl`, and allowing
  `hid-control` alone lifts it. The base never does; the explicit deny, after
  the unfiltered allow, makes the refusal ours should that gate move.
  `ioctl-command` takes a symbol (hex is an unbound variable) and, given a
  number, matches only its low 16 bits — group and number, not direction or
  size — so write the name. A `NewSession` launch has no controlling terminal,
  and unconfined XNU refuses TIOCSTI itself (`EACCES`, `isctty` in `tty.c`);
  confined, Seatbelt answers first. The foreground launch keeps the terminal,
  so there the profile is the only wall.

## Diagnosis

`authority::sandbox::diag` turns a kernel denial into a hint per operand class — a path
to `read`, `write` or `exec`, a network operand to the `net:` bit, a Mach or IPC
operand to nothing, since a service is the base's to decide and no grant widens one
([[map/core/capabilities|capabilities]]).

Entry has one diagnosis of its own. A lineage already inside a profile —
a per-command child of a confined runner — gets `EPERM` from `sandbox_init`,
since profiles do not stack, and `apply_profile` reports it as such: the launch
is refused rather than run under the wider profile
([[design/two-enforcers|two-enforcers]]).
