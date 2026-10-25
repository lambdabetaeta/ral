---
status: active
generated_at_commit: 773daa1062bb84e64c799cf502a0a5288c173f20
verified_at_commit: 002c654a
anchors: [Evidence, Meet, Join, Axis, Fragment, ExecGrant, Table, Withheld, ExecDenial, Refused, join_top, extend, places, GrantStack]
---

# Authority is Belnap evidence

**Fs and exec authority is a policy in the sense of Bruns and Huth's PBel: a
function from resolved requests to a pair of Booleans, positive and negative
support, composed pointwise and closed to a decision only at the boundary that
enforces. An exclusion beats every allowance, with no specificity ranking; an
extension adds to a ceiling once and only restriction follows; and what a grant
lets run or be written, it lets be read.** The mathematics, the instances and
every platform corner are [[design/authority-tables|authority-tables]]; the
user-facing reading is [[design/grant|grant]].

Supersedes the precedence and binary-verdict parts of
[[decisions/261006_one-table-two-instances|one-table-two-instances]] (rank,
`Verdict`, `meet_insert`, `Carrier` as a ranked scope, `respelled` as the
deepest fold, widening by `evicts`); the shared table, object identity and
dynamic stack stand. Supersedes whole
[[decisions/261006_a-veto-freezes-what-a-write-covers|a-veto-freezes-what-a-write-covers]].
Builds on [[decisions/261012_grant-surface-is-typed|grant-surface-is-typed]].
Dated decision pages keep the spelling `--extend-base`; the flag is now
`--extend`.

## Context

Exec ranked its scopes (`Dir(depth) < Carrier < Exact < Name`), so an exact
file or a deeper directory reopened an excluded directory, and a carrier
reopened it too. fs ranked by `(is a deny, depth)` and already let an exclusion
win. Verdicts were closed early: a table stored the final answer, a gap was
indistinguishable from a deny, and the meet kept an allow only where the met
verdict was not a deny. Several things followed that no reading of "the table
is a policy" could keep:

- **Extension read omission in the wrong role.** `Option<T>: Widen` took the
  supplied side when the other was `None`, so an omitted *base* axis was read
  as ⊥: `--base dangerous --extend-base git.exarch.ral` ran only `git`.
  `bool: Widen` was `&&` with an omitted verb `false`, so under extension an
  allowance *removed* authority.
- **The kernel and the guard disagreed.** Both kernels read every write prefix
  and every run allowance; the guard read the read list alone. Landlock admitted
  the dynamic linker and ral's own inode before the table; macOS admitted ral's
  image before the rules.
- **An exec allowance withdrew write authority**, in the kernel alone: the
  write freeze.
- **A symlink redirect read and wrote outside a grant.** Every fs prefix,
  allowances included, was re-frozen on every check and re-canonicalised at
  launch; a child that could write the folder holding an allowed folder
  repointed it, and both enforcers followed. Confirmed on Linux.

### What the old implementation did wrong

The semantics are not read off the code, and anything in the code that does not
fit them is a departure: an implementer who finds one not listed stops and
reports it, never preserves it by default, never invents a reading for it.
Each departure of the implementation as it stood at 6bfbb097, and its fate:

