# Capabilities and `grant`

**`grant` is a dynamic-context operator that attenuates authority for its body by
*intersection*.** `grant [exec: [...], fs: [...], net: `offline] { body }` narrows the
commands, filesystem paths, and network access available to `body` and
everything it calls. Authority is never amplified:

- a dimension omitted from a grant inherits the ambient authority;
- a dimension present can only narrow it;
- nested grants compose by meet, exclusion-wins: each of `fs` and `exec` is a
  table of scoped rules denoting a function from requests to Belnap evidence,
  and the meet is the pointwise meet of evidence
  ([[design/authority-tables|authority-tables]]), the algebra read against the
  literature in [[related/access-control-algebra|access-control-algebra]];
- an exclusion is anti-monotonic: further layers can add exclusions but never
  reopen an excluded region, and no specificity does either: neither an exact
  file nor a narrower directory, nor a carrier, the dynamic linker or ral's
  own binary.

**A deny's reach is the content, not the spelling.** "Never reopen a denied
region" must hold for what a deny protects, not just the string naming it: a
writable region is otherwise mutable end to end, so a confined child that
renames or removes a directory on the path to a denied file can make its bytes
resurface under a name no deny rule covers, without any layer ever re-granting
the denied path itself. The invariant is sharper than "the literal path stays
blocked": no confined child can cause a denied path's contents to become
reachable under a name the deny does not cover. Enforcing it is not free —
some name on the path to a denied file stops being renameable or removable,
and what that costs differs by backend — see
[[internals/holding-a-deny|holding-a-deny]] for the rendering
and the per-backend price.

**The axes are independent:** each is a separate field whose absence in a
*policy* is `t`, the meet identity (`None` = inherit,
[[map/core/shell-state|`Option<T>: Meet`]]).

- **Restricting one axis does not restrict another.** `grant [fs: [...]] { … }`
  narrows the filesystem but leaves `net`, `exec`, `editor`, and `shell` at the
  caller's authority; a grant that means to confine network access must say
  ``net: `offline`` itself.
