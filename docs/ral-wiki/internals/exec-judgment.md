---
verified_at_commit: 0dc4a920
verified_at_date: 2026-10-09
anchors: [Head, ExecRules, check_exec, Admitted, for_kernel, carriers, deputy_prefixes]
---

# Exec judgment: which program may run

**ral judges a command by the program it will run, then runs exactly that
program.** One table decides, `ExecRules`, and both the guard and the kernel
read it ([[decisions/261004_exec-rules|exec-rules]]). This page is the exec half
of [[internals/capability-enforcement|capability-enforcement]].

## The head is judged by its program

**`Head::resolve` (`runtime/command/head.rs`) finds the `Program` once**: a
bundled `Tool`, or a host `File` with its spelling and its real path.
`ExecRules::verdict` judges it, and it is the only function that does.

- **A path key** matches the real path by equality, and a directory key by
  containment. The deepest key decides.
- **A bare key** means the file the host `PATH` finds. So a planted
  `/tmp/evil/rg` does not inherit `rg: allow`.
- **A bare deny** also vetoes the name wherever it resolves. It stops an
  absolute `/bin/bash` and a symlink to it alike.
- **A head that names no file has no program.** It fails with 127 or 126, never
  with a denial.

## ral runs the object it judged

**The launcher execs the `File`'s real path, with the user's spelling as
`argv[0]`** (`Launch::arg0`; the warrant's `file` carries both). So a link
retargeted between judgment and `execve` (by an earlier pipeline stage, say)
cannot run another program.

- **A renamed *ancestor* still can.** On macOS the window runs up to the
  `execve`; on Linux, up to the moment the envelope opens the image to mount it
  ([[decisions/261006_the-envelope-mounts-by-handle|the-envelope-mounts-by-handle]]).
  Closing it needs `Program` to carry a handle from `Head::resolve`.
- **A `#!` script reached through a link sees its real path as `$0`.** The
  kernel hands the interpreter the exec path, not `argv[0]`.

## The kernel holds the same table

**`check_exec` judges the program and argv before the spawn. The kernel then
holds the rules against programs the child starts itself.**

- **The kernel's rules are `ExecRules::for_kernel` of the very table inside the
  `Admitted`.** The carriers are spliced in
  ([[decisions/261004_exec-carriers|exec-carriers]]), and each file is at its
  final verdict.
- **macOS:** the Seatbelt profile renders the rules as `process-exec` forms in
  `precedence` order. Seatbelt's last-match-wins is then the guard's rank, so
  guard and kernel cannot disagree. The kernel sees no argv, and no bundled
  tool's name ([[internals/seatbelt-profile|seatbelt-profile]]).
- **Linux:** the parent builds a Landlock ruleset, and the payload enters it
  inside the bwrap envelope
  ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]). Landlock
  only allows, so each allowed directory is rendered *less* what the table
  blocks beneath it, over the tree as it stands at launch. A veto is kept only
  where no trusted write reaches.
- **Windows:** the AppContainer has no path-exec filter. The guard stands alone,
  and the gap between check and exec stays open.

## A name veto is not a containment boundary

**A copy of a denied binary under another name is a different file.** It
carries no trace of the refused name, and an allowed directory admits it. That
is true in the guard, and in the macOS profile too, since Seatbelt's
`(deny process-exec (regex #"/bash$"))` also sees only the new name.

- **What holds is the projection, not the name.** The copy runs under the same
  confinement as the process that wrote it, so it reaches nothing new.
- **`capability/deputy.rs` reports the overlap** (`deputy_prefixes`). It names the writable,
  exec-admitted prefixes that make such a copy runnable, only where a grant
  restricts *both* fs and exec, since an unrestricted `fs` is not "everything
  writable". It reports rather than denies: the overlap is not itself an
  escalation ([[design/grant|grant]] §Concessions).

## A veto freezes what a write covers

**The backends must not let a write reach an exec-admitted directory the grant
did not name**
([[decisions/261006_a-veto-freezes-what-a-write-covers|a-veto-freezes-what-a-write-covers]]).
The folded grant already guarantees this. The backends did not: they rendered
an unrestricted `fs` as "every write allowed", and a broad write prefix as
writes into every admitted directory beneath it. A veto could then be answered
by authoring a copy there. So both kernels freeze what a write reaches without
naming it:

- under an unrestricted `fs`, the whole exec allow-set, when a veto is in force;
- under a restricted `fs`, each admitted directory that a write prefix
  *covers*.

On Linux the freeze goes on at every name a writable bind shows the admitted
directory. A prefix written through a symlink is two binds of one handle, and a
mount laid at one name does not show at the other.

**An overlap the grant names stays writable, and is reported.** That is a write
at or below an admit at or above the directory. `reasonable` admits `cwd:/` for
the very scripts it lets the agent write. `edit-only` and `read-only` admit no
directory they can write, which is what gives their `git` and `bash` denies
meaning.
