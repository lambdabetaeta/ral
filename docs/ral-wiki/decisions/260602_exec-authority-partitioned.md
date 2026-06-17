---
status: active
---

# Exec authority gets its partitioned type; the two folds stay two

A simplification pass over the capability layer following the
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]]
work. Four changes, all enforcement-preserving — the `core/tests/sandbox_*.rs`,
`grant_policy.rs`, and `ral/tests/capabilities.rs` suites pass unchanged before
and after — plus one design question settled in the negative.

## The exec map was always partitioned on use, so the type is now partitioned

`Capabilities.exec` / `RawCapabilities.exec` were
`Option<BTreeMap<String, ExecPolicy>>` whose keys carried three shapes in one
namespace: a bare command name (`git`), an absolute literal (`/usr/bin/git`),
and a directory prefix marked by a trailing `/` (`/usr/bin/`). **Every consumer
re-derived the key's kind** (`is_subpath_key`, then `split_exec_map`,
`partition_exec`, the matcher branches) before doing anything. A map that is
always split on use is the wrong type.

It is now `Option<ExecMap>`:

- `ExecMap { literals: BTreeMap<String, ExecPolicy>, dirs: BTreeMap<String, ExecDir> }`.
- *`ExecDir`* is two-valued (`Allow` / `Deny`): a directory admits or denies
  binaries under it but cannot name subcommands, so **`Subcommands`-on-a-directory
  is unrepresentable** — `validate_paths` no longer rejects it and the defensive
  match arms in the matcher and projection are gone.
- Dir keys are stored **slash-free**: the trailing `/` was a string tag for
  "this key is a subpath" inside the unified map; the type now encodes that, so
  the marker is dropped. Matching stays safe because `path_within` compares by
  path *component* (`Path::starts_with`), so `/usr/bin` does not match
  `/usr/binary` — the slash was never the safety mechanism.

Classification happens once, in the surface decoder (`decode_exec_grant`);
`split_exec_map` and `partition_exec` are deleted.

## The admitted verdict cannot carry a `Deny`, so it doesn't

`ExecVerdict::Allowed` / `LayerExec::Allowed` carried a full `ExecPolicy`, but a
`Deny` short-circuits to `Denied` upstream and can never reach an admitted
verdict — forcing an "unreachable in practice" arm in `check_exec_args`. The
allowed arms now carry *`Admit { Any, Subcommands(Vec<String>) }`*, whose `Meet`
mirrors the non-`Deny` cases of `ExecPolicy::meet` byte-for-byte. The impossible
state, and its dead arm, are gone.

## The prefix-set meet is one combinator at two fidelities

"Intersect two prefix sets, keeping the deeper of each overlapping pair" was
spelled twice — lexically in `intersect_prefix_strings` (raw strings, no
`Context`) and symlink-aware in `GrantPathSet`'s `Meet` (canonical form, at
projection time). The combinatorial core is now `crate::path::meet_prefix_sets_by`,
parameterised by the key overlap is judged on; each caller keeps its own fidelity
and dedup/ordering. This **completes the follow-up reduced-authority-witness §B5
named.** The per-access `path_within` loop in `check_fs_op` is a membership test,
not a meet, and stays separate.

## The two folds stay separate — full unification would be duct tape

Whether the in-process *check* fold (`evaluate_exec`, `check_fs_op`) and the OS
*projection* fold (`project`, `reduce_exec`) could collapse into one was
re-examined and **declined.** They are dual, not nested:

- the check fold is a **query** — it threads a subject (candidate names, one
  access path) through the stack to a yes/no;
- the projection fold is a **materialisation** — subject-free, it enumerates a
  closed region set a *different* process (the OS kernel) later tests subjects
  against.

A shared subject-free intermediate cannot preserve, without re-embedding both
originals:

- the **argv-level three-valued verdict** — `Subcommands` is checked against
  runtime `args`, which do not exist at projection time; the projection
  faithfully flattens it onto the coarser path-level OS lattice;
- the **process-split, per-access, mode-dependent resolver** —
  `resolver_for_check` is `LexicalOnly` in the sandboxed child and re-resolves
  each access, while the projection canonicalises once `Lenient` in the parent;
  a single fold must pick one mode and is wrong on the other side.

Collapsing them would force an intermediate that is a superset of both
representations plus a coupling layer — a hack the no-duct-tape rule forbids, and
one that would fail the enforcement-preservation guard. The `Reduced` witness
already gives the defensible unification: one type-enforced *entry point* both
consumers derive from.

## Directory meet/join follow the `ExecDir` lattice

Composition over `dirs` is by sign, mirroring the literal half:

- **meet** — allow-regions intersect (a prefix survives only where both sides
  admit it, the deeper winning); deny-regions union (a `Deny` is sticky, so it
  propagates from either side); on an exact-key clash the deny wins (meet's
  bottom).
- **join** — allow-regions union; deny-regions intersect (a prefix stays denied
  only where both sides deny); on a clash the allow wins (join's top).

This closes a privilege-escalation inversion: combining the `dirs` halves by
their bare key sets and re-labelling every survivor `Allow` let `meet` turn a
restrict profile's `dir: Deny` into an `Allow` (and drop a one-sided deny
entirely), so the operation a profile author trusts to *only narrow* could
widen. The per-layer matcher (`longest_dir_match`) and the projection
(`reduce_exec`) always honoured dir `Deny`; the gap was only in lattice
*composition* (`base.join(extend).meet(restrict)`). Guarded by
`meet_exec_dirs_deny_beats_allow` and the one-sided-deny tests in `lattice_tests`.

The `RawCapabilities` half of the `*.exec` pair this page names was later folded
into a single always-frozen `Capabilities` —
[[decisions/260605_capability-stage-collapse|capability-stage-collapse]]; the
partitioned `ExecMap` shape decided here is unchanged. The `Subcommands(Vec<String>)`
this page names later became `Subcommands(BTreeSet<String>)` — the set it models,
so its lattice laws hold by construction —
[[decisions/260615_no-core-repr-leak-into-exarch|no-core-repr-leak-into-exarch]].

See also [[map/core/capabilities|capabilities]],
[[internals/capability-enforcement|capability-enforcement]],
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]].
