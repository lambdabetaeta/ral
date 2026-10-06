---
status: active
generated_at_commit: 7f61632a
verified_at_commit: f4e88bce
anchors: [ExecGrant, ExecRules, Verdict, Rank, Program, Subject, Head, Missing, Admitted, check_exec, admits_head, RealPath, meet_insert, holds, met, command_name_key, evicts]
---

# Exec rules: one table, one function

Supersedes [[decisions/260806_a-head-has-three-identities|a-head-has-three-identities]].
[[decisions/260906_object-not-name|object-not-name]] made the `fs` guard
authorise the object; this does the same for exec.

## The law

> Decode turns each grant key into rules about programs. One function,
> `ExecRules::verdict`, gives the verdict for a program. The in-process guard
> calls it. The kernel gets the same rules as an ordered list. No other code
> makes a verdict.

- **Keys.** Decode keeps the author's grant as an `ExecGrant { names, paths,
  dirs }`: bare keys (`git`), frozen path keys (`/usr/bin/git`, `~/bin/x`),
  and frozen dir keys with the expansions of `path:` and `system:`. It rides
  the wire and exarch's prompt shows its spellings; nothing matches it.
- **Rules.** `ExecRules::compile` turns one layer's grant into a table over
  host files (`RealPath`), bundled tools, directories and vetoed names. A bare
  key is three rules at most: the file the *host* `PATH` finds (the process's
  own environment, never a scoped override), the bundled tool of that name,
  and for a deny a veto on the name everywhere. Path and dir keys are their
  frozen resolved forms, never re-read from disk. Every rule enters through
  `meet_insert`, so two keys reaching one file meet. A rule holds a name as
  its polarity reads it (`holds`, over `RealPath::within::<P>`): an allow dir
  covers by stored containment and an allow file names by stored equality; a
  deny dir or file does so under every spelling, and a veto matches by
  `command_name_key`
  ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]).
- **Precedence.** `Rank` is `Dir(depth) < Carrier < Exact < Veto`, and its
  derived `Ord` *is* precedence — the most-specific-match-wins of
  [[related/access-control-algebra|access-control-algebra]], made literal: among the rules that match, the greatest rank
  decides and equal ranks meet (`most_specific`, one fold). No match denies.
  A tool has no place on disk, so only its exact rule and a veto speak to it.
- **Stacking.** A stack's table is the meet of its layers' (`met`), the
  restrict-is-meet of
  [[related/access-control-algebra|access-control-algebra]]: allows meet,
  denies join. Every key of either side is met at its verdict, and a key met
  to Deny is written only where a layer wrote that deny; a default written as
  a deny would hold every other spelling of the name. So
  `verdict(a ∧ b, p) = verdict(a, p) ∧ verdict(b, p)` for every program. A
  property test over a fixed universe, with case and normalisation pairs,
  checks it. Compiled afresh on every question, since a bare key follows the
  host `PATH`.
- **Kernel.** `ExecRules::kernel` emits the same rules, each file once at its
  final verdict (carriers counted), sorted by `Rank` ascending (denies after
  allows within a rank), so the
  kernel's last-match-wins is highest-rank-wins by construction. A second
  property test checks a model of last-match-wins over that list against
  `verdict`. See [[decisions/261004_exec-carriers|exec-carriers]].

## Types

- **`Verdict`**: `Deny < Only(s) < Allow`. A directory carries a `bool`, since
  it cannot restrict argv; it enters a ranking as `Verdict::from`.
- **`Program`**: `Tool(name)` or `File { path, real }`, where `real` is the
  `realpath(3)` of `path`, judged and run, and `path` the spelling the program
  sees as `argv[0]`. `Subject` is its borrowed
  view, what `verdict` takes.
- **`Head { shown, program: Result<Program, Missing> }`**
  (`runtime/command/head.rs`). A bundled name is its tool with no walk; any
  other bare name is the effective `PATH`'s hit; a path head is anchored at
  the launch cwd and spelled as written, because the kernel walks a `..` after
  the links before it and honours a trailing `/` (Windows folds `..` itself
  and gets an absolute path). A path head that is not an executable file has
  no program, so no grant judges it before launch. A head with no program
  carries `Missing::{NotFound, NotExecutable}`, never
  reaches the guard, and `vet` reports 127 or 126.
- **`Admitted`**: what `check_exec` returns, holding the program, its argv and
  the table that judged it, with private fields. `SpawnPlan` carries it,
  `build_command` and the sandboxed launch read program and args only from it,
  and the projection renders exactly its table. Nothing launches unjudged, and
  the launcher runs the real path that was judged, under the spelling as
  `argv[0]`.

## Why

Before, the guard matched an enumeration of spellings against keys frozen in
yet another form, and the projection re-derived admission with its own fold.
Each spelling missing from the list was a bug and each on it a channel, and
two keys naming one file could disagree. One table judged by one function,
and rendered rather than re-derived, leaves no second opinion to drift.

## Allows meet, denies join

**A rule holds a name as its polarity reads it, so the meet of two tables
keeps a deny only where a layer wrote one.** Write `⊑S` for stored
containment (`path_within` under `Stored`, firmlink-aware) and `⊑C` for
collision containment (`collision_key` per component); `⊑S ⊆ ⊑C`, both are
transitive, both preserve component count. In a table:

- an *allow dir* `k` speaks at `p` iff `p ⊑S k`, a *deny dir* `d` iff `p ⊑C d`,
  each at `Rank::Dir(depth)`;
- an *allow* or `Only` *file* `f` speaks iff `p` and `f` contain each other
  under `⊑S`, a *deny file* iff under `⊑C`, at `Exact`;
- a *veto* speaks iff the program's `command_name_key` is the veto's, a *tool*
  by exact name.

**The problem a pointwise meet has.** One that stores, at every key of either
side, the met verdict writes `false` at a dir key one side never mentioned: a
*default* deny recorded as an *authored* one. Under `Stored` the two cannot be
told apart, since a side that covers nothing at `k` covers nothing beneath it.
Under `Collision` they can: an authored deny at `/a/b` holds `/a/B/x`, which a
side that merely did not mention `/a/b` may allow through `/a/B`.

On a byte-exact host take `a = {allow dir /a/B}` and
`b = {allow dir /a/B, allow dir /a/b}`. The pointwise table has `/a/B ↦ allow`
and `/a/b ↦ deny`; at `/a/B/x` both layers allow, yet the table matches `/a/B`
(Allow) and, by collision, `/a/b` (Deny) at one rank, and the tie denies. Files
have the same shape. The kernel renders every met key too, so Seatbelt would
receive `(deny process-exec (subpath "/a/b"))` beside the allow and match it
caselessly, refusing a child what the guard admits (this follows from the
measured fold, [[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]];
not measured on its own).

**The meet.** Over the union of keys of each of `files`, `tools` and `dirs`:

> `M[k] = T_a(k) ∧ T_b(k)`, kept iff it is not Deny, or `a[k]` or `b[k]` is
> itself a Deny. Vetoes union.

`T(k)` is `verdict` for files and tools, `dir_verdict` for dirs. Equivalently
*denies join* (`D_M = D_a ∪ D_b`) and *allows meet* (`k ∈ A_M` iff a side
allows `k` and both sides allow at `k`): the lattice shape fs has in
`PrefixSet<Deny>::union` and `PrefixSet<Allow>::meet`. A key met to Deny that
nobody denied is left to default-deny, which is absence.

**The law**, for every program `p`:
`verdict(a ∧ b, p) = verdict(a, p) ∧ verdict(b, p)`.

*Proof sketch.* Write `dv_T(k)` for what `T`'s dirs say at `k`.

- *Dirs, a side denies.* Say `a`. Either (i) its top speaker at `p` is a deny
  `d ∈ D_a`, or (ii) nothing in `a` speaks.
  (i) `d ∈ D_M`. Suppose `k ∈ A_M` with `p ⊑S k` is deeper than `d`; both
  are collision-ancestors of `p`, so `k ⊑C d`. Were `k ∈ A_a`, `a` would
  allow at `p` above `d`; so `k ∈ A_b` and `dv_a(k)` is Allow, which needs an
  `a`-allow `k″ ⊒S k` deeper than `d` (`d` holds `k` and wins ties), and `k″`
  covers `p`: a contradiction. So no allow of `M` outranks `d` at `p`, and
  one at its rank ties to Deny.
  (ii) No `a`-allow covers `p`, hence none covers any `k ⊒S p`, so no
  `k ∈ A_M` covers `p`, which would need `dv_a(k)` Allow: `M` denies by
  default.
- *Dirs, both allow*, through top allows `k_a`, `k_b` of ranks `r_a ≥ r_b`. A
  deny holding `k_a` holds `p`, so every `a`-deny holding `k_a` is shallower
  than `r_a` and every `b`-deny shallower than `r_b`; and `k_b ⊒S k_a`. So
  `dv_a(k_a) = dv_b(k_a)` = Allow: `k_a ∈ A_M` at rank `r_a`, and every deny
  of `D_M` that holds `p` is shallower. `M` allows.
- *Files.* An authored deny file `f` has verdict Deny at `f` (`Exact` ties
  meet and only a veto outranks it, which denies), so "kept iff not Deny or
  authored Deny" is exactly "denies join, allows meet". A deny file naming `p`
  on either side is in `M` and names `p`: Deny. Otherwise only the key `p`
  itself can speak on a side. If both sides are non-Deny at `p`, `M[p]` is
  kept and is the sole `Exact` speaker. If one is Deny at `p` with no exact
  rule, `M[p]` is dropped, no other key of `M` names `p` (kept allows are
  stored-exact, kept denies authored and none names `p`), and the dirs
  argument gives Deny.
- *Vetoes* union and deny on every side; *tools* are exact names, unchanged
  in substance and met by the same function. ∎

A randomised model of the tables (dirs, files, vetoes, `Only`, ranks,
lower-casing as the fold) satisfies the law, and the meet is associative and
idempotent on it; the pointwise meet breaks the law on 3,869 of 20,000 random
pairs. The in-tree property test checks the law over a fixed universe of case
and normalisation variants, and a second checks the kernel's last-match-wins
list against `verdict`.

## Rank under a fold

- **A folded match ranks at the rule's own depth**, as a stored match of the
  same key: `holds` decides whether a rule speaks, never at what rank. Ties
  still meet, so a deny at an allow's rank wins, and a deeper allow beats a
  shallower deny, folded or not.
- **`Exact` beats `Dir`.** A file rule names its program whatever directory
  denies it. This is exec's own law, and the one place it differs from fs,
  where a deny wins at any depth: `deny dir /x/B` with `allow file
  /x/b/tool` admits `/x/b/tool`, though a user coming from fs may expect Deny.
- **A veto's key is `command_name_key`**, the host's name key under
  `collision_key`, and ranks above everything.
- **`evicts` asks `Collision` both ways.** Mutual collision containment is one
  rank, where the guard's tie already denies, so the widening sweep that drops
  an allow dir a deny dir clashes with agrees with the verdict; a deny on a
  child dir does not evict its parent's allow.

### Rejected

- **A three-state dir key** (allowed, authored deny, unset). It judges
  verdict for verdict as above: "unset" is a key that must never speak and
  never render, which is what absence already is. It keeps writing defaults
  into a table that then needs a third arm in `covering`, `kernel`, the
  `last_match` oracle and `ExecRule`. The meet is where the default is
  manufactured, so the meet is where it is stopped.
- **Deny-always-wins, as fs has it.** It changes exec's precedence for reasons
  unrelated to spelling, and breaks a file rule over a covering deny dir and
  the deepest dir winning.
- **A secondary index by collision key.** It buys lookup speed the tables do
  not need, and a second place to keep right. A scan over a handful of keys
  decides.

### Worked cases

On a case-sensitive volume, where `Tools` and `tools` are distinct
directories. On a case-insensitive one each pair is a single object with one
stored spelling, `realpath` returns it, and the answers are those the author
expects of the stored side; on Windows the ASCII-case pairs are one key already,
and the fold adds only non-ASCII case and normalisation. *Respelled* marks a
refusal whose message names the denied spelling, because no deny holds the
program as stored.

| Grant | Program | Verdict | Why |
|---|---|---|---|
| allow dir `/x/Tools`, deny dir `/x/tools/evil` | `/x/Tools/evil/t` | Deny, respelled | the deny is deeper and holds `/x/Tools/evil` by collision; `/x/Tools/good/t` is Allow, only the allow speaking |
| allow dir `/x/b`, deny dir `/x/B` | `/x/b/t` | Deny | one rank, the tie meets; fs's `outside` drops the allow likewise |
| deny dir `/x/B`, allow file `/x/b/tool` | `/x/b/tool` | Allow | `Exact` over `Dir`; fs has no such rank |
| stack `a = {allow /a/B}`, `b = {allow /a/B, allow /a/b}` | `/a/B/x` | Allow | the counterexample above: `/a/b` is not written as a deny |
| stack `a = {allow /a, deny /a/B}`, `b = {allow /a/b, allow /a/c}` | `/a/b/x` | Deny, respelled | `/a/B` joins the met denies and ties `/a/b`; `/a/c/x` is Allow |
| two layers, one allowing `/a/B`, the other `/a/b` | `/a/B/x` | Deny | each allows only its spelling and the meet keeps neither: allows meet by stored name, as in fs |

## Consequences

- A symlink to an admitted file is admitted: it runs that file. A symlink
  under an admitted directory to a file outside it is not.
- A bare key is the host-`PATH` file, so `git: allow` admits `/usr/bin/git`
  typed as a path, and a scoped `PATH` that plants another `git` inherits
  nothing.
- A multi-call binary is one file, so a path key cannot tell its applets
  apart; a bare deny still vetoes by name.
- Missing is 127 and non-executable 126 under any grant, never a denial. On
  Windows a path head written without its extension that names no file is
  127, with a hint naming the suffixed file if one exists. Path keys there
  match by `RealPath` identity; the extension strip survives only in
  `command_name_key`, for bare names.

## Declared limits

- **A copy under a new name escapes a veto.** It is a different file holding
  no trace of the denied name, admitted wherever an allow dir covers it. What
  holds is the projection the copy inherits ([[design/grant|grant]]
  §Concessions).
- **The kernel cannot see argv or which bundled tool runs.** `Only` is an
  allow there, and a tool is ral's own binary.
- **Windows has no kernel exec layer**, so the window between check and exec
  stays open there.

## Where

`core/src/types/capability.rs` (`ExecGrant`, `Verdict`,
`meet_insert`), `core/src/capability/decode.rs` (keys to `ExecGrant`),
`core/src/capability/exec.rs` (`ExecRules`, `Rank`, `Program`, `Subject`),
`core/src/capability/enforce.rs` (`check_exec`, `admits_head`, `Admitted`),
`core/src/runtime/command/head.rs` (`Head`, `Missing`),
`core/src/path/real.rs` (`RealPath`, `within::<P>`),
`core/src/path/resolved.rs` (`evicts`),
`core/src/path/which.rs` (`command_name_key`).