- **`net` has no in-process guard.** ral has no network primitives, so
  ``net: `offline`` is enforced only by the OS sandbox — it fails closed where no
  backend exists.
- **The process view is the envelope's, on every projection.** No axis names
  it: a child confined for any reason sees and can signal only the envelope's
  own processes, so `ps` and `kill` under any grant reach the envelope alone,
  and a survivor `detach`ed under one grant is invisible from inside the next.
  On Linux this is a pid namespace wherever the host can build one; where it
  cannot, the launch runs and the host fact is reported
  ([[decisions/260906_the-envelope-is-a-process-namespace|the-envelope-is-a-process-namespace]]).

This is a deliberate mental-model fact, recorded so omission is never mistaken
for an implicit cross-axis deny: an omitted axis is `t`, the identity of `meet`.
Activation reads what a policy *denotes*, not what it spells: a layer restricts
exactly where some axis does not denote `t`, so `every` with no exclusion, or an
fs that writes the root and excludes nothing, restricts nothing and activates no
launch hardening.

## Selectors, refusals and summaries

- **A selector is a set of locations, and the table is one file.** The names are
  `path`, which reads `$PATH`, and `enum Toolchain` in
  `core/src/guard/toolchains.rs`: `system` and every package manager, each a
  few rows of data (`Location { overrides, defaults }`), checked complete by one
  exhaustive `match`. The rules below are stated here once; the table is the
  data, and no page copies it. `FreezeCtx::selector` dispatches to it, and
  `fs::floor` calls the same live expansion over the same rows.
- **Choose, then keep what exists.** A location's *overrides* are host
  environment variables with an optional subpath (`$CARGO_HOME/bin`), tried in
  order. The first usable one (non-empty and absolute under the host's rules;
  an empty or relative value is skipped, as `path` skips a relative entry) is
  all the location has; with none, every default is chosen. Then only
  directories that exist are kept. An override that chose a missing directory
  yields nothing and does not fall back to the defaults: the environment said
  where the manager is, so falling back would grant a directory it disowned. An
  absent manager expands to nothing, never an error. `GOPATH` alone holds a
  list, and only its first entry counts.
- **Overrides are the host's.** They are read from the host process
  environment, as `path` reads `$PATH`, never from a session's variables and
  never from a tool's own configuration files (cargo's `install.root`, an
  `.npmrc` `prefix`; a user who relies on them writes `` `dir `absolute ``).
  exarch redirects `CARGO_HOME` and `GOPATH` into the scratch inside a session;
  that must not move a profile's grants. This is why the table lives in core and
  not in helpers inside the profiles, which would read the shell's variables.
  The home comes from the `FreezeCtx`; a home default with no bound home is
  absent. A `‹config›`, `‹data›` or `‹cache›` default resolves as the matching
  `xdg-*` place does (`resolve_xdg_in`). A default that is not absolute under the
  host's rules belongs to the other platform and is skipped, so no row carries a
  platform mark and one profile loads on both.
- **A location names what runs, and holds no secret.** It is what a manager
  installs and its shims dispatch into, and running reads make it readable, so
  where a root also holds credentials or user data the location is the narrowest
  subtree that holds what runs (`cargo` runs `bin`, not `credentials.toml`;
  `stack` runs `programs`, not `upload/`). `~/.dotnet` is the one exception,
  because .NET keeps private keys under `~/.dotnet/corefx` wherever it is
  installed and its runnable parts include a file; `reasonable` excludes that
  subtree. Caches and registries stay `fs` entries.
- **No override names the world.** An override whose chosen directory, after its
  subpath, is the filesystem root, the home or an ancestor of it is a freeze
  error naming the variable and asking whether it is set correctly: running
  reads would make the selector a read of everything, `~/.ssh` included. The
  check is on the directory, not the variable (`GOPATH=~` chooses `~/bin` and is
  fine), by containment of symlink-followed forms. Defaults are fixed and need no
  check. `path` has the same hazard and the same guard, one function
  (`refuse_the_world`, in `freeze.rs`): a `$PATH` entry naming the world is a
  freeze error naming `PATH`, not a silent skip like a relative entry.
- **`system` is a row, and `homebrew` is another.** `system` is the platform's
  own tool roots and no longer holds Homebrew. `` `dir `homebrew `` is
  `HOMEBREW_PREFIX` when usable, else `/opt/homebrew` or Linuxbrew's
  `/home/linuxbrew/.linuxbrew`, where they exist. Intel macOS's `/usr/local` is
  reached only through `HOMEBREW_PREFIX`, since that prefix is shared with
  everything else. The root is the whole prefix, so a formula's `bin` link and
  the `Cellar` file it resolves to are both admitted, and readable by running.
- **Four rows are on the read floor.** `homebrew`, `macports`, `nix` and
  `pkgsrc` are system-wide prefixes outside any home, world-readable trees of
  the standing of `/usr`, and a program from one maps its libraries from its
  prefix wherever its admission came from. They join `fs::floor`. No home-based
  row does: a home tree is the user's, and reading it must be granted. An
  override can still choose one (`HOMEBREW_PREFIX` on a home Linuxbrew), so the
  floor keeps only chosen directories outside the process's home, the floor
  having no `FreezeCtx`; such a prefix is run, and read by running, wherever
  `homebrew` is granted, and is not floored. A floor directory whose walk
  fails refuses the floor, so boot fails and names it; an override naming the
  world or the home is no floor and no error. The floor is an allowance like
  any other, so it renders frozen and a redirected root shows nothing.
  An exclusion means "not allowed", never "not included": a profile that merely
  leaves something unlisted does not exclude it.
- **A refusal says why, in one of a few shapes.** *Missing allowance* names the
  permission (reading, writing or running) and, only where more than one layer
  supplies the axis, says an enclosing grant may be the one withholding it and
  that a nested grant cannot add authority its caller withholds; it never
  suggests an allowance in the innermost grant would help. *Excluded* names the
  witness and says that no allowance overrides it, and points at the policy that
  imposes it, never at `--extend`. *Unreadable program* says the program may run
  but what running reads is withheld, and for a bundled tool names ral's binary
  and the exclusion covering it. *Redirected allowance* names the link and says an
  allowance names what it named when granted, and how to grant the new target.
  *Respelled* keeps the explanation for an exclusion reached through case, Unicode
  or a filesystem alias.
- **Summaries list what was authored.** exarch's prompt, the grant legend and
  synod's reused summaries list allowances and exclusions as written and compute
  no effective permission per key (for bare names that would need host
  resolution). A key with both bits appears in both lists, and the legend says
  exclusions win. `every` reads "every program, less the exclusions below";
  allowances beside it add no running but still make what they name readable.
  A layer is summarised where it restricts *or* where its exec names a run place:
  a ceiling of `every` beside `File(s)` with fs omitted denotes `t`, yet under a
  nested fs-only restriction it makes `s` readable, so the reader needs to be
  told. "Activates confinement" and "says something the reader needs" are two
  predicates. The self layer is summarised like any exclusion: "every program,
  less the exclusions below", exarch's binary under `exec deny`.

## Policy, extension and the six rules

**Authority is a ceiling, extended once, then only restricted.** The ceiling is
a base extended by at most one fragment (`--extend`, `B ◁ F`); every later
layer (restrict files, credential layers, spawned children, `--capabilities`,
`grant` blocks) restricts. Within ral there is no extension at all
([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).

- **Roles are types.** An omitted axis denotes the identity of the role it
  plays: `t` in a *policy* (a base, a restriction, the stack), `⊥` in an
  *extension fragment*. `Capabilities` is a policy; the loader returns a
  `Fragment`, consumed only by `Capabilities::extend` and never pushable onto a
  stack. Reading an omission in the wrong role is a type error.
- **In the extension an allowance never removes authority; only an exclusion
  does.** The extension invariant states it of policy denotations: where `F` has no negative
  evidence at `s`, `B` permitting `s` implies `B ◁ F` permitting it; where `F`
  excludes `s`, `B ◁ F` refuses it; and `B ◁ F` permits only what `B` or `F`
  supports. An axis the base leaves unrestricted stays so, less the
  fragment's exclusions: `--base dangerous --extend git.exarch.ral` is every
  request less the file's exclusions, not "only `git`". Verbs join by union and
  carry no exclusions, so a base cannot withhold a verb against a fragment.
- **Minimal plus git.** `minimal` admits `system` and leaves Homebrew unlisted
  (it is a selector, `` `dir `homebrew ``, not part of `system`); a fragment
  that allows `` `name git ``, loaded with `--extend`, adds `git` and removes
  nothing, and a Homebrew `git` is admitted by name with nothing in the way.

