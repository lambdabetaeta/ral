---
status: active
generated_at_commit: 646bff0a
verified_at_commit: 646bff0a
anchors: [Program, Head, build_launch, Warrant, Confined, Slot, build, admit_exec, Binds]
---

# The image is not pinned

**The launcher execs the real path the guard judged, on every platform, and
nothing holds a descriptor on the launched image. The window between judgment
and `execve` is a declared limit, subsumed by pre-placement: any writer who
could swap the file could have placed it before judgment.** The kernel layer
(Landlock or Seatbelt) judges the object at `execve`, as macOS always did and
as Linux did until 12 Oct.

Supersedes the 12 Oct "Implementation update" of
[[decisions/261006_the-envelope-mounts-by-handle|the-envelope-mounts-by-handle]]
(the image pin, `94344cac`). The "renamed ancestor" residual of
[[internals/exec-judgment|exec-judgment]] is re-declared as subsumed, not
open. Builds on
[[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]],
which already made "where writing meets running" a declared limit.

## Context

The 12 Oct pin opened the image `O_PATH` before judgment
(`RealPath::open_image`, called from `Program::file`). Unconfined launches then
exec'd `/proc/self/fd/107` (`image_command`), and the trampoline re-opened the
image on the envelope's mount (`visible_image`), dup'd it below the handoff
without close-on-exec, and exec'd `/proc/self/fd/N`. The kernel's `#!` protocol
(`binfmt_script`) passes the exec path, not `argv[0]`, to the interpreter as the
script's name.

- **The symptom.** `$0` was `/proc/self/fd/107` for unconfined launches and
  `/proc/self/fd/4` for confined ones (measured on IT107528, WSL2, Linux 6.18),
  so `dirname "$0"` yielded `/proc/self/fd`.
- **The concrete failure.** dotfiles' `agents/mcp/wire` found no registry beside
  `$0`, so `jq -s` with no files read the terminal forever, and `claude.ral`
  hung after the menu.
- **The leak.** The script process also inherited a descriptor on itself.
- **The wiki contradicted itself.** `internals/exec-judgment` said a `#!` script
  reached through a link sees its real path as `$0`; `internals/confined-launch`
  said a script's `$0` is that descriptor path.

**What the pin guaranteed** is "the object run is the object judged", against a
writer who changes what the judged real path `R` resolves to between judgment
and `execve`. That writer must be able to write `R` itself or some directory on
`R`'s path, since nothing else changes the resolution.

## Decision

**Pre-placement reproduces every swap.** A writer who can change `R`'s
resolution after judgment can equally build a plain file at `R` before
judgment.

- A plain file is not a "redirect". The frozen allowance is a real path, so the
  guard admits it on the same terms.
- The file can carry any `#!`, be a copy of any readable binary, or be a wrapper
  `#!/bin/sh\nexec R'` for any `R'` the writer cannot copy.
- So whatever the swap achieves, pre-placement achieves the same or more. The
  one shape that needs the wrapper, a swap to a *symlink* aimed at an `R'` the
  writer cannot reproduce, is covered per launch mode below.

**Unconfined, no exec opinion** (`GrantStack::exec_is_top`): the plain shell,
every dotfiles launcher, every fs-only grant. Every object at every path is
admitted. No swap changes a verdict; there is nothing to protect.

**Confined, exec restricted.** Landlock judges the inode `execve` meets.

- A symlink swap aims `R` at a refused `R'`: `EACCES` in the kernel, pin or no
  pin.
- A plain-file swap is pre-placement.
- A copy of a refused binary under `R`'s name is the declared limit "a copy
  under a new name escapes a veto".