| Old | Why wrong | Now |
|---|---|---|
| exec `Rank` (`Dir(depth) < Carrier < Exact < Name`) and `most_specific` | an exact file or deeper directory reopened an excluded directory: contradicts rule 1 | deleted |
| fs ranked `(is_denied, depth)` | already exclusion-wins | re-expressed as evidence; fs judgments unchanged except writing and running reads |
| the guard's read region was the read list alone; both kernels also read every write prefix | the kernel permitted reads the fs guard refused: a layer that wrote a folder without listing it for reading let a child read it and ral not | writing reads, in the policy |
| `Table::meet` kept an allow only where the met verdict was not a deny | composed closed verdicts, not evidence | the evidence meet |
| `ExecGrant::widen` evicted an allow dir that `FrozenPath::evicts` a deny dir | destroyed positive evidence to emulate exclusion | both deleted |
| `ExecRules::for_kernel` wrote each file's final verdict; `Carrier` outranked a dir exclusion | early closure; a carrier reopened an exclusion | carriers are support |
| `Option<T>: Widen` took the supplied side when the other was `None` | read an omitted *base* axis as ⊥: `--base dangerous --extend-base git.exarch.ral` ran only `git`, could read only `~/.gitconfig`, `xdg:config/git` and the floor, and wrote nothing | roles as types, `Capabilities::extend` |
| `bool: Widen` was `&&`; an omitted verb field was `false` | under extension an allowance removed authority: base `editor: [read: true]` extended by `editor: [write: true]` kept neither | verb sets joined by union |
| Landlock `admit_exec` admitted `platform_base()` before the table | the dynamic linker bypassed every exclusion on Linux | support unless excluded |
| Landlock admitted ral's pinned inode unconditionally; macOS emitted it with the Rosetta loader before the policy rules (so an exec exclusion reached it there, and the fs denies followed it) | the backends disagreed about a descendant's ral; Linux exempted it from every exec exclusion; a descendant ral boots with no stack and ran every bundled tool an exec allow-list omits. The admissions served only a nested ral and its re-execs, since a bundled tool runs inside the confined child, started before confinement | both deleted |
| a bundled tool was judged by exec alone, yet runs ral's image | on Linux its `/proc/self/exe` reads that image whatever masks ral's name, and `env /proc/self/exe -c …` starts a descendant ral; on Windows an fs exclusion over `ral.exe`'s folder made the deny walk put an explicit deny on the image, which outranks its grant, so the AppContainer could not load it while the guard admitted the tool | a bundled tool reads ral's binary |
| the guard judged `ral -c` as any host file (refused unless a grant listed ral) while the kernel admitted ral's pinned inode for indirect starts (`sh -c 'ral …'`, a `#!` ral script) | the shell failed when called directly and ran when reached indirectly | ral runs exactly where a grant admits it, in both |
| a nested ral, or `exarch --ral-bundled-tool mv`, boots with no grants; Landlock admitted exarch's own inode unconditionally | exarch's agent could run a bundled tool its grant excluded, or shed its verbs, wherever the fs grant allows the effect | exarch's self layer; a nested ral wherever a grant admits it is a declared limit |
| the guard re-froze every fs prefix, allowances included, on every check; the projection re-canonicalised them at launch | a child that can write the folder holding an allowed folder repoints it with a symlink; confirmed on Linux, reading and writing outside the grant in the guard and the kernel | allowances frozen, exclusions re-resolved |
| Windows canonicalised every grant before stamping it | the same redirect, through a junction | stamp the frozen path, through a held handle |
| `FrozenPath::from_surface` stored its real form through `to_string_lossy` | harmless only while allowances re-resolve: frozen for good, a lossy real form names a different object | freezing refuses a real path that is not Unicode |
| Windows turned the universal prefix into the path `/`, which canonicalises to the current drive; exarch's `root_fs_policy` reached it under `dangerous` with a restrict file | "every path" became "this drive", stamped whole | Windows refuses the universal prefix |
| carriers were discovered only for a restricted exec table | under an fs-only restriction a trusted interpreter outside the reads was never shown | support for every confined launch |
| an exec allowance rendered as `file-read* process-exec` (macOS) and a read-only bind (Linux `shown`); the guard granted no such read | the kernel read what fs never granted | running reads, in the policy |
| `Capabilities::is_restrictive` was syntactic | a policy that denotes `t`, such as `every` with no exclusion, activated launch hardening | denotational activation |
| the write freeze (`exec_freeze`, `linux::frozen`): under a restricted fs, every run-admitted directory and exact file that a broad write covers without naming it was made read-only, exclusion or not; under an unrestricted fs, the whole admitted set once an exclusion existed | an exec *allowance* withdrew write authority, and only in the kernel: contradicts rule 3, the extension invariant and the guard/kernel contract | deleted; the protection is a declared limit |
| `WriteReach::{Trusted, Covered, Apart}` | the three-way split served the freeze | "writable or not" |
| `FsVerdict::{Unrestricted, Granted}` | every production use treated them alike; only tests told them apart | one `Result` |
| `ExecGrant`'s map was `pub` | any caller could bypass `meet_insert`; once values are evidence any caller could store Neither or bypass joining | private; built only by joining |
| `respelled` cited the deepest folded deny | needs rank | a deterministic witness |
| `deputy_prefixes` read authored exec dir allows, ignoring exec exclusions, and rebuilt writes from `write_prefixes`, ignoring `deny_paths`; its `.find()` reported one writable region per runnable directory | reported wholly excluded regions on either side, and missed all but one writable island | the meet of the two regions, what stays live |
| `minimal` allowed `system:` and excluded `/opt/homebrew/` while promising brew tools could be opted in; `git.exarch.ral` relied on it | an exclusion used to mean "not included" | Homebrew leaves `system` for `homebrew` |
| `reasonable` names toolchains by hard-coded home paths | ignores `CARGO_HOME`, `PYENV_ROOT` and the like; Volta, pipx and uv targets resolve outside the listed dirs and were likely refused | deferred to a package-manager-selectors plan, which needs override rules of its own |
| profile comments sent readers to `path/sigil.rs::system_tool_roots` | the function is `guard/freeze.rs` | corrected |
| exarch's `deny_layer` built `root_fs_policy` plus denies | is `t ⊕ (f if X)` | `join_top`; `root_fs_policy` deleted |

