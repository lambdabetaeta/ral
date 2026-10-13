# The freeze boundary

**A capability bundle is resolved the moment it is decoded: there is one type,
`Capabilities`, and every value of it is *frozen* — concrete paths, no places —
by construction.** The boundary between *syntactic profile* and *resolved
authority* is not a type split but a single pass inside the one constructor, so
"an unresolved bundle reaches the dynamic stack or the IPC wire" is
unrepresentable: there is nowhere for one to exist.

The type (`core/src/capability/lattice.rs`):

- *`Capabilities`* — per-effect policies (`exec`, `fs`, `net`, and the
  `editor` and `shell` verb sets), and nothing else: every member is authority, so a bundle
  carries no switch over what is recorded about it. Path lists hold concrete
  absolute paths. Bundles compose by stacking on a `GrantStack`, every
  judgment meeting the layers' evidence at the access
  ([[design/authority-tables|authority-tables]]); `Capabilities::extend` joins a base with
  a fragment into one ceiling bundle.

## `decode` is the one-way door

The sole non-trivial constructor is `decode_capability_map`
(`core/src/guard/decode.rs`), the single `Value::Map → Capabilities`
function. It walks a `.ral` profile's terminal record — or the inline
`grant [...] { body }` record — into a bundle, then runs a *freeze pass* before
returning. In that pass it:

- **resolves** every place (`` `cwd ``, `` `home ``, `` `tempdir ``, `` `gitdir ``,
  `` `xdg-* ``, `` `absolute ``) and every selector (`` `path ``, `` `system ``)
  against a `FreezeCtx` (owned `home` and `cwd`, `FreezeCtx::of(&Shell)`;
  `` `tempdir `` against the process temp dir). The freeze is `guard/freeze.rs`:
  `FreezeCtx::place` is the one place a grant's place becomes a `FrozenPath`,
  `FreezeCtx::selector` the one place a selector becomes many, and
  `FrozenPath::from_surface` the one minting door
  ([[decisions/261006_capability-is-data|capability-is-data]]);
- **classifies** an `absolute` payload on the raw string, before any folding,
  host first, so one profile loads on both platforms: a path absolute under the
  host's rules is live, one absolute only under the other platform's is a *dead
  grant* (`place` answers `None`), and one absolute under neither is refused.
  A well-formed forward-slash UNC `//server/share/x` is live on both; `\x`,
  `C:x` and malformed UNC roots (`\\` or `//` with no server and share) name
  no path anywhere. Host folding would
  otherwise read `C:/../x` on POSIX as the relative `x`;
- **validates** a subpath lexically, before joining, so a profile fails alike
  everywhere: the empty string, a leading separator and a drive prefix are
  refused, `.` is the anchor and `..` folds and may leave it;
- **rejects** a `~` or `xdg:NAME` payload under every tag: those prefixes are
  expanded at run time, never in a grant, and the error names the place meant;
- **rejects** an `xdg-*` place that escapes `home` — defence in depth against an
  attacker-set `XDG_*_HOME=/etc` silently widening a grant;
- **rejects** an anchored place that freezes relative (a relative `HOME` or
  `TMPDIR`) and a `` `home `` or `xdg-*` place where nothing binds `HOME`: no
  base to resolve against, and the escape guard just above is itself stated in
  `home`'s terms, so an absolute `XDG_*_HOME` cannot stand in for one. The
  readers answer `Option`, and the reason the place cannot be frozen is the
  reason the author is given;
- **rejects** a `` `gitdir `` whose `.git` pointer file names a directory that
  does not name the working tree back.

A bare string or a relative path is no place at all: the checker refuses it
before the program runs, and the decoder refuses it again for a value that
arrived untyped. Authors write `` `cwd sub `` to pin "relative to here" once.

Both callers — `eval_grant` (`core/src/evaluator/scope.rs`) and the profile
loader (`core/src/load/profile.rs`) — are in-crate, and the path-free
`root` / `deny_all` / `default` are the only other ways to obtain a
`Capabilities`. So *resolved by construction* is a visibility fact, not a
discipline: no surface admits an unresolved bundle.

## What freeze pins, and what the access resolves

**Freeze pins the author's places and follows their symlinks once; it never
touches the path of a future access, which is resolved at the moment of
enforcement.** `FrozenPath::from_surface` is the one disk consultation: it
folds the spelling and canonicalises leniently, so a prefix carries its
`surface` (the author's spelling) and its `real` form (symlinks followed as the
disk stood at decode). That makes the algebra a pure function of the policy.
Enforcement then re-asks the world.

- **Allowances are frozen for good; only exclusions re-resolve.** An allowance,
  fs prefix or exec `Path`/`Dir` key, keeps the real path it was frozen to and
  is never re-frozen, so a symlink put in its place later redirects nothing. An
  exclusion re-freezes against the live disk on every question
  (`FrozenPath::refreeze`; `RealPath::live` for exec), so it holds its object
  under any new spelling the moment one exists. A bare program name resolves
  afresh at each run.
- **Freezing refuses a real path that is not Unicode**, since the frozen real
  form is now the allowance's identity and a lossy form would grant a sibling.
- **A guest allowance is not yet frozen inside the machine.** synod mints it with
  `FrozenPath::from_guest`, whose real form copies its surface, and sends it with
  every run, so a redirect made in one guest run can take effect in the next.
  Freezing it once per session, inside the guest, is deferred
  ([[design/authority-tables|authority-tables]], "Freshness").
- **The access is canonicalised before matching.** A grant of
  `` read: [`home sandbox] `` is not escaped by a `~/sandbox/leak → /etc/passwd`
  link the body plants once the grant has taken force: a lexical match would
  admit it, the resolved `/etc/passwd` is outside the region and denied. This is
  a time-of-check-to-time-of-use race whose *use* is the `open()`; at decode the
  link does not yet exist, and there is nothing for freeze to resolve.