**Running and writing read.** A grant reads what it lets be written, in the
layer that states it (writing reads), and what it, *and the grants enclosing
it*, let run by path, directory or name (running reads, at the stack). A
layer's read region joins its own read and write prefixes to the run places of
the exec meet over itself and the layers beneath it; no layer makes anything
readable for the layers beneath it, so pushing a layer never widens
(restriction attenuates).

- A restrict file that reads only `` `cwd . `` leaves `cargo` in
  `~/.cargo/bin` readable and runnable where the enclosing grant runs it; to
  hide it, *exclude* it.
- A nested grant never makes something readable by naming it in exec: under a
  ceiling that reads nothing and omits exec, a nested `File(s)` makes `s`
  neither readable nor runnable. Order matters: a layer that reads nothing,
  beneath one that runs `File(s)` and omits fs, refuses `s`; the other way
  round permits it.
- A program that runs only through `every`, or with exec omitted, must be
  readable under fs; an fs exclusion of a run place refuses running it, and the
  refusal names the exclusion.
- A bundled tool runs inside the confined child, which runs ral's own image, so
  it reads ral's binary: an fs exclusion over ral's binary refuses every
  bundled tool on every platform, and an exec exclusion of ral leaves them
  alone.

**ral's binary is an ordinary host file.** It runs exactly where a grant
admits it, by path, directory or name, in the guard and the kernel alike; nothing
admits it beyond the policy, and nothing excludes it beyond the policy. A grant
that admits the folder holding ral (`reasonable` admits `~/.cargo/bin/`, where
a cargo install puts it) admits ral. A *nested ral* boots with a fresh stack and
is confined by the kernel alone: whatever only the guard enforces, bundled-tool
rules and verbs among it, does not bind it, so under an exec allow-list that
admits ral, `ral -c 'mv a b'` runs the bundled `mv` the list omits. A grant whose
guard-only rules must hold against a nested ral does not admit it.

**exarch's agent cannot start exarch as a process, wherever exec restricts.**
In such a session exarch pushes as its last layer an exec exclusion of its own
binary, by the path it was pinned from; spawned agents inherit it. It names
that binary and nothing else, so another install of exarch or ral, in a folder
a grant admits, runs. `dangerous` restricts nothing, so it gets no such layer
and needs no OS sandbox: an exclusion there would make every launch sandboxed,
and on Linux a host without bwrap would run no command at all.

The cost of the condition: a spawn forks the parent's stack and pushes one
layer without passing through `for_invocation`, so a `dangerous` parent's
restricted child has no self layer, and can start exarch and so regain the
parent's authority, which the parent holds anyway. Nothing the agent legitimately does re-execs
exarch (its shell evaluates in exarch's process, bundled tools run in-process in
the confined child, pipeline anchors are spawned unconfined by exarch itself), so
the exclusion is exec only: an fs exclusion of exarch's binary would refuse every
bundled tool. Costs: on Linux, a program created during a launch directly on the
spine of exarch's binary (`~`, `~/.cargo`, `~/.cargo/bin` for a cargo install)
runs from the next launch. Windows pays
nothing: exec is enforced only in process and an exec-only restriction projects
nothing there. synod builds its own stack and its guest holds no exarch binary,
so it has no self layer. Like every exclusion it is a guardrail: it does not hold
on Windows, against another install, against a copy in a folder both writable and
runnable (`--ral-bundled-tool mv` runs there without any stack), the dynamic
linker run on one, or, on Linux, a hard link to it at an admitted path.

**Allowances name what they named; exclusions follow their object.** An allowance
keeps the position it was resolved to and is never resolved again, so a symlink
put in its place later redirects nothing and both enforcers refuse a launch whose
allowance now meets a link. An exclusion names the object its spelling reached
when the grant was decoded, the link and its target where it was a link, under
every name it has or gets and at the position it named, whatever is put there;
it follows the object, not what is moved out of it. A bare program name is
resolved where the grant is decoded.

**Exclusions are guardrails, not walls, where writing meets running.** An
exclusion names programs by path and name. A copy, a newly written program or a
script fed to an interpreter is a different request. Support, the dynamic linker
and the carriers a launch needs, joins as an allowance only where the author
wrote none, and yields to every exclusion on either axis
([[design/capability-carriers|capability-carriers]]).

**Capability checks gate four dimensions:**

- **exec** as allowance or exclusion of a target, regardless of its arguments
  — more expressive than orthodox object-capability, since a base profile can
  exclude a name a restrict file never mentions; `every` is the one universal
  allowance, a flag no surface spelling sets, never a key;
- **fs** by read/write path prefix with exclusions;
- **net** as `` `offline `` or nothing;
- **editor** and **shell** as verb lists, where `detach` is the one verb that
  gates a *verb* rather than an action on a resource, and the one that reaches
  no OS profile: it decides
  whether a process the session stops owning may be born at all, never what
  that process may then do
  ([[decisions/260727_detach-under-a-grant|detach-under-a-grant]]).