### Scope

The typed surface of
[[decisions/261012_grant-surface-is-typed|grant-surface-is-typed]] is a
prerequisite and is used as it stands: `Only` is gone and an old `Only` policy
is refused, never translated into an allowance. Never ship a state in which the
guard and a backend disagree on these rules, or in which some exec consumers
rank while others exclude: a guard with the new rules over the old kernel
behavior is not a finished change, and neither is a kernel whose `shown` stops
reading the exec table before it reads the places in `read`. Out of scope:
argument policies; socket containment; any public policy-expression language;
PBel operators beyond the fragment; provenance of evidence; caches; write-only
fs exclusions; package-manager selectors beyond `homebrew`. Kept: `GrantStack`,
`Admitted`, the held-object guard, freezing, path identity and loader trust.

## Decision

Six user-facing rules, then the machinery that makes them true.

1. **An exclusion beats every allowance**, fs and exec, regardless of
   specificity, in the guard and the kernel. Neither a narrower directory nor an
   exact file reopens an excluded region; nor do a carrier, the dynamic linker
   or ral's own binary.
2. **Authority is a ceiling, extended once, then only restricted.** The ceiling
   is a base extended by at most one fragment (`--extend`); every later layer
   restricts. Within ral there is no extension.
3. **In the extension, an allowance never removes authority; only an exclusion
   does**, on every axis, including one the base leaves unrestricted.
4. **Running and writing read.**
5. **Where writing meets running, an exclusion is a guardrail, not a wall.**
6. **An allowance names what it named when it was granted;** an exclusion
   follows its name; a bare program name is live per run.

The mechanism:

- **`Evidence { positive, negative }`** replaces `Verdict`. `∧` is the truth
  meet, `⊕` the information join, `↓` the wrapper applied once at enforcement.
  Both is not malformed: allowing a directory and excluding a subtree produces
  it inside the subtree. No `[⊤ ↦ f]` below the enforcement boundary.
- **Roles are types.** `Capabilities` is a policy, in which an omitted axis is
  `t`; the loader returns a `Fragment` for the `--extend` file, in which an
  omitted axis is `⊥`. A `Fragment` is consumed only by `Capabilities::extend`
  and can never be pushed onto a stack. `Widen` and its `Option` lift are
  deleted.
- **`every` is a flag on `ExecGrant`, not a key.** Only `join_top` sets it, so
  no surface spelling widens and "every program" has no negative form. It
  compiles to `ExecScope::Any`. `ExecGrant`'s map is private, built only by
  joining.
- **No ranking.** `Table::meet` is exact under three obligations; the witness a
  refusal cites is any exclusion that applies, chosen deterministically, never
  the deepest.
- **Carriers and the dynamic linker are support**, joined as Allow where the
  author wrote no allowance, yielding to every exclusion on either axis.
- **Activation is denotational.** A layer restricts exactly where some axis
  does not denote `t`; an `every` table with no exclusion restricts nothing.
- **Allowances are frozen; exclusions re-resolve.** Both enforcers refuse a
  redirected allowance rather than follow it. Freezing refuses a real path that
  is not Unicode. Freezing synod's guest grant once per session is deferred: today it is minted per run by `FrozenPath::from_guest`.
- **Writing reads, at the layer. Running reads, at the stack.** A layer reads
  its own read and write prefixes and the run places of the exec meet over
  itself and the layers beneath it.
- **ral's binary is an ordinary host file**; a bundled tool reads it. Both
  kernels' admissions of ral's own image are deleted.
- **exarch's self layer.** An exec exclusion of exarch's own binary, pushed
  wherever exec restricts.
- **`--extend-base` is `--extend`.** The flag names the operation, `◁`, and no
  longer suggests that it only widens. No alias; clap's own suggestion answers
  the old spelling. The help text says what the rules say: the file's allowances
  and exclusions join the base before any `--restrict`; an allowance never
  removes permission; an exclusion always wins; an area the base leaves
  unrestricted stays so, less the file's exclusions. No refusal hint suggests
  `--extend` against an exclusion.

### The decisions reached, with their rationale

