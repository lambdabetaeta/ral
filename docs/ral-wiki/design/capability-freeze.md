# The freeze boundary

**A grant is resolved the moment it is decoded: `decode_capability_map` walks
every spelling once and returns a `GrantLayer`, whose rules hold what the walks
reached — positions for allowances, held objects for exclusions — and nothing
beside them.** The boundary between *syntactic profile* and *resolved
authority* is a type split with one crossing: nothing authored reaches the dynamic
stack or the IPC wire, because a `GrantLayer` keeps no authored copy for
anything to disagree with.

The types (`core/src/capability/lattice.rs`):

- *`Capabilities`* — the authored value: per-effect policies (`exec`, `fs`,
  `net`, and the `editor` and `shell` verb sets) over spellings. Nothing
  judges it and no layer keeps it.
- *`GrantLayer`* — what a layer in force is: its rules, resolved, and the axes
  that name no path. Each path is a `GrantPath { written, real }`: the
  spelling as written, and what its walk made of it (`Extent` for an allowance,
  `Excluded` for an exclusion; [[internals/path-judgment|path-judgment]]).
  Layers compose by stacking on a `GrantStack`, every judgment meeting the
  layers' evidence at the access
  ([[design/authority-tables|authority-tables]]); `GrantLayer::extend` joins a
  base with a `Fragment` into one ceiling layer.

## `decode` is the one-way crossing

The sole non-trivial constructor is `decode_capability_map`
(`core/src/guard/decode.rs`), the single `Value::Map → GrantLayer`
function. It walks a `.ral` profile's terminal record — or the inline
`grant [...] { body }` record — into a layer, resolving each spelling by the
walk as it freezes it. In doing so it:

- **resolves** every place (`` `cwd ``, `` `home ``, `` `tempdir ``, `` `scratch ``,
  `` `gitdir ``, `` `xdg-* ``, `` `absolute ``) and every selector (`` `path ``, `` `system ``, the package managers)
  against a `FreezeCtx` (owned `home` and `cwd`, `FreezeCtx::of(&Shell)`;
  `` `tempdir `` against the host's temp dir; `` `scratch `` against the spelling
  of the session's scratch the context carries, which a host that made none
  refuses). The freeze is `guard/freeze.rs`:
  `FreezeCtx::place` is the one place a grant's place becomes a `FrozenPath`,
  `FreezeCtx::selector` the one place a selector becomes many, and
  `FrozenPath::from_surface` the one minting entry
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
`GrantLayer`, and a `Capabilities` that arrives already decoded passes through
`GrantLayer::from_capabilities`, the same walk. So *resolved by construction* is
a visibility fact, not a discipline: no surface admits an unresolved layer.

## What decode pins, and what the access resolves

**Decode walks each spelling once and keeps what the walk reached; it never
touches the path of a future access, which is walked at the moment of
enforcement.** A `FrozenPath` is the author's spelling, folded; what it names is
the walk's to say, once. That makes the algebra a pure function of the layers.

- **An allowance is a position and is never resolved again.** An fs prefix or
  an exec `Path`/`Dir` key keeps the `RealPath` its walk reached, so a symlink
  put in its place later redirects nothing. The root spelling `/` is
  `Extent::Everywhere` on every host, resolved by polarity with no walk (Windows
  has no `/` to walk); a spelling that walks to the root on Unix is the same.
- **An exclusion is the object its spelling reached.** `walk_excluded` holds the
  object and, where it was a link, its target; the exclusion matches by position
  or by that object, so it holds under any new name the object gets, and a link
  retargeted later is still the denied link. A bare program name is resolved
  where the grant is decoded, on the decoding environment's `PATH`, absolute
  entries alone.
- **Resolving refuses a path that is not Unicode** where it is a spelling
  (`FrozenPath::freeze`); a position is kept native and byte-exact.
- **A guest's grant is resolved on the guest's own filesystem, not the host's.**
  synod spells it with `FrozenPath::from_guest` and sends the authored
  `Capabilities` as the guest engine's config; the engine resolves it where the
  guard that matches it runs. Spellings cross hosts, a position or an identity
  never does ([[design/authority-tables|authority-tables]]).
