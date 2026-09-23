# Capabilities and `grant`

**`grant` is a dynamic-context operator that attenuates authority for its body by
*intersection*.** `grant [exec: [...], fs: [...], net: ...] { body }` narrows the
commands, filesystem paths, and network access available to `body` and
everything it calls. Authority is never amplified:

- a dimension omitted from a grant inherits the ambient authority;
- a dimension present can only narrow it;
- nested grants compose by meet;
- a deny is anti-monotonic — further layers can add denies but never reopen a
  denied region.

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
[[internals/capability-enforcement|capability-enforcement]] for the rendering
and the per-backend price.

**The axes are independent:** each is a separate field whose absence is the meet
identity (`None` = inherit = ⊤, [[map/core/shell-state|`Option<T>: Meet`]]).

- **Restricting one axis does not restrict another.** `grant [fs: [...]] { … }`
  narrows the filesystem but leaves `net`, `exec`, `editor`, and `shell` at the
  caller's authority; a grant that means to confine network access must say
  `net: false` itself.
- **`net` has no in-process gate.** ral has no network primitives, so a
  `net: false` is enforced only by the OS sandbox — it fails closed where no
  backend exists.
- **The process view is the envelope's, on every projection.** No axis names
  it: a child confined for any reason sees and can signal only the envelope's
  own processes, so `ps` and `kill` under any grant reach the envelope alone,
  and a survivor `detach`ed under one grant is invisible from inside the next.
  On Linux this is a pid namespace wherever the host can build one; where it
  cannot, the launch runs and the host fact is reported
  ([[decisions/260906_the-envelope-is-a-process-namespace|the-envelope-is-a-process-namespace]]).

This is a deliberate mental-model fact, recorded so omission is never mistaken
for an implicit cross-axis deny: an omitted axis is ⊤, the identity of `meet`.

**Capability checks gate four dimensions:**

- **exec** over a three-valued lattice (Allow / Subcommands / Deny) — more
  expressive than orthodox object-capability, since a base profile can veto a
  name a restrict file never mentions;
- **fs** by read/write path prefix with denies;
- **net** as allow/deny;
- **detach** as allow/deny — the one dimension that gates a *verb* rather than
  an action on a resource, and the one that reaches no OS profile: it decides
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

**The dimensions are a type, and the bracket is the form's.** `grant` declares
its six as a closed row — `net` and `detach` at `Bool`, the four structured
ones at a variable each, their interiors staying the decoder's — minted fresh
at every occurrence, and the bundle it is handed is unified against it. So a
misspelled dimension is refused before the program runs, naming `grant` and its
own six; a bundle computed elsewhere gets the same verdict as one written out,
at a *fixed* set of dimensions (the branches of an `if` must agree on a type,
so a bundle whose membership varies has no type — lift the condition to the
form); a map is refused by name, a map's keys being data
([[design/records-and-maps|records-and-maps]]); and `[]` is the empty grant
rather than the empty list, the bracket being read by the form.

Typing the four structured dimensions to full depth is what the row does *not*
do, and the reason is the one condition the presence discipline owes: within
one check a label may not be optional at two different ground types, and
`editor.read: Bool` beside `fs.read: [String]` is exactly that
([[decisions/260921_a-field-is-a-flag-and-a-type|a-field-is-a-flag-and-a-type]]).

A `.ral` profile and the inline
`grant` surface are symmetric: both decode through the same walker into one
frozen `Capabilities`, so configuration is the grant value written as source,
not a second schema. That walker resolves every sigil as it decodes —
[[design/capability-freeze|the freeze boundary]] — so no unresolved form ever
reaches the stack or the IPC wire.

This is the boundary [[map/exarch|exarch]] reuses as its sandbox: each agent
run evaluates under a profile's capabilities pushed onto this same stack.

**Concessions.** Three caveats are inherent to the design, not defects:
- **Bare command names are ambient.** A bare exec key like `git` resolves through
  `PATH`, so it is not a strict object-capability; the mitigation is the base
  profile's deny on shells together with the `fs` deny on writing `xdg:bin` — the
  names that matter are pinned, the rest rest on `PATH` integrity.
- **TOCTOU on path resolution.** The resolver-form and bind-form checks share one
  source, so the OS profile and the in-process check cannot disagree, but symlink
  races inside the admitted set are a known surface, not a closed one.
- **Exec admission is not containment; the projection is.** A prefix that is
  both `exec`-admitted and `fs`-writable reads like an escape hatch — drop a
  binary, the next call admits it — but inside one projection it escalates
  nothing: whatever the confined process writes and then runs is spawned under
  the very projection that admitted the write, so it can do only what its
  author could already do. Copying a denied binary under a fresh name defeats
  the name veto for the same reason and to the same small effect. The overlap
  is therefore not merely tolerable but *required* — `cargo build &&
  ./target/debug/app` is exactly this shape, and every bake-in profile makes
  `cwd:`, `/tmp`, and `tempdir:` both writable and exec-admitted on purpose.
  `capability::deputy_prefixes` accordingly reports and never denies, and *no
  prefix is both* is not an invariant this design wants.

  What does bite is **authority that outlives or exceeds the projection**: a
  write escalates when whoever later treats those bytes as code is not confined
  by the projection that admitted the write — running after the session ends
  (*outlives*) or beside it with more authority (*exceeds*). `xdg:bin` is the
  case the base profiles decide correctly for the wrong stated reason. It must
  stay unwritable not because `exec` also names it, but because a program left
  on the human's `$PATH` is run tomorrow by the human, unconfined.

**An open class: the unconfined reader.** Every dimension `grant` gates names
what the *confined* process may do. One family of escalations has no such
shape: a write into a region that some process the host runs later —
unconfined — treats as code. A hook under `gitdir:`, a `core.pager` or an alias
in `.git/config`, an `.envrc`, a `package.json` script, a `Makefile`. None is a
binary, none is exec-admitted, and each runs with the user's full authority the
moment the user reaches for the ordinary tool that reads it. The `exec`
dimension cannot see this: a file that is never executed, only interpreted, is
invisible to a gate on execution. The default profiles make `cwd:` and
`gitdir:` writable, so the class is live rather than hypothetical.

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
model this lattice instantiates — composition over a Belnap bilattice).

**Realised in** [[internals/capability-enforcement|capability-enforcement]].

Cite: RATIONALE §"`grant` attenuates authority",
§"Lexical data, dynamic authority"; `docs/SPEC.md` §12.