1. **Running reads, at the stack.** Pairing per bundle made a restriction that
   supplies fs alone hide every program the enclosing grant runs outside its
   reads and the floor: a restrict file reading only `` `cwd . `` stopped
   `cargo` in `~/.cargo/bin`. A grant now reads what it and the grants
   enclosing it let run, so an fs restriction hides such a place only by
   excluding it. Pairing at the *whole* stack would let a nested grant make
   readable, and so runnable, what its ceiling withholds, merely by naming it
   in exec; looking *down* the stack keeps restriction attenuating. The stack
   is an ordered elaboration of layers, then the pointwise meet. Rejected:
   pairing per bundle; pairing at the whole stack; a kernel-only read grant
   (the enforcers disagree); "running grants reading" over the whole exec axis
   (`every` names no place, so it grants none); "running requires reading" with
   separate entries, which writes every toolchain twice.
2. **No write freeze.** See below.
3. **ral's binary is an ordinary host file, and a bundled tool reads it.** An
   early design exempted it from every exclusion on the premise that the kernel
   must admit it to run bundled tools. A bundled tool runs inside the confined
   child, which ral starts before confinement, so the exemption served only a
   *nested* ral. Making ral's binary support was worse: a nested ral boots with
   a fresh stack and runs every bundled tool whatever an exec allow-list omits
   (`ral --ral-bundled-tool mv` needs no stack at all), so every grant that
   restricted exec leaked the bundled toolset, and exarch's configuration frame
   (`Capabilities::deny_all`, an empty exec grant that must run nothing) would
   have admitted the current executable. So ral's binary runs where a grant
   admits it, by path, directory or name, and is read through pairing. A bundled
   tool does read ral's image (`/proc/self/exe` on Linux; the AppContainer's
   image on Windows, where the deny walk defeats any exception), so an fs
   exclusion of ral's binary refuses every bundled tool on every platform, and
   an exec exclusion leaves them alone. `reasonable` admits `~/.cargo/bin/`,
   where a cargo install puts ral; a nested ral there is bound by the kernel
   alone, a guardrail like every other: "so be it, there's nothing we can do".
   Nothing legitimate needs ral's image admitted inside confinement: bundled
   tools run in-process in the confined child, pipeline anchors are spawned
   unconfined by the outer shell, and on macOS the confined child starts
   unconfined and calls `sandbox_init` itself; the kernels' own comments gave
   the admissions' purpose as "a ral run inside". History: the first draft
   exempted ral's binary from every exclusion; the third review found the
   exemption served only a nested ral, and made ral's binary support, run and
   read unless excluded, so that the shell always ran; the fourth showed a
   bundled tool still reads ral's image, so an fs exclusion of it refuses every
   bundled tool (that part stood); the seventh took support away, for the
   reasons above. The draft's account of how the guard read ral's binary was
   also circular (the floor "held" it, and it got there "through the floor"),
   and the floor, a static list of system prefixes, held nothing of the kind.
   Rejected: ral as support, run and read unless excluded (the shell always
   ran, but every grant that restricted exec leaked the bundled toolset by
   default, and `deny_all` stopped meaning no exec); a built-in exclusion of
   ral's image wherever exec restricts (special in the other direction, and
   wherever exec was otherwise every program it forced the Linux lowering of
   `Any` and its limits onto every restriction; a cargo-installed ral is not
   worth that); the early exemption and its variants (keeping it with Linux
   repaired to reach ral only by descriptor; keeping bundled tools exec-only,
   which needs a Windows-only refusal and a declared Linux read and re-exec of
   ral; a Windows deny walk that spares the image's ACE, new surface on shared,
   persistent stamps); ral in the fs floor (two concepts for one file, and a
   per-process, often user-owned entry in a static system list). Deferred, each
   its own plan: a nested ral that inherits its parent's grants, the only
   mechanism that keeps guard-only rules across nesting, and `ral` as a builtin
   of ral, which keeps them only for a direct invocation.
4. **`homebrew`.** `system` loses Homebrew; `` `dir `homebrew `` joins `path`
   and `system`. Other package managers wait for a design of their own: their override rules are not settled, and only Homebrew is needed to drop `minimal`'s exclusion. Selectors for the rest would also replace `reasonable`'s hard-coded toolchain directories (it ignores `CARGO_HOME` and `PYENV_ROOT`; Volta, pipx and uv targets resolve outside the listed directories), whose duplicate read entries stay until then.
5. **Writing reads.** Both kernels already read every write prefix; the guard
   now agrees. Rejected: declaring the discrepancy a kernel limit, which would
   have left the running law false of the kernel for a layer that writes
   without reading.
6. **Extension keeps run allowances beside `every`.** Where the base leaves exec
   unrestricted, `t ⊕ Q` sets `every` and keeps `Q`'s allowances; they restrict
   nothing but still name run places, resolved afresh like every other name.
   Rejected: pairing the fragment once at load, whose snapshot left a
   name-allowed program unreadable after its file moved mid-session; resolving
   every name once at push, which breaks a name-allowed program across an
   upgrade until restart.
7. **No scope capacities, no launch-time carrier check.** A `Name` veto is built
   only from negative evidence and `Any` only from `every`, so each has one
   polarity by construction; masking by capacity was a second mechanism for a
   fact already fixed. The kernel refuses an excluded carrier anyway; the
   interpreter hint, told the exclusion through the warrant, words it. After a
   refused `execve` the child used to advise allowing the script's interpreter,
   which is wrong when an exclusion covers it, and the child holds no policy to
   tell; discovery cannot tell either, since it skips a script the user can
   write. So the parent reads the `#!` line once more, for diagnosis alone and
   never as evidence, and records the covering exclusion in the warrant's
   `Run::File` (one optional field; both ends are one pinned binary, so the
   version string changes and no old form is decoded). No dependency resolver is
   built.
