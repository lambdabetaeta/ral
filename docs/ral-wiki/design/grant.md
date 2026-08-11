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

This is a deliberate mental-model fact, recorded so omission is never mistaken
for an implicit cross-axis deny — see
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] §B7.

**Capability checks gate five dimensions:**

- **exec** over a three-valued lattice (Allow / Subcommands / Deny) — more
  expressive than orthodox object-capability, since a base profile can veto a
  name a restrict file never mentions;
- **fs** by read/write path prefix with denies;
- **net** as allow/deny;
- **detach** as allow/deny — the one dimension that gates a *verb* rather than
  an action on a resource, and the one that reaches no OS profile: it decides
  whether a process the session stops owning may be born at all, never what
  that process may then do
  ([[decisions/260727_detach-under-a-grant|detach-under-a-grant]]);
- **audit** as a record of check events.

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
- **Confused deputy at the name layer.** A prefix that is both `exec`-admitted and
  `fs`-writable is an escape hatch — drop a binary, the next call admits it — so the
  invariant *no prefix is both* is a candidate load-time check.

See also [[design/syscalls-are-effects|syscalls-are-effects]] (a capability is permission over the effect set),
[[design/scoping|scoping]], [[design/control-operators|control-operators]],
[[design/two-enforcers|two-enforcers]], [[related/system-c|system-c]] (the
type-based pole of this calculus),
[[related/access-control-algebra|access-control-algebra]] (the security-literature
model this lattice instantiates — composition over a Belnap bilattice).

**Realised in** [[internals/capability-enforcement|capability-enforcement]].

Cite: RATIONALE §"`grant` attenuates authority",
§"Lexical data, dynamic authority"; `docs/SPEC.md` §12.