So the runtime fold resolves paths against the live environment rather than
reading a fully-canonical bundle: the [[internals/path-judgment|four-stage
rule]] (expand → lex → canonicalise → `path_within`) runs *per access*, and
"canonicalise before match" is what closes symlink and `..` escape. This is the
access-time complement to the load-time freeze — the same reason the runtime
fold stays [[design/capability-carriers|a live judgment]] and not a
precomputed value.

Freezing the rule's anchors pins its *authorship*: `` `cwd `` resolves to where
the grant was established, so a later `cd` cannot retroactively widen authority.
The prefixes themselves are a different matter.

**Allowances name what they named.** A grant reading `work/ro` and writing
`work` is not escaped by renaming `ro` and planting a symlink named `ro` that
points outside: the allowance keeps its frozen real path, the guard judges by it,
and each backend refuses a launch whose frozen allowance now meets a link or
reparse point rather than following it (Landlock's `RESOLVE_NO_SYMLINKS` binds,
Seatbelt's resolved-path check, Windows' held handles). Creating a granted
directory after the grant, or `rm -rf dir && mkdir dir`, keeps working: these
are path grants, not inode pins. Re-resolution is right for exclusions and wrong
for allowances ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).

## The XDG-escape guard is a per-profile invariant

Because freeze runs once per profile *as it loads*, the escape check fires on
**any** input profile that names an `xdg-*` place resolving outside `HOME` —
whether or not later composition would keep that path. A `restrict` profile that
mentions an escaping `` `xdg-data `` is rejected even if a `meet` would have
intersected it away.

The guard asks its containment question on the form the *in-process guard*
matches, here the `fs` one, which authorises objects, so the form is the
symlink-followed `real` one, asked with `path_within` — the same alias-aware
predicate every fs rule speaks through, `FrozenPath::contains` — and with `HOME` canonicalised on the
other side. Asking it of the surface spelling instead read
`XDG_DATA_HOME=~/link`, with `link → /etc`, as contained: the surface stayed
under `HOME` while the frozen `real` form — the only one `fs` enforcement
ever sees — was `/etc`.

A freeze guard and an in-process guard that judge different forms of the same
prefix do not guard it. The general rule is
[[invariants/grants-judge-objects|grants judge objects]]: every authority is
judged on the real form, and the choice is made once inside
`core/src/path/`, where the containment kernel now
stays, and each authority reaches it through its own door. No caller elsewhere
holds two paths and picks.

This is *fail-closed*: the honest reading for a defence-in-depth control is that
the offending input is refused at the door, not that its rejection depends on
what survives the lattice. Error attribution is per-profile — exarch's loader
prepends `--restrict <path>` (or `--extend`), so the message names the file
that carried the bad token.

## A `` `gitdir `` pointer is answered, not obeyed