8. **Windows reads what runs.** Windows receives the read region with its places
   and stamps it. Rejected: keeping run places out of its projection, which
   leaves Windows stricter than the guard for descendants.
9. **Allowances are frozen; exclusions re-resolve.** A symlink redirect was
   confirmed on Linux to read and write outside the grant in both enforcers.
   Rejected: re-freezing allowances (the redirect); freezing exclusions too (a
   new spelling would escape them); *shaped* places, a file read exactly, which
   made the fs meet need shaped probes for nothing a write to the file's parent
   could not already reach. A run place reads as a prefix. Every allowance renders as its frozen real path alone: a surface spelling is never a predicate of its own, since a name that was a symlink at freezing can later hold a real directory the grant never named. Synod's guest grant is the unfinished half (see *As built*).
10. **exarch excludes its own binary wherever exec restricts.** Alex: "avoid
    exarch running ral running exarch; not sure why you'd re-exec exarch". A
    nested exarch is an agent outside the session's stack: `exarch
    --ral-bundled-tool mv` runs with no grants at all. Nothing the agent
    legitimately does re-execs exarch, so wherever some layer's exec restricts,
    `for_invocation` pushes an exec exclusion of the binary, by the path it was
    pinned from, which re-resolves like every exclusion. It is the credential
    deny's rule, on exec. The layer was first pushed in every session,
    `dangerous` included, so that a spawn, which forks the parent's stack and
    pushes one layer without passing through `for_invocation`, could not miss
    it. That made `dangerous` restrictive, so every launch went through the OS
    sandbox, and on Linux a host without bwrap, such as every Terminal-Bench
    container, ran no external command at all (exarch-bench, 2026-10-18).
    Costs accepted: a child that a `dangerous` parent's spawn grant narrows
    can start exarch, regaining the parent's unconfined authority, which the
    parent holds anyway, so nothing escalates; the same child can read the
    credential files. Where exec restricts, on Linux a program created during
    a launch directly on the spine of exarch's binary waits for the next
    launch. Windows pays nothing (exec is enforced only in process). synod
    builds its own stack and has no self layer. The earlier self layer also
    vetoed `exarch` and `ral` by name; the layer names that binary alone, so
    another install of either, in a folder a grant admits, runs. Rejected:
    vetoing `exarch` and `ral` by name (another install is an ordinary file,
    and name vetoes are advisory beneath writable directories); pushing it in
    every session (it confines `dangerous`, which then needs an OS sandbox the
    host may not have); making the exclusions a part of the stack met in on
    each axis wherever any layer restricts it, at any depth (it closes the
    spawn gap, but gives `GrantStack` a second component for a gap that
    escalates nothing); no self layer (shedding a restricted session's grants
    through a nested exarch should take more than one command). It is built as
    `deny_layer` is, by `join_top` over a fragment of one exclusion, and is
    exec only, since an fs exclusion of exarch's binary would refuse every
    bundled tool.
11. **Windows refuses the universal prefix.** `FrozenPath::root` means every
    path on every volume; stamped as a path it becomes the current drive. A
    launch whose fs projection holds it is refused before anything is stamped;
    the guard still judges. The shape arises only in exarch, under `dangerous`
    with an fs exclusion from `--extend` or a deny-only restrict file. Rejected:
    ambient grants plus the deny walk (the session would break silently);
    stamping every fixed volume (persistent ACEs over every file); today's
    narrowing to one drive.
12. **Deputy findings stay in the audit trail.** They are recorded only into an
    open trail, which exarch's ordinary dispatch never opens. The documentation
    names the route: `ral --audit --capabilities <profile> -c true`, which pushes the profile as a session layer under an open trail, so the `deputy` checks are recorded (the plan had named `audit { grant … }`; that is not the route). The findings are recorded only by `Shell::audit_deputy_prefixes` on pushing a session layer; exarch's ordinary dispatch opens no trail and its cards suppress flagged checks. Alex chose to leave them there and document the route. Rejected:
    the agent's prompt summary (it would tell the agent where to plant programs
    the user later runs outside any sandbox); exarch's banner, recommended and
    not taken, so the limit is seen only by an author who runs the audit.
13. **Freezing refuses a real path that is not Unicode.** A frozen path holds
    its real form as a `String`; once it is the identity, a lossy form would
    grant a sibling whose name holds U+FFFD. Rejected: real forms as bytes,
    which every textual consumer would refuse anyway.
14. **Names are live per run.** `which::locate` keeps an answer for the rest of a
    top-level run (one submitted line, one script or one dispatch; misses
    re-probed after a second), and command resolution shares it with
    compilation, so the program launched is the program judged. The early
    wording said "at each judgment"; the wording changed, not the code. Rejected: an uncached resolution for
    policy alone, which would walk `PATH` for about a hundred names on every
    exec under `reasonable`.

### How the design was reached

The plan went through seven reviews before it was implemented; the shape of
the semantics is the residue of their findings.

1. **First review.** An earlier draft (written against 94344cac) was replaced,
   and the decisions of the list above were reached with Alex.
2. **Second (Claude, Codex).** The account of the write freeze and of ral's
   exemption was corrected; writing reads were added; `Any` was lowered against
   the envelope's binds; the support set stayed projection data; the
   package-manager catalogue moved to its own plan.
3. **Third (Claude, Codex, Fable).** No kernel judges an exec of ral for a
   bundled tool, so ral's binary became ordinary support; an extension's run
   allowances were kept beside `Any` so that their reads resolve afresh; support
   moved out of restricted exec; scope capacities and the launch-time carrier
   check were dropped; every directory admission was lowered against the binds;
   hard links and the existing fs limits were declared.
4. **Fourth (Claude, Codex, Fable).** Windows' deny walk defeats any `ral.exe`
   exception and a bundled tool reaches its own image through `/proc/self/exe`,
   so a bundled tool reads ral's binary; the read pairing of run allowances
   moved from the bundle to the stack, for ergonomics; frozen identities stayed
   frozen through rendering; `Any` became an authored-free flag, `every`, with
   no negative form; activation became denotational; support got one cross-axis
   rule; the remaining Linux and Windows limits were declared.
5. **Fifth (Claude, Codex).** Confirmed on Linux that a re-resolved allowance
   can be repointed by a symlink to read and write outside the grant, so
   allowances are frozen and only exclusions re-resolve; pairing looks down the
   stack, since pairing at the whole stack let a nested grant widen reading; a
   run place kept its file-or-directory shape; frozen places were carried
   through Windows stamping and its projection cache; ral's binary was given
   support in the guard's judgment; directory lowering was restored for alias
   binds; the self layer was stated as the guardrail it is.
6. **Sixth (Codex).** Shaped places broke the fs meet and Windows' capability
   names, so places read as prefixes; a surface spelling must never be an
   allowance of its own; Windows must stamp the object it checked, with no
   reparse point on its path; exec exclusions must re-resolve too; synod's grant
   must be frozen once per session, not per run; lowering must cover canonical
   sub-binds; the stack is an ordered elaboration before the meet; hard links
   reach past any path grant.
7. **Seventh (Claude, Codex).** ral's binary became an ordinary host file,
   neither support nor excluded; exarch's self layer shrank to its own binary,
   pushed in every session; freezing refuses a real path that is not Unicode;
   names live per run; Windows refuses the universal prefix; deputy findings
   stay in the audit trail; the engine protocol bumped; summaries keep layers
   that name run places.

### As built

Four places where the implementation departs from the plan, recorded as they
stand:

- **Windows refuses only redirecting reparse points.** A frozen allowance whose
  path meets a name-surrogate reparse point (symlink, junction, mount point) is
  refused; other reparse points, cloud-files placeholders among them, pass.
  `OBJ_DONT_REPARSE` would have refused every reparse point. Components are held
  root first, each opened relative to its parent's handle with
  `FILE_OPEN_REPARSE_POINT`, the tag read on the held handle before the walk
  descends (Alex's decision; `sandbox/windows/dacl/frozen.rs`).
- **The audit route is `ral --audit --capabilities <profile> -c true`**, not
  `audit { grant … }`.
- **A floor entry that cannot be walked refuses the floor.** The platform floor
  is not authored, yet a host that cannot resolve it, a non-Unicode name
  included, cannot run anything: boot fails, naming it (`floor_in`).
- **`PROTOCOL_VERSION` is 17**, and the warrant's magic is `ral-warrant/2`.

Freezing synod's guest grant once per session is deferred. synod sends its grant
with every run and the guest pushes it as layers, so freezing per run would let a
redirect made in one run take effect in the next. The design: an unfrozen guest
grant is its own type, which the guard cannot judge by (today
`FrozenPath::from_guest` copies the surface into the real form); the guest
engine freezes a grant the first time it arrives and keeps the frozen ceiling
resident for the session, keyed by the unfrozen grant, so a changed grant is
frozen when it first arrives; a child inherits resident layers already frozen and
never re-freezes them.

## What the lost write freeze costs

The freeze made an exec allowance withdraw write authority, in the kernel alone:
under a restricted fs it made every run-admitted directory and exact file that a
broad write covers without naming it read-only, exclusion or not; under an
unrestricted fs it froze the whole admitted set once an exclusion existed. No
Belnap reading keeps that, since it turns positive evidence on one axis into
negative evidence on another; it breaks rule 3, the extension invariant and the
guard/kernel contract.

What it protected becomes a **declared limit**:

- A broad write that covers a run-admitted directory lets a child author
  programs there. Where that directory is on the user's own `PATH`, the program
  later runs outside any sandbox. `edit-only` launched from `~` is the shipped
  case: it runs `` `dir `xdg-bin . `` and writes `` `cwd . `` without running
  `` `dir `cwd . ``, so `~/.local/bin` is covered, was frozen, and is now
  writable.