**Every dimension gates authority; none gates recording.** A grant says what
its body may do, never what may be written down about it: whenever a trail is
open, a *denial* is recorded as a `` `check `` observation
([[design/audit|audit]]) and an allowed check never is. There is no flag to set
and none to clear, so no inner grant can hide from its caller's trail a denial
that caller's authority produced.

Filesystem checks are alias-aware and resolve symlinks, so a directory scoped by
`within [dir: ...]` inside a grant cannot escape its policy. The bundled
coreutils (`cp`, `mv`, `rm`, `mkdir`) route through the same check chokepoint as
the structured primitives, closing the bypass.

The grant body evaluates locally. RAL-owned filesystem effects are checked in
process by `check_fs_op` before the syscall; each external or bundled child the
body spawns is confined under the effective projection — per-command Seatbelt
on macOS and bwrap on Linux, the projection's own AppContainer on Windows,
whose token carries a capability SID per granted path
([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).
A survivor `detach` births inside the body is confined the same way and then
keeps that confinement for life: the projection is frozen at birth, since no
later frame can name the process to widen it. Only the envelope's tie to this
process's death is dropped, which is what makes it a survivor rather than a
receipt for something already killed.

**The dimensions are syntax, and the bracket is the form's.** `grant` takes
its five dimensions written out in its own bracket, the way `case` writes its
arms, and the checker holds each written one to its table. No record type is
built for the bracket, so a dimension that is not written is an absent premise,
not a flag. A misspelled dimension is refused before the program runs, naming
`grant` and its own five; a bundle bound elsewhere (`grant $o { … }`), a spread
and a repeated dimension are parse errors, since each hides which dimensions
are written; a map is no bundle either, a map's keys being data
([[design/records-and-maps|records-and-maps]]); and `[]` is the empty grant
rather than the empty list, the bracket being read by the form. The values are
ordinary terms — ``grant [fs: [read: $paths], net: `offline]`` — and to grant a
dimension only sometimes, lift the condition to the form.

