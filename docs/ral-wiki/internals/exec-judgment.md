---
generated_at_commit: 21778dc9d2ea3aa347c7ec0d2cb0d902fc37ffde
verified_at_commit: 21778dc9d2ea3aa347c7ec0d2cb0d902fc37ffde
verified_at_date: 2026-10-28
anchors: [Head, ExecRules, ExecAllow, ExecDeny, Withheld, ExecDenial, Evidence, GrantStack, admit, Head::admit, ReadRegion::judge_file, check_exec, Admitted, for_kernel, places, deputy_prefixes]
---

# Exec judgment: which program may run

**ral judges a command by the program it will run, then runs exactly that
program.** One table decides, `ExecRules`, and both the guard and the kernel
read it ([[design/authority-tables#Two instances|authority-tables]]). This page is the exec half
of [[internals/capability-enforcement|capability-enforcement]].

## The head is judged by its program

**`Head::resolve` (`shell/exec/command/head.rs`) finds the `Program` once**: a
bundled `Tool`, or a host `File` with its spelling and its real path.
`Head::admit` judges the head once per launch through `GrantStack::admit`: at
classification for a plain call; at launch for a direct pipeline stage and
`detach`. It judges the exec table, then what running reads (below), and mints
`Admitted`, which `vet` plans from and the launcher takes. A head naming no
file passes unjudged to `vet`.

- **A bundled name is its tool**, found with no walk, so the in-binary
  implementation wins over a host twin; any other bare name is the effective
  `PATH`'s hit.
- **A path head is walked as written**, anchored at the launch cwd: the kernel
  walks a `..` after the links before it and honours a trailing `/`, so a
  lexical fold would judge another file than `execve` reaches. Windows folds
  `..` itself, and is handed an absolute path.
- **Arguments play no part.** An allowed target runs with any arguments, a
  denied one with none. `Admitted` holds only the judged program and its exec
  table; `Refused` holds the program and its denial. The caller owns argv, and
  no refusal records it.
- **A path key** matches the real path by equality, and a directory key by
  containment. Specificity decides nothing: **an exclusion beats every
  allowance**, so an exact file or a deeper directory never reopens an excluded
  directory ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).
- **`every` is a flag, not a key.** It resolves to `ExecAllow::Any`, positive
  for every program, bundled tools included, and has no negative form.
- **A bare key** means the file the decoding env's `PATH` finds. So a planted
  `/tmp/evil/rg` does not inherit ``allow: [`name rg]``.
- **A bare exclusion** also vetoes the name wherever it resolves. It stops an
  absolute `/bin/bash` and a symlink to it alike.
- **A head that names no file has no program.** It fails with 127 or 126, never
  with a denial. On Windows a path head written without its extension offers
  the suffixed file beside it, if one exists.

## Running reads

**A program that runs must be readable.** After the exec table, `admit` asks
what running reads (`ExecDenial::Read`): `ReadRegion::judge_file` over the
`read_region(baseline)` fold for a file, which vouches for the handle, and the
image's read for a bundled tool.

- **A host file** reads its real path. A grant reads what it, and the grants
  enclosing it, let run by path, directory or name (its run places,
  `ExecRules::places`): pairing looks down the stack, so an fs-only restriction
  nested under a grant that runs `cargo` leaves `cargo` readable, and a nested
  grant never makes anything readable by naming it in exec. Exclusions do not
  pair: an fs exclusion of a run place refuses running it, naming the exclusion.
- **A program that runs only through `every`** must be readable under fs.
- **A bundled tool** reads ral's binary, which only an fs exclusion withholds:
  it runs ral's own image (`/proc/self/exe`, the AppContainer's image). An fs
  exclusion of ral's binary therefore refuses every bundled tool; an exec
  exclusion of ral leaves them alone.
- **ral's binary is an ordinary host file.** It runs exactly where a grant
  admits it, by path, directory or name, in the guard and the kernel alike;
  `exec: [allow: []]` runs nothing, ral included. A nested ral is bound by the
  kernel alone and boots with no stack.

## ral runs the object it judged

**The launcher execs the `File`'s real path, with the user's spelling as
`argv[0]`** (`Launch::arg0`; the warrant's `file` carries both). So a link
retargeted between judgment and `execve` (by an earlier pipeline stage, say)
cannot run another program.

- **A renamed ancestor, or a rewrite at the real path, between judgment and
  `execve` runs the new file** wherever the policy admits that path, and
  nowhere else: inside the envelope the kernel judges the inode `execve`
  meets. This is a declared limit, since a writer of the path could have
  placed the file before judgment
  ([[decisions/261013_the-image-is-not-pinned|the-image-is-not-pinned]]).
- **A `#!` script reached through a link sees its real path as `$0`.** The
  kernel hands the interpreter the exec path, not `argv[0]`.

## The kernel holds the same evidence

**A launch runs under the stack that admitted it.** `build_launch` projects the
live stack beside the `Admitted`'s table; each path admits and launches within
one machine step, and the stack changes only at `Frame::Grant` and
`Shell::with_pushed`.

**`authority::guard::check_exec` judges the program before the spawn. The kernel then
holds the same table against programs the child starts itself.**

- **The kernel's rules are `ExecRules::for_kernel` of the table inside the
  `Admitted`, joined with live support.** Support is what a launch needs beyond
  the policy: carriers
  ([[decisions/261004_exec-carriers|exec-carriers]]) and the Linux dynamic
  linker. It yields to every exclusion on either axis, grants running and
  reading and never writing, and is computed for every confined launch.
  Bundled-tool scopes are dropped.
- **macOS:** the Seatbelt profile renders every positive `process-exec`
  clause, then every negative one, and its fs section refuses running whatever
  it does not read. Ordering within a
  group is for stable output; depth grants no exception, so an exclusion wins
  as it does in the guard. The kernel sees no argv, and no bundled tool's name
  ([[internals/seatbelt-profile|seatbelt-profile]]).
- **Linux:** the parent builds a Landlock ruleset, and the payload enters it
  inside the bwrap envelope
  ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]). Landlock only
  allows, so each admitted directory is rendered *less* what the table
  excludes beneath it, over the tree as it stands at launch; a name veto is
  walked only beneath directories no write reaches. That is a choice, not a
  kernel limit: walking beneath a writable directory would admit each file
  present at launch and leave later ones unrunnable, which would break building
  and then running in one command (`make && ./a.out`) under any veto. `Any` is lowered as a rule
  on every bind root; the bind plan exists before the ruleset
  ([[internals/linux-envelope|linux-envelope]]).
- **Windows:** the AppContainer has no path-exec filter. The guard stands
  alone, and the gap between check and exec stays open.

**A refused `execve` of a script says why.** The child used to advise allowing
the interpreter, which is wrong when an exclusion covers it, and it holds no
policy to tell; discovery cannot tell either, since it skips a script the user
can write. So the parent reads the `#!` line for diagnosis only, never as
evidence, and records in the warrant's `Run::File` the exclusion covering the
interpreter on either axis, if any; the hint then says `its interpreter '{c}' is
excluded by {scope}; an allowance cannot override an exclusion`, and otherwise
keeps the old advice. No dependency resolver is built.

## An exclusion is a guardrail, not a wall

**A copy of an excluded binary under another name is a different file.** It
carries no trace of the refused name, and an allowed directory admits it. That
is true in the guard, and in the macOS profile too, since Seatbelt's
`(deny process-exec (regex #"/bash$"))` also sees only the new name. A program
written into a writable, runnable folder, or a script fed to an admitted
interpreter, is the same.

- **What holds is the projection, not the name.** The copy runs under the same
  confinement as the process that wrote it, so it reaches nothing new.
- **There is no write freeze.** A broad write that covers a run-admitted
  directory lets a child author programs there; where that directory is on the
  user's own `PATH`, the program later runs outside any sandbox. `edit-only`
  launched from `~` is the shipped case (`~/.local/bin`). Name vetoes beneath
  writable directories are advisory to descendants
  ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).