- **The access is walked before matching.** A grant of
  `` read: [`home sandbox] `` is not escaped by a `~/sandbox/leak → /etc/passwd`
  link the body plants once the grant has taken force: the walk lands on
  `/etc/passwd`, which is outside the region and denied. This is a
  time-of-check-to-time-of-use race whose *use* is the `open()`; the open goes
  through the handle the walk holds.

So the runtime fold judges a transcript against the resolved layers: expand →
anchor → walk → `RealPath::contains` or `Excluded::matches`, per access
([[internals/path-judgment|path-judgment]]). This is the access-time complement
to the decode-time resolution — the same reason the runtime fold stays
[[design/capability-carriers|a live judgment]] and not a precomputed value.

Resolving the rule's anchors pins its *authorship*: `` `cwd `` resolves to where
the grant was established, so a later `cd` cannot retroactively widen authority.

**Allowances name what they named.** A grant reading `work/ro` and writing
`work` is not escaped by renaming `ro` and planting a symlink named `ro` that
points outside: the allowance keeps its position, the guard judges by it, and
each backend refuses a launch whose allowance's position now meets a link or
reparse point rather than following it (`open_nofollow`). Creating a granted
directory after the grant, or `rm -rf dir && mkdir dir`, keeps working: these
are path grants, not inode pins. Position is right for allowances and identity
is right for exclusions ([[decisions/261021_one-path|one-path]]).

## The XDG-escape guard is a per-profile invariant

Because freeze runs once per profile *as it loads*, the escape check fires on
**any** input profile that names an `xdg-*` place resolving outside `HOME` —
whether or not later composition would keep that path. A `restrict` profile that
mentions an escaping `` `xdg-data `` is rejected even if a `meet` would have
intersected it away.

The guard asks its containment question of the *traces the walks reached* — the
`fs` rules authorise objects, so it asks `RealPath::contains`, the predicate
every allowance speaks through, of the xdg place's trace and of `HOME`'s.
Asking it of the surface spelling instead read `XDG_DATA_HOME=~/link`, with
`link → /etc`, as contained: the surface stayed under `HOME` while the
position the walk reached — the only one `fs` enforcement ever sees — was
`/etc`.

A freeze guard and an in-process guard that judge different forms of the same
prefix do not guard it. The general rule is
[[invariants/grants-judge-objects|grants judge objects]]: every authority is
judged on what the walk reached, and the choice is made once inside
`core/src/path/`, where the containment kernel stays; each authority reaches it
through `RealPath::contains` or `Excluded::matches`. No caller elsewhere holds
two paths and picks.

This is *fail-closed*: the honest reading for a defence-in-depth control is that
the offending input is refused at the boundary, not that its rejection depends on
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
- A pointer nothing claims is a `PolicyError` at the boundary (`guard::freeze`, which holds the three `gitdir` refusals), like an escaping
  `xdg-*` place. Silently narrowing to the cwd would be safe and unreadable — the same
  fail-closed-and-say-so reading the XDG guard takes.

## Composition is over resolved paths

When several profiles merge into one ceiling — exarch's
`base ◁ extend ⊓ restrict` ([[map/exarch/policy|policy]]) or the
`--capabilities` fold in `apply_session_profiles` — each is resolved as it loads
and the lattice operations run on the resolved layers.

- the meet and the extension compare **resolved positions**, so prefix containment is exact
  (`/work/proj/src ⊂ /work/proj` is seen as such). Strictly more precise than
  comparing place spellings; no soundness change.
- the `FreezeCtx` is built **once** per composition site, home and cwd in hand
  — `eval_grant` and `apply_session_profiles` against the live shell's
  effective home; exarch's `for_invocation` against `host::home()` and the
  session cwd.

The resolved layers and the runtime fold are then two fidelities of one
algebra — over the positions and objects resolved at decode here, over the
transcript of the access's own walk at enforcement time — sharing one meet,
`Table::meet` ([[design/authority-tables|authority-tables]]), but kept apart
deliberately:
[[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]
§"one combinator at two fidelities" and §"the two folds stay separate".
Composition itself is stacking, not a fold over frozen values: the
`GrantStack` holds each layer and every judgment meets the layers'
evidence at the access ([[decisions/260906_object-not-name|object-not-name]]).

## Off the wire by construction

`GrantLayer` and `GrantStack` derive `Serialize` / `Deserialize`, and the stack
crosses to a re-exec'd child or a guest engine as its traces: an exclusion's
`Hold` serialises as its trace and deserialises unheld, and the receiver calls
`GrantStack::rehold`, which walks each exclusion again and holds the object
where it is still the one recorded. A codec is pure; a stack that is not
`root()` is dispatched to a local engine only, since a position resolved on one
host means nothing on another.

A spelling means something *in some namespace*, and one carrier crosses to a
different one: synod's grant is spelled on the host and enforced by the
in-process guard inside a Linux guest, so it names `/work` and `/tmp` and is
folded by the guest's rule, not this process's
([[decisions/260726_guest-namespace-prefixes|guest-namespace-prefixes]] — a
Windows host folding `/work` with `Path::components` rebuilds it as `\work`).
The guest resolves the spelling there, so a position is always resolved in the
namespace the guard matches in.

See also [[design/grant|grant]] (the calculus this resolves into),
[[design/two-enforcers|two-enforcers]],
[[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]
(allowances by position, exclusions by object),
[[decisions/260605_capability-stage-collapse|capability-stage-collapse]] (the
decision to collapse to one always-frozen type),
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]],
[[related/system-c|system-c]] (the type-based pole), and
[[map/core/capabilities|map: capabilities]].