**Two places read a source the session does not author — an `XDG_*_HOME` and a
`.git` pointer file — and each is guarded in the terms of whoever could write
it.** The `xdg-*` guard is containment, because an environment variable's harm is
where it lands. The `gitdir` guard cannot be: a linked worktree's git directory
legitimately lies outside the tree, so no region bounds it. What bounds it is
*mutual reference*.

- A `.git` file lives in the working tree, which under exarch's `reasonable`
  base is granted for write. So the pointer is a sentence the confined body may
  compose, and `` `gitdir `` is granted read **and** write.
- Freeze therefore follows the pointer only as far as a git directory that names
  this tree back — a linked worktree's `gitdir` file, or `core.worktree` in the
  config of a repository split off with `--separate-git-dir`, the two records git
  itself keeps. Both ends must agree, and only one end is writable from inside
  the tree.
- A pointer nothing claims is a `PolicyError` at the door (`guard::freeze`, which holds the three `gitdir` refusals), like an escaping
  `xdg-*` place. Silently narrowing to the cwd would be safe and unreadable — the same
  fail-closed-and-say-so reading the XDG guard takes.

## Composition is over resolved paths

When several profiles merge into one ceiling — exarch's
`base ◁ extend ⊓ restrict` ([[map/exarch/policy|policy]]) or the
`--capabilities` fold in `apply_session_profiles` — each is frozen as it loads
and the lattice operations run on the resolved bundles.

- the meet and the extension compare **concrete paths**, so prefix containment is exact
  (`/work/proj/src ⊂ /work/proj` is seen as such). Strictly more precise than
  comparing place spellings; no soundness change.
- the `FreezeCtx` is built **once** per composition site, home and cwd in hand
  — `eval_grant` and `apply_session_profiles` against the live shell's
  effective home; exarch's `for_invocation` against `host::home()` and the
  session cwd.

The frozen prefixes and the runtime fold are then two fidelities of one
algebra — over the forms frozen at decode here, over forms re-frozen against
the live disk at enforcement time — sharing one meet, `Table::meet`
([[design/authority-tables|authority-tables]]), but kept apart deliberately:
[[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]
§"one combinator at two fidelities" and §"the two folds stay separate".
Composition itself is stacking, not a fold over frozen values: the
`GrantStack` holds each layer and every judgment meets the layers'
evidence at the access ([[decisions/260906_object-not-name|object-not-name]]).

## Off the wire by construction

`Capabilities` derives `Serialize` / `Deserialize` so the stack's folded
[[design/capability-carriers|SandboxProjection]] crosses to a re-exec'd child —
a trusted boundary between cooperating ral processes, where the parent has
already resolved every path. Since a `Capabilities` is always resolved, only
concrete paths cross; there is no second, unresolved form that could.

A resolved path is resolved *in some namespace*, and one carrier crosses to a
different one: synod's bundle is frozen on the host and enforced by the in-process guard inside
a Linux guest, so its prefixes name `/work` and `/tmp` and are folded by the
guest's rule, not this process's
([[decisions/260726_guest-namespace-prefixes|guest-namespace-prefixes]] — a
Windows host folding `/work` with `Path::components` rebuilds it as `\work`).
The like-for-like promise above is per-namespace and unaffected; what the case
adds is that "the parent has already resolved every path" must be read as *in the
namespace the child will match in*.

See also [[design/grant|grant]] (the calculus this resolves into),
[[design/two-enforcers|two-enforcers]],
[[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]
(frozen allowances, re-resolved exclusions),
[[decisions/260605_capability-stage-collapse|capability-stage-collapse]] (the
decision to collapse to one always-frozen type),
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]],
[[related/system-c|system-c]] (the type-based pole), and
[[map/core/capabilities|map: capabilities]].
