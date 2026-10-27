---
status: active
generated_at_commit: 104cb519
---

# An envelope does not touch the host

**Building a sandbox is the construction of a *view*, and a view that scratches
the glass is broken: assembling the envelope must leave the host filesystem
exactly as it found it.** On Linux that rules out one mask outright — a
[[design/grant|grant]]'s `deny` of a name that does not exist — because bwrap
can only mount over a name, and a name it has to create first, it creates on
the host.

## The mount API cannot forbid a name

`bwrap` has no negative path rule, so `core/src/sandbox/linux.rs` realises a
deny as a mount laid over the denied path. Every such mount needs a mountpoint,
and bwrap creates one — a directory, or an empty file for a file mask — when
the path is absent, inside the new mount namespace, whose writable binds are
*identity* binds of real host directories. So that creation lands on disk:

- under a **read-only** bind it fails with `EROFS` and kills the envelope
  before the body execs — one deny of `xdg:config/gcloud` on a host that never
  installed gcloud, and every external command under the grant died in sandbox
  setup;
- under a **writable** bind it succeeds, on the host, and the deny *creates the
  very name it forbids* — `deny: ['cwd:/.env']` leaving an `.env` directory in
  the user's project tree, which the user's own tools can then never write.

Neither is a deny. The first trades the whole session for it; the second
performs the act it was asked to prevent, permanently, outside the sandbox's
lifetime.

## Decision

- `DenyMask::over` masks only a name that already exists: a directory takes
  `--perms 0000 --tmpfs`, an existing non-directory takes `--ro-bind
  /dev/null`, a symlink is masked at its resolved twin, and an absent name
  takes nothing at all, whatever the binds around it.
- Under a read-only bind the host can create the name during a launch, and
  that launch reads it; the next one masks it. The child's writes are refused,
  but the host's underlying directory remains writable.
- Under a writable bind the child can also create the name, read and write
  what it made, and leave it on the host. The next launch masks it. ral's
  in-process guard refuses reads and creation throughout, but cannot see a
  spawned child's own operations ([[design/two-enforcers|two enforcers]]).
  The risk includes both a host-created secret leaking during the launch and
  a child planting a file. It joins
  [[decisions/260530_linux-exec-confinement|linux-exec-confinement]] as a seam,
  not a solved case. macOS Seatbelt ranges over names and refuses both reads
  and creation from the start. Windows carves around the excluded name, so a
  name made in a carved directory during the launch stays unreadable
  ([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).

## Where the directory mask stops

The `--tmpfs` mask over a denied directory is unreadable and unwritable — the
missing permission bits refuse the owner as much as anyone, so a tool that
writes there is refused, honestly, rather than told it succeeded. But the tmpfs
belongs to the sandboxed uid, so a child that deliberately `chmod`s the bits
back has private scratch space at that name: memory the host never sees, and
never the denied directory. Only a *read-only mount* makes the mode immutable,
and bwrap 0.11.2 refuses `--remount-ro` over a tmpfs it has just mounted —
though the kernel accepts the same `MS_REMOUNT|MS_BIND|MS_RDONLY` from
`mount(8)`, and bwrap's own `--ro-bind` performs it. So the read-only mount has
to be a bind, and a bind needs an empty source directory this process holds for
the life of the run. Whether the mask is worth that is open.

The failure mode of every rule here is a sandbox that never launches or a mount
that lies, and neither shows in an argv assertion, so each is pinned by a test
that spawns the envelope for real (`sandbox::linux::tests`).
