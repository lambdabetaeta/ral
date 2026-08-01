# The freeze boundary

**A capability bundle is resolved the moment it is decoded: there is one type,
`Capabilities`, and every value of it is *frozen* — concrete paths, no sigils —
by construction.** The boundary between *syntactic profile* and *resolved
authority* is not a type split but a single pass inside the one constructor, so
"an unresolved bundle reaches the dynamic stack or the IPC wire" is
unrepresentable: there is nowhere for one to exist.

The type (`core/src/types/capability.rs`):

- *`Capabilities`* — per-effect policies (`exec`, `fs`, `net`, `editor`, `shell`)
  plus an `audit` flag. Path lists hold concrete absolute paths. The lattice
  `meet` / `join` compose two of them into a third.

## `decode` is the one-way door

The sole non-trivial constructor is `decode_capability_map`
(`core/src/capability/decode.rs`), the single `Value::Map → Capabilities`
function. It walks a `.ral` profile's terminal map — or the inline
`grant [...] { body }` map — into a bundle, then runs a *freeze pass* before
returning. In that pass it:

- **resolves** every `~` / `xdg:` / `cwd:` / `tempdir:` sigil against a
  `FreezeCtx { home, cwd }` (and `tempdir:` against the process temp dir);
- **rejects** an `xdg:` value that escapes `home` — defence in depth against an
  attacker-set `XDG_*_HOME=/etc` silently widening a grant;
- **rejects** a non-sigil *relative* entry — it would otherwise anchor to the
  live cwd at the access rather than the grant, so the same rule would mean a
  different region after a `cd`. Authors write `cwd:sub` to pin "relative to
  here" once; a bare command name in `exec` is a name, not a path, and is exempt.

Both callers — `eval_grant` (`core/src/evaluator/scope.rs`) and the profile
loader (`core/src/capability/load.rs`) — are in-crate, and the path-free
`root` / `deny_all` / `default` are the only other ways to obtain a
`Capabilities`. So *resolved by construction* is a visibility fact, not a
discipline: no surface admits a sigil-bearing bundle.

## What freeze does *not* resolve: the access and its symlinks

**Freeze resolves the grant's own paths; it never canonicalises symlinks, and it
never touches the path of a future access — those belong to the moment of
enforcement, not the moment of decode.** Sigil expansion is intrinsic to the
profile and safe to pin once. Symlink resolution is not: the symlink that defeats
a grant is *created at runtime, inside an already-granted region*, on the very
path a running body asks to open.

- A grant of `read: [~/sandbox]` is not escaped by a `~/sandbox/leak → /etc/passwd`
  link the body plants once the grant has taken force. A lexical match would admit
  it — `~/sandbox/leak` sits squarely under the granted prefix — so what saves us
  is canonicalising the **access** *before* matching: resolved to `/etc/passwd`,
  outside the region, it is denied. This is a time-of-check-to-time-of-use race
  whose *use* is the `open()`; at decode the link does not yet exist, and there is
  nothing for freeze to resolve.
- Canonicalisation is therefore a property of the *access*, not the *rule*.
  Pinning the grant's prefixes to canonical form at freeze would defend nothing
  here — the threat rides the opened path, not the prefix — and would shackle the
  grant to the symlink topology as it stood at decode, drifting from the live
  filesystem thereafter.

So the runtime fold resolves paths against the live environment rather than
reading a fully-canonical bundle: the [[internals/capability-enforcement|four-stage
rule]] (expand → lex → canonicalise → `path_within`) runs *per access*, and
"canonicalise before match" is exactly what closes symlink and `..` escape. This
is the access-time complement to the load-time freeze — the same reason the
runtime fold stays [[design/capability-carriers|a live judgment]] and not a
precomputed value.

The two are dual defences against a confined body cheating its fence. Freezing the
rule's sigils pins its *authorship*: `cwd:` resolves to where the grant was
established, so a later `cd` cannot retroactively widen authority — the body cannot
widen by moving. Deferring symlink resolution to the access binds the rule to the
*live world* — so the body cannot escape by re-pointing. Pin the intent; resolve
the world.

## The XDG-escape guard is a per-profile invariant

Because freeze runs once per profile *as it loads*, the escape check fires on
**any** input profile that names an `xdg:` path resolving outside `$HOME` —
whether or not later composition would keep that path. A `restrict` profile that
mentions an escaping `xdg:data` is rejected even if a `meet` would have
intersected it away.

This is *fail-closed*: the honest reading for a defence-in-depth control is that
the offending input is refused at the door, not that its rejection depends on
what survives the lattice. Error attribution is per-profile — exarch's loader
prepends `--restrict <path>` (or `--extend-base`), so the message names the file
that carried the bad token.

## Composition is over resolved paths

When several profiles merge into one ceiling — exarch's
`base ∨ extend ⊓ restrict` ([[map/exarch/policy|policy]]) or the
`--capabilities` fold in `apply_session_profiles` — each is frozen as it loads
and the lattice operations run on the resolved bundles.

- `meet` / `join` compare **concrete paths**, so prefix containment is exact
  (`/work/proj/src ⊂ /work/proj` is seen as such). Strictly more precise than
  comparing sigil strings; no soundness change.
- the `FreezeCtx` is built **once** per composition site, home and cwd in hand
  — `eval_grant` and `apply_session_profiles` against the live shell's
  effective home; exarch's `for_invocation` against `host::home()` and the
  session cwd.

The composition `meet` and the runtime fold are then two fidelities of one
algebra — lexical over the resolved strings here, canonical over
symlink-resolved paths at enforcement time — sharing their atoms
(`meet_prefix_sets_by`, `meet_literal_exec`) but kept apart deliberately:
[[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]
§"one combinator at two fidelities" and §"the two folds stay separate".

## Off the wire by construction

`Capabilities` derives `Serialize` / `Deserialize` so its meet-folded
[[design/capability-carriers|SandboxProjection]] crosses to a re-exec'd child —
a trusted boundary between cooperating ral processes, where the parent has
already resolved every path. Since a `Capabilities` is always resolved, only
concrete paths cross; there is no second, sigil-bearing form that could.

A resolved path is resolved *in some namespace*, and one carrier crosses to a
different one: synod's bundle is frozen on the host and enforced by a gate inside
a Linux guest, so its prefixes name `/work` and `/tmp` and are folded by the
guest's rule, not this process's
([[decisions/260726_guest-namespace-prefixes|guest-namespace-prefixes]] — a
Windows host folding `/work` with `Path::components` rebuilds it as `\work`).
The like-for-like promise above is per-namespace and unaffected; what the case
adds is that "the parent has already resolved every path" must be read as *in the
namespace the child will match in*.

See also [[design/grant|grant]] (the calculus this resolves into),
[[design/two-enforcers|two-enforcers]],
[[decisions/260605_capability-stage-collapse|capability-stage-collapse]] (the
decision to collapse to one always-frozen type),
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]],
[[related/system-c|system-c]] (the type-based pole), and
[[map/core/capabilities|map: capabilities]].