- **`authority/policy/deputy.rs` reports the overlap** (`deputy_prefixes`): the live
  part of the meet of the exec region and the write region, so each writable
  island under a runnable directory is named and a directory wholly excluded on
  either side is not. It reports only where exec does not denote `t` and fs
  writes something restricted, and only into an open `audit` trail
  ([[design/audit|audit]]); the route is `ral --audit --capabilities
  exarch/data/edit-only.exarch.ral -c true`, then the trail's `deputy` checks; it reports rather than denies
  ([[design/grant|grant]]).

**How the deputy meet is built.** The findings are `(R ∧ W).live()`, each side
folded as the guard folds it, so neither side's exclusions are lost.

- **`R`** is the meet, over the exec-supplying layers, of a region holding each
  layer's `Dir` keys with their full evidence, and the root prefix as Allow for
  a layer with `every`. Treating an `every` layer as an empty region instead
  would let the meet erase every other layer's finding. Where exec denotes `t`
  there is no finding: unrestricted running is not a deputy.
- **`W`** is `authority::policy::fs::region(FsOp::Write)` itself, deny paths included, not a second
  reading of `write_prefixes`; where fs writes nothing restricted it is `None`
  and there is no finding. `deputy_prefixes` takes the resolver `authority::policy::fs::region` takes.
- **The meet** keeps both sides' exclusions, so a directory wholly excluded on
  either side holds Both and is dropped (running `/work` less `/work/private`
  while writing only `/work/private` reports nothing), and keeps every positive
  the other side supports, so each writable island is reported (running `/work`
  while writing `/work/a` and `/work/b` reports both). A partially excluded
  directory is still reported, conservatively, with no region-difference engine.
- **Where it is read.** Findings are recorded only by
  `Shell::audit_deputy_prefixes` when a session layer is pushed under an open
  trail. exarch's ordinary dispatch opens none and its cards suppress flagged
  checks, so no banner, summary or prompt shows them: the agent's prompt would
  tell it where to plant programs the user later runs outside any sandbox. With
  the freeze gone these findings are where an author learns which folders are
  both writable and runnable.