- Name vetoes beneath such a directory lose their kernel walk too: they are
  advisory to descendants, as they already were beneath writable directories and
  beneath `Any`.
- An exclusion never stopped a copy placed in a folder that is both writable and
  runnable, a program written there, or a script fed to an admitted interpreter;
  nothing here pretends otherwise.
- The deputy findings name every such folder, but only inside `audit` (decision
  12).

The remedy that fits these semantics is a *write-only exclusion*: authored
negative evidence on writing alone. fs has none (`deny_paths` refuses every
operation) and adding one is its own design. Rejected: restoring the
protection now as write-only exclusions (new surface on both backends); keeping
the freeze as a declared exception to rules 2 and 3.

## Consequences

Policies that relied on a carrier, loader, exact file or deeper directory
reopening an exclusion now fail, and the refusal says that an allowance cannot
override an exclusion. No migration adds a backend exception to an authored
exclusion, and no exclusion is deleted mechanically to recover an old success:
a further dependency on a reopened exclusion, found in a fixture or in synod, is
reported before its permissions change.

**Behavior changes**, each explained for users in `docs/SPEC.md` and
`exarch/PROFILES.md`:

- **Exclusions win.** An exact file, deeper directory, carrier or dynamic
  linker inside an exec exclusion no longer runs.