**The values are typed data.** Places and exec targets are tagged variants
(`` `cwd src ``, `` `name git ``, `` `dir `system ``); `exec` and `fs` are
nested records of lists, `net` is `` `offline ``, `editor` and `shell` are
verb lists. The checker ascribes the nested tables too, so a bare string where
a place belongs, a misspelled tag or `net: true` fails before the program
runs, naming the tags on offer. Every list is a set, with repeats idempotent;
extension joins verb lists; a selector (`path`, `system` or a package manager) is valid only under
`dir`; and there is no argument filter
([[decisions/261012_grant-surface-is-typed|grant-surface-is-typed]]). Values
the checker cannot type still reach the runtime decoder, which judges the same
vocabulary.

Why it is shaped so: lists are homogeneous and a tag has one payload type, so
every place is tagged and every anchor takes a subpath (`.` for the anchor), or
a string could not sit beside a tag. A selector is a different type from a place
because its expansion has no containment guard, so a write grant on one would
plant programs where the user's own shell looks. An `absolute` payload is
classified on the raw string, so a path rooted only on the other platform is a
dead grant and one profile loads on both. Nested tables are records (`()` stays
the empty *profile*, and `exec: [allow: []]` the empty table). `chdir` gates
`cd` while `within [dir: …]` is judged by `fs` alone. A path in `fs` can name a
socket and no tag stops a process connecting to it; fs authority and service
authority are separate, and no spelling widens what an envelope forbids.

A `.ral` profile and the inline
`grant` surface are symmetric: both decode through the same walker into one
frozen `Capabilities`, so configuration is the grant value written as source,
not a second schema. That walker resolves every place as it decodes —
[[design/capability-freeze|the freeze boundary]] — so no unresolved form ever
reaches the stack or the IPC wire.

This is the boundary [[map/exarch|exarch]] reuses as its sandbox: each agent
run evaluates under a profile's capabilities pushed onto this same stack.

**Concessions.** Five caveats are inherent to the design, not defects:
- **The sandbox bounds a running session, not what it leaves behind.** It
  bounds what a session reads (credentials, `~/.ssh`), what it damages outside
  the project, and what it runs. It does not stop a hostile session from
  leaving code in the project that the user later runs unconfined:
  `.git/config` (`core.fsmonitor` runs on the next `git status`;
  `core.hooksPath`), `.git/hooks`, ignored build outputs (`node_modules/.bin`,
  `.venv/bin`, `target/`) and build scripts. One such unconfined run reaches
  the whole account. Against that attacker, write confinement only delays it;
  only reviewing the session's work, or running one's own builds confined too,
  closes the path. `.git/config` and `.git/hooks` stay writable because a
  grant's `deny` forbids reads too and git reads its config on every command;
  no write-only exclusion exists yet, which is an open gap.
- **Bare command names are ambient.** A bare exec name like `` `name git `` resolves through
  `PATH`, so it is not a strict object-capability; the mitigation is the base
  profile's deny on shells together with the `fs` deny on writing `` `xdg-bin `` — the
  names that matter are pinned, the rest rest on `PATH` integrity.
- **TOCTOU on path resolution.** The resolver-form and bind-form checks share one
  source, so the OS profile and the in-process check cannot disagree, but symlink
  races inside the admitted set are a known surface, not a closed one.