- Before the envelope exists, `Binds::open` opens `R` with `RESOLVE_NO_SYMLINKS`;
  a symlink swap in that window refuses the launch ("now goes through a symbolic
  link"). That is `Binds::open`'s behaviour and does not depend on the pin.
- The ruleset is built after judgment. `landlock::admit_exec` opens every
  `allowed_files` entry and every `lowered` directory by handle at launch, so a
  file swapped in after the ruleset is built is refused. Dropping the pin moves
  in the stricter direction here.

**Landlock needs no image admission.** Every shape that admits the launched
file in the guard reaches the kernel through the same table: an `` `absolute ``
key is `ExecScope::File` and joins `allowed_files`; a `` `name `` allow compiles
to `ExecScope::File` at `ExecRules::compile` (via `which::locate`); a `` `dir ``
key is `ExecScope::Dir` and joins `lowered`/`expand`; `every` is `Any`, so every
bind root; carriers join as files through `for_kernel(support)`. Before
`94344cac`, `build` had no image parameter and `admit_exec` admitted no image;
`admit_image` existed to admit the *pinned inode* when the name's inode had
changed, so it served the pin alone.

**Guest jail, exec restricted.** There is no kernel exec layer; the guard binds
only ral's own heads. A pre-placed wrapper runs anything, unchecked, because an
admitted program's children are unchecked there. The swap adds nothing.

**Landlock-less kernels.** `landlock::plan` refuses a restricting exec grant
where Landlock is absent (`Refusal::Unavailable`), so "confined, exec
restricted, no Landlock" does not launch. No case is left.

**What the model already declares.** [[design/two-enforcers|two-enforcers]],
"Writing meets running": an exclusion does not stop a copy, a newly written
program or a script fed to an admitted interpreter. The Belnap decision deleted
the write freeze and made the protection a declared limit: where writing meets
running, an exclusion is a guardrail, not a wall. A writer on `R`'s path
controls `R` by the model's own account, so the pin closed a window whose
contents the model had already given away. What is left of the pin is
atomicity, one launch running exactly what was judged: a diagnostic nicety, not
a security property.

### What stays, and why

- **Exec the judged real path, never the spelling.** This has teeth
  pre-placement cannot reproduce: `./tools/build` a link to an admitted file,
  retargeted after judgment by a writer of `tools/`. Exec-by-spelling would
  follow the new target; placing a file at the link's location beforehand fails,
  because that location is not admitted. In the jail only real-path exec catches
  it. `a_link_retargeted_after_judgment_runs_the_file_judged` (1 Oct) stays.
- **The user's spelling as `argv[0]`** for native images, as before. A script's
  `$0` becomes its real path through the kernel, as on macOS.
- **The boot pins on ral's own binary and on bwrap** (`reexec::own`,
  `pin_envelope`, `Slot::Trampoline`, exec'd as `/proc/self/fd/102`) guard the
  *enforcers* against a swapped build mid-session, a different property.
  Untouched.
- **The envelope mounts by handle** (`Binds::open`, `--ro-bind-fd`): *fs*
  authority, its own decision. Untouched. The image keeps its bind, shown by the
  read region alone (law 7), opened without following symlinks.
- **Everything else `94344cac` carried.** Windows stored-case allows
  (`path/identity.rs`, `path/walk.rs`, `RealPath::cmp`), the rlimit checks
  (`process/limits*.rs`), fs denies at the shell boundary
  (`core/tests/fs_deny.rs`), the covered-file freeze (since superseded by Belnap
  on its own terms). None of it serves the pin.

## Consequences

What is regained:

- **Every `#!` script sees its real path as `$0`.** The lexical idioms
  (`dirname "$0"`, zsh `${0:a:h}`, `BASH_SOURCE`, usage messages) work again,
  not only the canonicalising ones. Inside dotfiles, `agents/mcp/wire`,
  `bin/build-get-events` and `claude/install.sh` work.
- **No program inherits a descriptor on its own image.**
- **A bypass hazard disappears.** A host-mount descriptor reopened through
  `/proc` could bypass the envelope's read-only binds and masks;
  `visible_image` existed to prevent that, and with the descriptor gone so is it.
- **The machinery goes.** `RealPath::open_image`, `Program::File { image }`,
  `image_command`, `Slot::Image` (mounts back at fd 107), `visible_image`, the
  sub-handoff dup, `admit_image`, the `/proc/self/fd` exec in the trampoline and
  `shebang_of_image` are deleted; `Launch::new` is the unsandboxed launch on
  every Unix again.
- **The platforms agree**, and the wiki with itself.

What the kernel now reports:

- **A masked or hidden image is refused by the kernel, not by a sentence.**
  `visible_image` also refused a masked or hidden image with words. Now a masked
  image is an unopenable device node (`EACCES`, 126) and a hidden one is absent
  (`ENOENT`, 127). `exec` prints `ral: <real>: <err>` and, on `PermissionDenied`,
  the interpreter hint only if `shebang(real)` parses one, which it cannot for a
  mask. Both are exits a grant author can read; no new hint.
- **A confusing-error class, owned rather than fixed.** Carrier support is
  computed from the shebang at judgment. A file rewritten between judgment and
  `execve` (an editor save) may need a different interpreter, which Landlock
  then refuses with the "not admitted" hint. Pre-placement gives the same
  support, so no authority is involved; the pin made the error impossible.
- **`` `dir `absolute /usr/bin `` admitted nothing on IT107528.** The guard
  refused `/usr/bin/ls` under it, and with a trailing slash the kernel refused
  the script's children. Unexplained and independent of this change (the
  lowering is untouched); to be investigated separately. The end-to-end tests
  avoid `` `dir `` allowances.

**The commitment.** Reopening judge→`execve` atomicity would reverse this page.

## Where

`core/src/capability/exec.rs` (`Program`), `core/src/path/real.rs`,
`core/src/runtime/command/{head,process}.rs` (`Program::file`, `build_launch`),
`core/src/process/launch.rs` (`Launch::new`), `core/src/sandbox/launch.rs`,
`core/src/sandbox/warrant.rs` (`Slot`, `Confined`, `Warrant::confine`),
`core/src/sandbox/carriers.rs`, `core/src/sandbox/linux.rs`,
`core/src/sandbox/linux/landlock.rs` (`build`, `admit_exec`); tests in
`core/src/runtime/command/process.rs` (`judged_program`),
`core/src/sandbox/linux.rs` and `ral/tests/script_name.rs`.