- **Running reads.** A program that a grant or an enclosing grant runs by path,
  directory or name is readable in the guard, as it already was in the kernel,
  unless an fs exclusion covers it; an fs-only restriction does not hide the
  tools an enclosing grant runs (to hide one, exclude it); a nested grant never
  makes anything readable by naming it in exec; an fs exclusion of a run place
  refuses running it in the guard too; a program that runs only through `every`
  must be readable under fs.
- **Allowances keep what they named.** No allowance by path follows a symlink put
  in its place later; exclusions still follow their names and a bare name still
  follows `PATH`, afresh at each run. A grant whose real path is not Unicode is
  refused when frozen. Creating a granted directory after the grant, or
  `rm -rf dir && mkdir dir`, keeps working (path grants, not inode pins).
  Replacing a granted directory, or an ancestor, with a symlink refuses every
  launch that shows it on Linux and Windows, naming it, and reaches nothing on
  macOS. A symlink spelling that existed at the grant and is retargeted later
  keeps showing its old target. macOS firmlinks keep working, as twins of the
  real path. exarch's `` `cwd `` anchor stays where exarch was started.
- **Writing reads.** The guard now reads every write prefix, as both kernels did.
- **No write freeze.** See above; `edit-only` launched from `~` can write
  `~/.local/bin`.
- **Extending an omitted axis** is a no-op for a positive-only fragment, apart
  from the reads of what its run allowances name; for a fragment with exclusions
  it yields every request less them. `--base dangerous --extend git.exarch.ral`
  stays unrestricted apart from exarch's self layer.