- **Builds start cold.** No profile writes a cache the user's own tools read.
  This is hygiene, not a barrier against a hostile agent while the project and
  `.git` stay writable: sessions never contaminate each other or the user's
  caches (cargo's unpacked sources, Go's module and build caches, Maven's
  repository, uv's wheels, Bun's cache), and a scratch dies with its session.
  So every cache a session writes lives in its scratch, which is deleted when
  the session ends, and `reasonable` and `minimal` do not write `xdg-cache`; a
  cache shared between sessions was rejected because it grows without bound and
  could not be emptied while another session ran. In-tree build outputs
  (`target/`, `node_modules`, `.venv`) stay writable with `cwd`
  ([[map/exarch|exarch]]'s `Scratch` lists the redirected tool homes).
- **Exec admission is not containment; the projection is.** A prefix that is
  both `exec`-admitted and `fs`-writable reads like an escape hatch — drop a
  binary, the next call admits it — but inside one projection it escalates
  nothing: whatever the confined process writes and then runs is spawned under
  the very projection that admitted the write, so it can do only what its
  author could already do. Copying a denied binary under a fresh name defeats
  the name veto for the same reason and to the same small effect. The overlap
  is therefore not merely tolerable but *required* — `cargo build &&
  ./target/debug/app` is exactly this shape, and every bake-in profile makes
  `` `cwd ``, `/tmp`, and `` `tempdir `` both writable and exec-admitted on purpose.
  There is no write freeze: an exec allowance never withdraws write
  authority, on any axis. A broad write that covers a run-admitted directory
  (``write: [`home .]`` over `~/.cargo/bin`, or `edit-only` from `~` over
  `~/.local/bin`) lets a child author programs there, and a bare-name veto
  beneath a writable directory or `every` is advisory to descendants. That is a
  declared limit, not a defence ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]
  records what it costs). `capability::deputy_prefixes` reports the folders that
  stay both writable and runnable, into an `audit` trail only (read them with
  `ral --audit --capabilities <profile> -c true`), and never denies; *no prefix is both* is not an invariant this design wants.

  What does bite is **authority that outlives or exceeds the projection**: a
  write escalates when whoever later treats those bytes as code is not confined
  by the projection that admitted the write — running after the session ends
  (*outlives*) or beside it with more authority (*exceeds*). `` `xdg-bin `` is the
  case the base profiles decide correctly for the wrong stated reason. It must
  stay unwritable not because `exec` also names it, but because a program left
  on the human's `$PATH` is run tomorrow by the human, unconfined.

**An open class: the unconfined reader.** Every dimension `grant` gates names
what the *confined* process may do. One family of escalations has no such
shape: a write into a region that some process the host runs later —
unconfined — treats as code. A hook under `` `gitdir ``, a `core.pager` or an alias
in `.git/config`, an `.envrc`, a `package.json` script, a `Makefile`. None is a
binary, none is exec-admitted, and each runs with the user's full authority the
moment the user reaches for the ordinary tool that reads it. The `exec`
dimension cannot see this: a file that is never executed, only interpreted, is
invisible to any check on execution. The default profiles make `` `cwd `` and
`` `gitdir `` writable, so the class is live rather than hypothetical.

Nothing in this vocabulary expresses it, because the question is not what the
confined process may do but what an unconfined one does afterwards with what
the confined one wrote. Perimeter sandboxes do not answer it either: `nono` and
its kin draw the boundary at the confined process, which puts the later reader
outside by construction. The only lever available today is an `fs` deny on the
particular files, a defence at the name layer with the name layer's limits.
Naming the class as open is worth more than pretending the `exec` dimension
covers it.

See also [[design/syscalls-are-effects|syscalls-are-effects]] (a capability is permission over the effect set),
[[design/scoping|scoping]], [[design/control-operators|control-operators]],
[[design/two-enforcers|two-enforcers]], [[related/system-c|system-c]] (the
type-based pole of this calculus),
[[related/access-control-algebra|access-control-algebra]] (the security-literature
model this lattice instantiates: a fragment of Bruns and Huth's PBel over
Belnap's FOUR), [[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]].

**Realised in** [[internals/capability-enforcement|capability-enforcement]].

Cite: RATIONALE §"`grant` attenuates authority",
§"Lexical data, dynamic authority"; `docs/SPEC.md` §12.