- **Every program, less the exclusions.** On Linux a program created during a
  launch on the spine of an excluded path does not run until the next launch,
  and under a restricted fs the envelope's private `/tmp` runs nothing.
- **Activation** reads what a policy means: a layer that restricts nothing no
  longer activates launch hardening, and exarch pushes its credential layer only
  where some fs restricts (`--base dangerous --extend` with a positive-only fs no
  longer gets one; with an fs exclusion it does).
- **Verbs** join by union under extension; a base can no longer withhold a verb
  (`detach` included) against a fragment.
- **Omitted exec, restricted fs.** A program that runs only through `every`, or
  with exec omitted, and lies outside the fs read region is now refused in the
  guard (`ExecDenial::Read`). For the launched program this is a tightening, not
  alignment (Linux and Windows showed or granted the launched image itself); a
  descendant outside the reads was already refused on Linux and macOS.
- **ral itself.** ral runs exactly where a grant admits it. Under an exec
  allow-list that does not admit it, `ral -c`, `#!` ral scripts and
  `sh -c 'ral …'` are refused, and so is the current executable under exarch's
  no-authority configuration frame (which relies on an empty exec grant running
  nothing); under one that admits ral's folder a nested ral runs, bound by the
  kernel alone. An fs exclusion of ral's binary refuses every bundled tool; an
  exec exclusion leaves them alone.
- **exarch's agent cannot start exarch** wherever exec restricts; `dangerous`
  is ambient authority.
- **Windows refuses "every path, less exclusions"**: in exarch, `dangerous` with
  an fs exclusion from `--extend` or a restrict file. Today's stamping of the
  current drive's root ends.
- **Interpreters under fs-only restrictions.** A trusted interpreter outside the
  reads and the floor now starts its scripts under a restriction that supplies fs
  alone, as it already did under a restricted exec.
- **Deputy findings** name every writable island under a runnable folder and no
  longer name a folder wholly excluded on either side; read through
  `ral --audit --capabilities <profile> -c true`.
- **Refusals** cite an exclusion that applies, chosen deterministically, and name
  the missing permission.
- **Selectors.** Homebrew leaves `system` for `homebrew`; the read floor keeps
  it, at whatever prefixes `homebrew` finds.
- **The flag.** `--extend-base` is `--extend`, with no alias.

**Profiles.** The only bundled exclusion meant to be reopenable was `minimal`'s
`/opt/homebrew/`; the others are name vetoes that already won. `minimal` broke the
wiki's own rule ("deny only what is truly off-limits; leave 'not included, but
fine to add' unlisted") and could not follow it while `system` folded Homebrew in;
on Linux it already admitted Linuxbrew through `system`, so its promise was broken
there too. Now: `reasonable`, `confined`, `edit-only` and `read-only` add
`homebrew`, keeping today's admissions; `minimal` drops its exclusion and says
brew tools are added by `--extend`; `edit-only`'s exec comment, which claimed no
directory it can write is admitted (its `git` veto leaned on that), now says it
holds only where `` `cwd . `` covers no admitted directory, gives the audit route
to its deputy findings; the stale remark in `git.exarch.ral` that `confined` does
not cover Homebrew is corrected; profile comments that sent readers to
`path/sigil.rs` name `guard/freeze.rs`.

## Where

`core/src/authority/policy/{evidence,table,exec,fs,lattice,deputy}.rs`,
`core/src/authority/guard/enforce.rs`, `core/src/authority/grant/{freeze,decode}.rs`,
`core/src/authority/sandbox/projection.rs`
and the three backends, `core/src/load/profile.rs` (`Fragment`),
`exarch/src/policy.rs` (`for_invocation`, the self layer).

**Amended 2026-10-22 by [[decisions/261021_one-path|one-path]].** The denotation
above is unchanged; its representation is. A `Table` is two sets, one polarity
each (allowances and exclusions); a key cannot hold `Both`, because evidence is
computed at the access and never stored. The scopes are resolved where a layer
is decoded: an allowance holds a position (`Extent`), an exclusion the objects
its walk held (`Excluded`), and nothing is re-frozen at a check. "An exclusion
re-resolves" therefore reads: an exclusion names the object its spelling reached
at decode, and also holds its position, so a new name for that object and
whatever is put at its old name are both caught. `FsRefusal` is `Refused`
(`Guarded`, `Withheld`); `ExecScope` and `FsScope` are the two `Table`
instances' key types, `ExecAllow`/`ExecDeny` and `FsAllow`/`FsDeny`.
