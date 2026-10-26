---
generated_at_commit: 1e3124cbc0ac2352d44c9d743a70b1cde5081361
---

# Authority is a table of scoped rules

**Each dimension of a grant that ranges over places, fs and exec, denotes a
function from subjects to Belnap evidence, and is represented by one
structure: a finite table of scoped rules, read as "an exclusion beats every
allowance; silence permits nothing".** A layer is a table, a stack is the meet
of its layers' tables, and the meet is *defined* by the pointwise law
`ev(a ∧ b, s) = ev(a, s) ∧ ev(b, s)`. The fs region and the exec rules are two
instances of it (`core/src/authority/policy/table.rs`); everything that differs
between them is a choice of scope and subject. Authority is a policy in the
sense of Bruns and Huth's PBel, in a small fragment of it
([[related/access-control-algebra|access-control-algebra]]). Why the meet is a
stack fold and not a flattened grant is
[[decisions/260906_object-not-name|object-not-name]]; why the two dimensions
became one structure is
[[decisions/261006_one-table-two-instances|one-table-two-instances]]; why
evidence, and what it cost, is
[[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]].

## The structure

- **Evidence.** For a fixed request, `Evidence { positive, negative }` is a pair
  of Booleans, Belnap's FOUR as a product bilattice: `(0,0)` Neither, `(1,0)`
  Allow, `(0,1)` Deny, `(1,1)` Both. Neither is not an authored deny, and Both
  is not malformed: allowing a directory and excluding a subtree deliberately
  produces Both inside the subtree.
- **The decision.** Enforcement is PBel's wrapper `↓`: permit exactly when
  `positive ∧ ¬negative`, applied once, at the boundary that enforces
  (`Evidence::permits`). Nothing below it closes a gap or a conflict.
- **Requests, not strings.** Policies are functions of resolved requests: an fs
  request is a resolved path with an operation; running a host file is a resolved
  file (and a read of it); running a bundled tool is its name plus a read of
  ral's binary. A bare name is resolved before its meaning meets another layer's
  directories. Belnap changes how evidence combines, not what a name denotes: no
  case folding, canonicalisation or containment code lives outside `path/`.
- **Subjects** `s ∈ S`: what a table judges. For fs, a `path::Subject`: an
  access by its transcript (`Trace`), a position, or everywhere; for exec a
  `Subject`: `Any`, a bundled tool by name, a program by where it is, or a
  directory's inside.
- **Scopes.** A table is two sets, `allows` of positive scopes and `denies` of
  negative ones (`Table<A, D>`), each `Ord`, each of one polarity by its type.
  Evidence is computed from the sets, never stored: a key cannot hold Both,
  though a subject can, and no table holds Neither.
- **Polarity picks the reading.** An allowance (`Allows::contains`) holds `s` by
  position, names as stored (`⊑S`). An exclusion (`Denies::matches`) holds it by
  position under every spelling some filesystem takes for the name (`⊑C`,
  `collision_key` per component), *or by the object its walk held*, wherever
  `s` reaches it from (`MatchedBy::Identity`). `⊑S ⊆ ⊑C`, both transitive and
  firmlink-aware; identity is not transitive, and positive containment must be,
  so an allowance cannot hold by identity: the two readings are two traits on
  two types ([[invariants/grants-judge-objects|grants-judge-objects]],
  [[internals/path-judgment|path-judgment]]).
- **Own subject.** `k̂ = k.own()` is the subject an allowance is itself judged
  at: a prefix's position, a directory's inside, a file itself, a tool's name.

**Denotation.**

```
supports(s)  = ∃ k ∈ allows(T). k contains s
excludes(s)  = ∃ k ∈ denies(T).  k matches s
ev(T, s)     = (supports(s), excludes(s))
```

There is no specificity: a rule's depth, kind and spelling decide nothing, only
whether it speaks. One method answers a refusal, `Table::withheld`, which
returns `None` exactly when `s` has positive evidence alone, otherwise
`Withheld::Excluded { scope, matched_by }` or `Withheld::MissingAllowance`. The
guard asks it, the projection renders what it admits, and no other code makes a
decision.

**The witness.** `withheld` looks only at exclusions that match the subject, so
it never cites one that does not exclude; among them it cites the one that
matches the least way (`MatchedBy`: `Name`, then `FoldedName`, then `Identity`),
in stable key order. The choice is deterministic, not by depth. Its one
guarantee is that the cited exclusion applies.

**A missing allowance is never an exclusion.** An absent positive candidate
leaves Neither, which refuses under `↓` but is not stored as negative evidence;
an empty written table is supplied and denotes ⊥ everywhere, and a later
fragment can still supply its allowance.

## The meet: exclusions join, allowances meet

```
a ∧ b = { (k, Deny)  : k ∈ denies(a) ∪ denies(b) }
      ∪ { (k, Allow) : k ∈ positives(a) ∪ positives(b),
                       a.supports(k̂) ∧ b.supports(k̂) }
```

Every exclusion either side wrote is kept; an allowance survives at its own
subject where *both* sides support it. **No default is written as an
exclusion.** The obvious pointwise representation stores, at every key of
either side, both sides' evidence met, which writes Deny at a key one side
never mentioned: a *default* recorded as an *authored* exclusion. Under `⊑S`
the two are indistinguishable; under `⊑C` they are not, because a written
exclusion holds every spelling of its name. On a byte-exact host take

```
a = { allow /a/B }      b = { allow /a/B, allow /a/b }
```

Both layers allow `/a/B/x`. The pointwise table holds `/a/B ↦ Allow` and
`/a/b ↦ Deny`; at `/a/B/x` the manufactured deny holds by `⊑C` and the subject
becomes Both. The kernel would render the same key, and Seatbelt, matching
caselessly, would refuse a child what the guard admits. The meet is where the
default is manufactured, so the meet is where it is stopped
(`ral/tests/capabilities.rs::a_stacked_one_sided_allow_denies_no_other_spelling`).

**The law.** For every subject `s`, `ev(a ∧ b, s) = ev(a, s) ∧ ev(b, s)`,
Neither and Both included. Agreement only after `↓` is too weak. It holds of
any scope set satisfying three obligations, which the implementation documents
and both instances meet:

- **(1) self**: every positive scope holds its own subject;
- **(2) nesting**: two positive scopes with a common subject are nested;
- **(3) containment**: if `a` holds `own(k)` and `k` holds `s`, then `a` holds
  `s`.

Prefixes, run places, tools and `Any` satisfy them; `Name` is negative only.
Each scope has one polarity by its type, not by masking: a `Name` veto is an
`ExecDeny` and `Any` an `ExecAllow`, and `every` has no negative form. A future
positive scope that does not nest needs an explicit intersection first.

*Proof sketch.* Fix `s`.

- *Negative bit.* The meet keeps every exclusion, so it excludes `s` exactly
  when either side does: `n = n_a ∨ n_b`.
- *Positive bit, ⇒.* A positive scope `k` of the meet holds `s` and both sides
  support `k̂`: some positive `j` of `a` holds `k̂`, and by (3) holds `s`. So
  `a` supports `s`, and `b` likewise.
- *Positive bit, ⇐.* Let `j` of `a` and `j′` of `b` both hold `s`. By (2) they
  are nested, say `j` inside `j′`. Then `j′` holds `j̃ = own(j)` (the nesting),
  so `b` supports `j̃`; `a` supports `j̃` since `j` holds it (1). So `j`
  survives in the meet and holds `s`. ∎

The generic law is mechanised in `dev/agda/Core/Authority/Table.agda`
(`denote-meet`), using the separate Belnap algebra in `Core.Belnap`.
`Core.Authority.Prefix` discharges the three obligations for resolved component
prefixes, with negative matching independent of the positive relation. The
exec and fs scope constructors are modelled in `Core.Authority.Exec` and
`Core.Authority.Fs`, which discharge the scope laws from a common resolved-path
geometry and enforce polarity restrictions in their types. The
proof uses lists of legal positive and negative scopes and establishes
denotational equality; correspondence with Rust's concrete path operations,
map normalisation and the machine's grant semantics remains to be proved.

The pointwise law gives commutativity, associativity and idempotence of
denotations. A table's representation, two ordered sets, also satisfies these
laws *structurally*, including `a ∧ a = a`; this stronger claim is checked by
property tests over a universe of directories, files,
tools, vetoes and `Any`, built over a tempdir tree of real links and renames and
holding case pairs and an NFC/NFD pair, so a name one filesystem keeps apart and
another merges (`laws` in `authority/policy/table.rs`).

## Laws

These hold of denotations; representations may hold redundant scopes. The
scalar and table laws (1 to 6) are stated of evidence and tables, never of
stacks.

1. `⊕` and `∧` are associative, commutative and idempotent on evidence, with
   identities ⊥ and `t`.
2. `↓(P ∧ Q) = ↓P ∧ ↓Q`.
3. A negative bit, once present, survives `⊕` and `∧`. This is why no
   composition followed by `↓` tells Both from Deny.
4. **Extension invariant.** For base `B`, fragment `F` and request `s` on any
   axis: if `F` has no negative evidence at `s`, `↓B(s)` implies
   `↓(B ◁ F)(s)`; if `F` has negative evidence at `s`, `↓(B ◁ F)(s)` refuses;
   and `↓(B ◁ F)(s)` implies `↓B(s)` or positive evidence of `F` at `s`. `F`'s
   evidence includes the reads of what its run allowances name. It is a law of
   *policy denotations*: it holds of the guard and of the policy part of every
   kernel table, not of a kernel table as a whole, since support depends on the
   launch and on which files are live (an allowance for a script can admit its
   interpreter, and excluding the script can withdraw it). Nor is it a law of
   launches: excluding a linker or a carrier refuses programs the fragment says
   nothing about.
5. **A table's evidence is the pointwise operation of its operands'**, Neither
   and Both included: `ev(P ∧ Q, s) = ev(P, s) ∧ ev(Q, s)`, likewise for `⊕`.
   Agreement only after `↓` is too weak.
6. **Evaluation never stores ⊥ as negative evidence.** Mapping an empty
   fragment to an explicit denial would prevent a later fragment from supplying
   its allowance.
7. **Running reads**: `↓run(s) = ↓exec(s) ∧ ↓read(s)` for a host file, where
   `read` includes the write prefixes and the run places each layer looks down
   to; for a bundled tool `t`, `↓run(t) = ↓exec(t) ∧ ¬read.negative(ral)`. ral's
   binary is a host file under the first equation, like any other.
8. **Writing reads**: `↓write(s)` implies `↓read(s)`, with no check, since every
   layer that supplies the write pairs it into its own reads.
9. **Redundant scopes denote nothing extra.** A layer's denotation includes its
   run places: `every` and `every ⊕ (t if run s)` run the same programs but read
   differently.
10. **Restriction attenuates**: for every stack `S`, layer `R` and request `s`,
    on every axis and for running and reading alike, `↓(S ∧ R)(s)` implies
    `↓S(s)`. Pairing looks down the stack so that this holds: a pushed layer
    adds a conjunct and changes none beneath it.

Laws 4, 7, 8 and 10 are why roles-as-types, writing reads, verb `Join` and
looking-down pairing exist; several departures of the old implementation broke
law 4
([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]).

## Why FOUR, net, and verbs

- **Why FOUR is kept.** Within ral's fragment (`⊕`, `∧`, `↓`) no composition
  followed by `↓` tells Both from Deny, since a negative bit survives both
  operators. The carrier stays FOUR because PBel's other operators do tell them
  apart (`¬⊤ = ⊤` but `¬f = t`; `⊤ ⊗ t = t` but `f ⊗ t = ⊥`), and offering one
  later must not change the representation. So `[⊤ ↦ f]` is never applied below
  the enforcement boundary, and every law is stated of evidence, Both included.
- **Net is a decision, not evidence.** `` `offline `` stands for every value with
  its negative bit set, so `t ⊕ Offline` (⊤) and `Offline` itself (f) share one
  representation. This is the one place below enforcement where ⊤ and f are
  identified, sound only because no operator offered on net tells them apart; if
  one is ever offered, net becomes an evidence pair like the other axes. An
  omitted `net` is `t`; restriction is unchanged. `Gpu` is the same shape:
  `` `withheld `` is the one opinion.
- **Verbs are sets of positive evidence.** `editor` and `shell` are each a set of
  verbs: a listed verb is positive evidence, an unlisted one is Neither, never
  Deny. `⊕` is union and `∧` is intersection, so after `↓` a verb is permitted
  only where every supplied layer lists it. Under extension verbs join by union,
  and since verbs carry no exclusions, an omitted base axis absorbs any fragment
  (`join_top` is `t`). Withholding a verb is a restriction's job; there is no
  verb exclusion, so a base cannot withhold a verb, `detach` included, against an
  extension.
- **Evidence is never closed early.** A table stores evidence and the question
  asks it; nothing stores a decision. Evaluation never stores ⊥ as negative
  evidence, so an empty fragment cannot block a later one's allowance.

## Extension: roles as types

An omitted axis denotes the identity of the role it plays: in a *policy* (a
base, a restriction layer, the ceiling, the stack) it is `t`; in an *extension
fragment* it is `⊥`. The two roles are two types: `GrantLayer` is a policy,
the loader returns a `Fragment`, and a `Fragment` is consumed only by
`GrantLayer::extend`. Reading an omission in the wrong role is a type error.

`B ◁ F`, per axis: any `B` with omitted `F` is `B`; omitted `B` with supplied
`Q` is `t ⊕ Q`, every request less `Q`'s exclusions (`Axis::join_top`); supplied
`P` with supplied `Q` is `P ⊕ Q`. On exec, `join_top` sets `every` and keeps
`Q`'s allowances, which change nothing about running but still name run places.
The shape of every enforced policy is `↓(E₀ ∧ E₁ ∧ … ∧ Eₙ)`: `E₀` elaborates
the ceiling `B ◁ F`, each `Eᵢ` a restriction, every `⊕` before every `∧`.

**Why order matters.** Elaboration is ordered and contextual on one point only:
on fs reading, a layer reads the run places of the exec meet over itself and the
layers beneath it. Everything else in a layer's elaboration is its own evidence.
The meet of elaborated layers is the ordinary pointwise meet, commutative and
associative, but the order of the stack still matters, because the same layer
elaborates differently above and beneath another: a layer that reads nothing and
omits exec, beneath a layer that runs `File(s)` and omits fs, refuses `s`; the
same two layers the other way round permit it. Law 10 follows: pushing a layer
appends a conjunct and elaborates none beneath it anew.

**An extension after a restriction could undo it**: `(B ∧ R) ⊕ E` permits what
`R` withholds. The order holds structurally: pushing a layer is `∧` and a pushed
layer is immutable, while `◁` takes a `Fragment`, which only the loader
constructs, and runs once, in exarch's `for_invocation`, before the first push.
One extension file is an interface choice; several would fold by `⊕` before
extending the base. `◁` itself resolves no name: a fragment is a layer, resolved
where it was decoded, and the ceiling holds its rules. Its equations are
relative to the host state at which the fragment's paths were walked, as every
layer's are.

**Representation per axis.** `Axis: Join` has `join_top` (`t ⊕ self`, `None`
where omission loses nothing) and `is_top` (denotes `t`):

| Axis | `join` | `join_top` | `is_top` |
|---|---|---|---|
| `ExecRules` | union of the tables | `None` for an empty table; otherwise the same rules with `Any` among the allows, whose other allowances still name run places | `Any` and no exclusion |
| `FsLayer` | union each table | `None` without exclusions; otherwise `Everywhere` among the read and write allows, the same exclusions | `Everywhere` among the write allows, no exclusions |
| `Net` | `Offline` | `Some(Offline)` | never |
| `Gpu` | `Withheld` | `Some(Withheld)` | never |
| verb sets | union | `None` | every verb listed |

`join_top` on exec keeps its result even where it denotes `t`, because its
allowances still name run places; elsewhere a result that denotes `t` is `None`.
`GrantLayer::is_restrictive` is "some supplied axis is not `is_top`". `Meet`
and `Join` are the two bare operations, implemented in one shape by `Evidence`
and `Table` (both) and by the axes (`Join`); `Widen` is deleted and the
symmetric `Option<T>` lift with it. `Evidence`'s `Default` is Neither, the
identity for collecting evidence; restriction starts from `t` explicitly, never
from `Default`; it derives no `Ord`, neither order being total. A written empty
table (`exec: [allow: []]`, `fs: [read: []]`) is supplied: ⊥ for every request,
which on fs leaves the floor alone; being empty never makes a table omitted.

## Reading what runs and what is written

- **Writing reads.** A layer's read evidence is its read prefixes joined with
  its write prefixes.
- **Running reads, looking down the stack.** The run places of an exec table are
  its `File` and `Dir` allowances, as resolved positions read as prefixes
  (`FsAllow::RunPlace`; `ExecRules::places`); `every` and bundled tools have none.
  For a layer `L` that supplies fs, `P≤L` is the set of run places of the exec
  meet over `L` and every layer beneath it. So a grant reads what it, and the
  grants enclosing it, let run by path, directory or name, and a nested grant
  never makes something readable by naming it: pushing a layer appends a conjunct
  and changes none beneath it. An fs-only restriction nested under a grant that
  runs `cargo` leaves `cargo` readable and runnable unless it excludes it.
- **Exclusions do not pair**: refusing to run something does not refuse reading
  it, and a scope holding Both pairs its positive half. Running and reading
  couple in no other way; no write follows from either. Pairing matters only
  where fs is supplied: an omitted fs is `t`.
- **The read region.** With `P≤L` the places above, `floor` the platform's read
  floor and `D` the supplied denies,
  `Aread = ⋀_L (read_L ∪ write_L ∪ P≤L) ∪ floor`, over the fs-supplying layers
  `L`. Joining the floor into each layer before the meet equals joining it once
  after; an exclusion of a floor path or a place still refuses it; with no
  supplied fs there is no floor-only restriction. Universal positive support on
  fs is `Extent::Everywhere`, so `t ⊕ Q` is read `Everywhere`, write
  `Everywhere`, deny `Q`'s denies. Discard devices and the pinned-binary write guard
  keep their own rules.
- **Why two calls, and what they are exact for.** `fs::region` runs on every fs
  check and `reasonable` has about a hundred names, so permitted reads compile
  nothing. Places add only positive evidence and the meet is monotone in it, so a
  permit or an exclusion from the first call is final, with the same witness; the
  second call can only turn a missing allowance into a permit. That is exact for
  enforcement, not for evidence: a place can turn Deny into Both, which `↓` never
  distinguishes, and the first call's table is never composed further. One
  method, `GrantStack::read`, serves the fs guard, the running-reads check at
  exec and the bundled-tool check of ral's binary; the projection calls
  `region(…, true)` once per spawn.
- **Places are prefixes of a real path.** A place reads as a prefix whether exec
  runs it as a file or a directory, so containment is transitive across every
  pair of scopes and the meet stays exact without shaped probes. A file place
  later replaced by a directory reads its tree, in the guard and every kernel
  alike; making that replacement needs a write to its parent, whose reading law 8
  already grants, or the host.
- **A fragment pairs nothing on its own.** `join_top` keeps a fragment's exec
  allowances beside `every`, so the ceiling's table holds them, and a name in an
  extension is resolved where the fragment is decoded like every other name.
- **A toolchain is written once, in exec.** `reasonable` lists a `dir` selector
  per toolchain and no read entry for what a selector runs, running reads
  covering it ([[design/grant|grant]] §Selectors); it keeps `fs` entries only
  for caches and registries.
- The guard keeps compilation off permitted reads: the read judgment
  (`GrantStack::read`) asks `fs::region(…, places: false)` and only on
  `MissingAllowance` `region(…, true)`; places add only positive evidence, so a
  first-call permit or exclusion is final.
- **ral's binary is an ordinary host file**, run where a grant admits it and
  read through pairing like any program. A *bundled tool* runs ral's image, so
  it reads it: an fs exclusion of ral's binary refuses every bundled tool, on
  every platform, and an exec exclusion leaves them alone. Nothing admits ral
  beyond the policy and nothing excludes it beyond the policy.
- **Refusals** are `ExecDenial::{Run, Read}` and
  `Refused::{Guarded, Withheld}`, each carrying a `Withheld` reason.

## Activation is denotational

A layer is restrictive exactly where some axis does not denote `t`
(`Axis::is_top`), a stack exactly where some layer is. Exec restricts unless it
is `every` with no exclusion; fs unless it writes the root and excludes nothing;
net when `Offline`; a verb set unless it lists every verb. Every reader goes
through it: the projection, launch hardening, exarch's credential layer and the
summaries. An `every` table with exclusions activates.

## What a table hands out

- **`live()`**: the positive scopes still permitted at their own subjects, for
  which descendants may still be excluded. For fs, the lists a backend renders;
  for exec, the files and directories the kernel admits.
- **`denies()`**: the scopes written as exclusions.
- **`matched_by`** on `Withheld::Excluded`: the least way the exclusion holds
  `s`. `FoldedName` says it holds only by a fold, so the author learns the deny
  holds through case or Unicode form:
  `fs write denied by grant: /w/.ENV is the denied /w/.env under another spelling (case or Unicode form); a deny holds under every spelling`;
  `Identity` says it holds the object under another name. A veto's key is folded
  already, so a vetoed name is refused plainly.

## Two instances

| | fs: `FsRules = Table<FsAllow, FsDeny>` | exec: `ExecRules = Table<ExecAllow, ExecDeny>` |
|---|---|---|
| allowance `A` | `FsAllow { Prefix(GrantPath<Extent>), RunPlace(RealPath) }` | `ExecAllow { Any, Dir, File, Tool }` |
| exclusion `D` | `FsDeny(GrantPath<Excluded>)` | `ExecDeny { Name, Dir, File }` |
| subject | `path::Subject`: an access by its `Trace`, a position, or everywhere | `Subject { Program, Dir, Tool, Any }`, from a `Program` |
| `k̂` | the position; everywhere for `Everywhere` | `Any`, the directory's inside, the file itself, the tool's name |
| an allowance holds | by position, `RealPath::contains`; a place reads as a prefix whether exec runs it as file or directory | by position; a file by mutual containment; a tool by name; `Any` holds everything |
| an exclusion matches | `Excluded::matches` within: position under `collision_key`, or the object | the same, within for a directory and exactly for a file; a name by `command_name_key` |
| where the exclusions live | each layer's read and write tables both hold every exclusion | in the table, beside the allowances |
| tables per stack | two, one per `FsOp`, `None` when no layer holds an fs opinion | one, `None` when no layer holds an exec opinion |
| resolved | where the layer is decoded: allowances to a position, exclusions to the objects held; never again | the same; a bare name on the decoding environment's `PATH` |
| extra scopes | places of exec's run allowances; the platform floor and live support, joined as positives | support files, joined by `for_kernel` |

**An exclusion beats every allowance, in both.** An fs exclusion is a carve-out
protecting *content*: credentials under a readable `` `xdg-config ``, the agent's
profile under a writable cwd. An exec exclusion names a *program*. Neither a
narrower directory nor an exact file reopens it, and nor do a carrier, the
dynamic linker or ral's binary. This is the one rule every backend can hold: a
bwrap mask hides the whole tree, an ACE inherits down it, a Windows ACL orders
explicit allows before inherited denies, and Seatbelt emits the negative
clauses after the positive ones.

**Exec has two languages, one map each, and the kernel reads the second.**

- `ExecKey { Name, Path, Dir }`: the grant *as written*. `ExecGrant` is a
  private `BTreeMap<ExecKey, Evidence>` plus the flag `every`, built only by
  joining (`FromIterator`, `Join`, `join_top`, the wire decoder). It rides the
  wire, and exarch's prompt prints it through `Display`, a dir as `` `dir ``.
  `every` is not a key: only `join_top` sets it, so no surface spelling widens,
  and a flag has no negative form, so "every program" can never be excluded.
- `ExecAllow` and `ExecDeny`: the grant *resolved where it is decoded*
  (in `authority::grant`): a bare name becomes the bundled tool of that name,
  the file the decoding environment's `PATH` finds and, for negative evidence
  alone, a `Name` veto; a path key a `File`, a dir key a `Dir`; `every` the
  allowance `Any`. A key walks as its polarity walks: the positive bit to the
  position it reached, the negative to the objects it held; a key with Both
  yields both entries. A name found nowhere makes no allowance and keeps its
  veto. The veto carries only the negative bit, since a
positive one would assert support for every executable sharing the basename: an
allowance by name leaves no veto entry at all, and Both leaves exactly Deny, the
positive half living on in the tool and file entries. One executable reached by
several spellings accumulates evidence. A bare name resolves through the
decoding `PATH` and the bundled-tool registry; a scoped `PATH` cannot redirect an
allowance.

A bare key is no scope until a `PATH` is consulted, so an authored grant is not
closed under meet; the layer is resolved where it is decoded and the meet runs
over what it holds ([[decisions/260906_object-not-name|object-not-name]],
fault B). The model sits
below the sandbox ([[decisions/261006_capability-is-data|capability-is-data]]),
so `ExecProjection::Restricted` carries the `ExecRules` itself.

**What the kernel is handed.** `for_kernel(support)` drops the `Tool` scopes and
joins the live support as `File` allowances, giving again an `ExecRules`. The
kernel sees no tools and no argv, so the table is its poorer view of the same
evidence. Seatbelt renders every positive clause, then every negative one; its last-match-wins puts every exclusion on top by
construction, and order within a group is for stable output only. Landlock
renders allowances only, from `live()` and `permits()`, subtracting exclusions
along their spines before it issues rules, and `Any` lowered against the
envelope's binds. The one judge of the expansion that renders a block inside an
allowed directory is the table, so an exclusion by another spelling holds in the
kernel as in the guard
([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]).

## Support

**Support is what a launch needs beyond the policy to start a program:
carriers and the dynamic linker. It yields to every exclusion.** With `S` the
support set, the live support is
`S′ = { s ∈ S | ¬exec.negative(s) ∧ ¬read.negative(s) }`, and on each axis the
kernel's evidence at `s` is `(policy.positive(s) ∨ s ∈ S′, policy.negative(s))`.

- Support grants running and reading, never writing.
- It is a function of the launch: the launched file's carriers, and where exec is
  restricted every live file's carriers and the Linux linker candidates. It is
  computed for every launch with a restricted fs or exec, joined into both
  tables as allowances, so no backend knows support as such.
- The guard consults none. Support is therefore not a law of denotations: an
  allowance for a script can admit its interpreter, and excluding the script can
  withdraw it; excluding a linker or a carrier refuses programs the policy says
  nothing about.
- ral's binary is not support.

### Worked cases

On a case-sensitive volume, where `Tools` and `tools` are distinct
directories. On a case-insensitive one each pair is one object with one
stored spelling, `realpath` returns it, and the answers are those of the
stored side. *Folded* marks a refusal cited `MatchedBy::FoldedName`, naming the excluded
spelling.

| Grant | Subject | Decision | Why |
|---|---|---|---|
| allow dir `/x/Tools`, exclude dir `/x/tools/evil` | `/x/Tools/evil/t` | refused, folded | the exclusion holds `/x/Tools/evil` by `⊑C`; `/x/Tools/good/t` is allowed |
| allow dir `/x/b`, exclude dir `/x/B` | `/x/b/t` | refused, folded | Both; fs's `live()` drops the allowance likewise |
| exclude dir `/x/B`, allow file `/x/b/tool` | `/x/b/tool` | refused, folded | an exact file does not reopen an exclusion |
| allow file `/x/tool`, exclude file `/x/Tool` | `/x/tool` | refused, folded | Both |
| allow dir `/w`, exclude dir `/w/private` | `/w/private/k` | refused | Both inside the subtree; `/w/other` is allowed |
| stack `a = {allow /a/B}`, `b = {allow /a/B, allow /a/b}` | `/a/B/x` | allowed | the counterexample: `/a/b` is not written as an exclusion |
| stack `a = {allow /a, exclude /a/B}`, `b = {allow /a/b, allow /a/c}` | `/a/b/x` | refused, folded | `/a/B` joins the met exclusions; `/a/b` and `/a` are dropped, each unsupported by the other side; `/a/c/x` is allowed |
| two layers, one allowing `/a/B`, the other `/a/b` | `/a/B/x` | refused | each allows only its spelling and the meet keeps neither |
| stack `a = {allow /a, exclude /a/b}`, `b = {allow /a/b/c}` | `/a/b/c/x` | refused | a deeper allowance in one layer cannot lift an exclusion another places above it |
| allow dir `/x`, veto `sh` | `/x/SH`, `/x/ſh` | refused | a veto's key folds; `/x/shx` is allowed |

## Corner cases, and why the design is what it is

### Spelling: which names are one name

- **APFS folds by canonical caseless matching.** Measured on macOS 26.5,
  default (case-insensitive) APFS takes two names for one exactly when they
  agree under Unicode D145, `NFD(casefold(NFD(s)))`, on all 18 probe pairs:
  ß≡ss, ß≡ẞ, ſ≡s, K (Kelvin)≡k, ﬁ≡fi, σ≡ς, µ≡μ, Å≡Å (angstrom) and NFC≡NFD
  merge; ı/i, ı/I, İ/i and fullwidth Ａ/a stay distinct. Case-sensitive APFS
  merges canonical equivalents only.
- **Only an absent name keeps its written spelling.** `realpath(3)` and
  `F_GETPATH` return the stored spelling of an existing name, for case and
  normalisation, on both volume kinds; the macOS walk spells components as
  stored (`getattrlistat`). An absent leaf has no stored spelling, and that is
  exactly the write a deny exists to stop: under `` deny: [`cwd .env] `` with
  `.env` absent, `echo key > .ENV` once left `key` in the object `.env`
  names. Every later access was refused; the hole was the access that creates
  the object ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]).
- **Seatbelt matches `literal`, `subpath` and `regex` under the same
  equivalence on every volume, absent and existing names alike, and folds
  allows too.** On a case-sensitive volume, where `Work` and `work` are
  distinct objects, `subpath Work`, `subpath WORK` and `literal …/Work/t`
  each admit reading `work/t`: a Seatbelt allow over-grants a child across
  case there. SBPL cannot say otherwise; it is a stated residual. Hence
  **macOS renders nothing differently**: on APFS the classes the key adds
  beyond Seatbelt's are distinct objects. On an OpenZFS volume under macOS
  they are not, and a child can create an absent denied name spelled with ı
  or a compatibility form: a stated residual.
- **NTFS upcases each UTF-16 unit through `$UpCase` and does not normalise**,
  and its table keeps ı apart from I, though Unicode's simple uppercase joins
  them: measured on the Windows runner, `fıle` and `FILE` are two names.
- **ZFS upcases by Unicode 5.0's simple uppercase, then normalises as the
  dataset says**, both fixed at creation (OpenZFS `u8_textprep`, its tables
  decoded). `casesensitivity=insensitive` or `mixed` sends ı to I;
  `normalization=formKC` or `formKD` merges compatibility equivalents
  (fullwidth Ａ≡A, 𝐚≡a, no-break space ≡ space). Hence `collision_key` is
  compatibility caseless matching (D147) of the uppercase image of the
  compatibility decomposition,
  `NFKD(casefold(NFKD(casefold(NFD(upper(NFKD(name)))))))` through ICU4X
  (`icu_normalizer`, `icu_casemap`, one Unicode version for every step).
  Measured over every code point, it splits no class APFS, Seatbelt or ZFS
  under any setting merges. NFKD comes first: `𝚤` decomposes to ı, which ZFS
  upcases, yet has no uppercase of its own. `lower(upper(·))` without
  casefold misses ß≡ẞ, which APFS merges; NFKC_Casefold misses ı≡I. ASCII
  keys are their lowercase, and a non-ASCII character whose key is ASCII (K,
  ſ, ﬁ, Ａ) lands on that same lowercase, so the ASCII fast path is exact.
  Unicode's stability policies mean a newer table never splits a class an
  older one merged.
- **Linux ext4 and f2fs casefold is NFD plus casefold, per directory.** The
  volume, not the OS, owns case-insensitivity: synod's Linux guest reads APFS
  and NTFS shares, and APFS has a case-sensitive variant. So the fold is
  neither a `cfg` (one grant would mean three things) nor a question put to
  the filesystem: `pathconf(_PC_CASE_SENSITIVE)` and
  `FileCaseSensitiveInformation` answer per directory and only for what
  exists (`ENOENT` for an absent path), a same-uid writer can create the tail
  with the other flag after the query, Linux has no query for virtiofs or 9p,
  and it would make the algebra depend on the disk at check time. Storing the
  key in `real` instead would rename a distinct object on a case-sensitive
  volume, `real` being rendered into bwrap destinations, Seatbelt rules and
  the Windows SID hash. Refusing a deny on an absent path breaks the documented
  `` `cwd .env ``. Masking every colliding sibling on Linux and Windows enumerates
  directories, races, and builds machinery to enforce an over-approximation.
- **Each polarity takes the identity whose error is a refusal.** A deny that
  under-merges is the hole; one that over-merges refuses a sibling nobody
  meant. An allow that folded would over-grant on every case-sensitive volume,
  a grant on `Work` admitting `work`; held as stored it errs only towards
  refusing. Across layers the split carries on: denies join and allows meet
  by stored name, so two layers that spell one directory differently grant it
  to neither.
- **A folded match is as good as a stored one.** `holds` decides whether a rule
  speaks; it decides nothing else. An exclusion met by fold is negative evidence
  like any other. The
  witness a refusal cites is any exclusion that applies, so a folded one is
  named only when none holds the subject by name.
- **The over-deny is accepted and said.** On a case-sensitive volume — Linux,
  case-sensitive APFS, a case-sensitive NTFS folder — a distinct `secrets`
  beside a denied `Secrets` is refused in process and not by the Linux or
  Windows kernel, whose mask or ACE hangs on the object the volume's own lookup
  finds — which is also why those kernels hold an *existing* deny under every
  spelling already, Landlock's rules being by descriptor too. The refusal says
  why:
  `… is the denied … under another spelling (case or Unicode form); a deny holds under every spelling`.
- **A name that is not UTF-8 is its own key**; no filesystem folds one. Both
  identities, and `RealPath`'s order, compare names as the host encodes them
  (`OsStr::as_encoded_bytes`), never their `to_string_lossy` forms, because
  two distinct names that are not Unicode (bytes off Windows, an unpaired
  surrogate on it) decode to one replacement-character string, as does a name
  holding a literal U+FFFD, and would compare equal
  (`path_within_does_not_collide_distinct_non_utf8_paths`,
  `a_non_utf8_byte_is_not_its_replacement_character`,
  `an_unpaired_surrogate_is_not_its_replacement_character`). The Windows split
  works on those encoded units too: separators and heads are ASCII, so they
  are found losslessly, and a component that is not UTF-8 keys as itself. A
  file name that is not Unicode is no command name: `RealPath::name` gives
  none, so a `name` scope never matches it. A grant spelling whose
  real path is not Unicode is refused when it is frozen, lest a sibling whose
  name holds U+FFFD be granted in its place.

### Windows path identity

- **`/` and `\` alike, the `\\?\` verbatim prefix stripped
  in either slash spelling, a verbatim UNC head folded to `\server\share`,
  drive letters folded, directory entries compared as stored**
  (`windows_stored_components`). A
  `//?/`-spelled access must meet a `\\?\`- or plain-spelled deny: real Windows
  honours only the backslash form, but this matcher holds the separators
  interchangeable, and folding the two differently would leave a deny a
  differently spelled access slips past.
- **The platform rules are a `PathRules` parameter, never a `cfg!` read**, in
  `starts_with_identity`, `starts_with_collision`, `is_foreign_rooted`,
  `is_discard_device`, `reserved_device_refusal` and
  the name key, with `PathRules::HOST` read once at each sole call site, so both tables are pinned by tests on every host and
  neither is dark on the other.
- **8.3 short names are an alias, not a fold**, so no key closes them. With a
  deny on `C:\w\secretfile.txt`, a leaf left spelled `SECRET~1.TXT` misses the
  long name and admits the read. The walk instead reads each step's stored name from the handle it just opened,
  the normalized long name, and checks it against the parent's own path; a name
  it cannot check refuses the access. A short
  spelling makes an allow miss, a refusal, but a deny miss, an admission; the
  kernel's ACE, being on the object, holds for a child regardless.
- **The Windows allow side keeps stored case.** The step's stored name
  (`FileNormalizedNameInfo`) supplies the existing object's name, and the
  parent's for an absent leaf. Both allow containment and `RealPath` ordering
  preserve that case,
  so an allow on `Work` cannot reach a distinct `work`. Denies retain the
  collision fold. Display abbreviation still folds ASCII case.

### Firmlinks and the root

- **macOS firmlinks make `/tmp` and `/private/tmp` one directory.**
  Containment folds the alias table it shares with `canon`
  (`canon::firmlink_toggle`), because under Seatbelt `realpath(3)` can fail on
  `/tmp` itself and a grant authored in one spelling must cover an access in
  the other.
- **Two spellings of one directory are one object**, an allowance and an
  exclusion over them meet as Both, and the exclusion wins.
- **The root `/` is minted, never walked.** `realpath("/")` answers the
  *current drive* on Windows, so a frozen ceiling covered one volume, and a
  session launched from `D:` lost a `%TEMP%` on `C:` — the shape GitHub's
  Windows runners have. Folded to zero components it is the universal prefix, `Extent::Everywhere`;
  a spelling that walks to the root is `Everywhere` too. The bare root is
  one separator alone under the host's rules (`lex::is_bare_root`): `/`, and
  under Windows rules `\` too, the same driveless root. Under POSIX rules `\`
  is a relative file name, and minting it as the root would widen a relative
  spelling to every path
  (`a_backslash_mints_no_universal_authority_under_posix_rules`).

### Symlinks

- **A grant path carries `written` and `real`, and containment is on
  `real` on both sides.** A grant that lexically nests under a shallower
  ceiling but resolves outside it collapses to the empty meet: `{allow /a}`
  met with `{allow /a/link}`, `link → /x`, keeps nothing. A survivor would
  reach the OS sandbox, where `bwrap --bind` follows the source symlink and
  Seatbelt matches lexically, so a child could read the link's target
  (`a_symlinked_grant_cannot_escape_a_shallower_ceiling`, with its positive
  control).
- **A symlinked exec dir covers where it points**, allowance or exclusion
  alike. An allowance keeps the target it was resolved to: a symlink put in its
  place later redirects nothing and the allowance grants nothing there. An
  exclusion holds the objects its walk held, the link and its target, and the
  position its spelling named; a symlink put at an excluded directory's place
  later is not the denied object, and what it reaches, such as an admitted
  `/tools`, is judged on its own.
- **A symlink to an admitted file is admitted**, since it runs that file; a
  symlink under an admitted directory to a file outside it is not. The
  launcher runs the real path it judged, the user's spelling as `argv[0]`.

### Namespaces

- **synod mints guest prefixes with the POSIX fold**
  (`FrozenPath::from_guest`, `lex::fold_dots_under` under Posix rules),
  because the guard that matches them runs inside the machine, and on a
  Windows host the host fold once rebuilt `/work` as `\work`, a relative path
  in the namespace it claimed to name ([[decisions/260726_guest-namespace-prefixes|guest-namespace-prefixes]]).
- **A spelling carries no namespace.** Whose machine a path names is settled by
  the door that mints it; the meet compares resolved positions and nothing else,
  and a guest spelling is resolved only by the guard that matches it, inside the
  machine ([[decisions/261006_one-table-two-instances|one-table-two-instances]]).

### Exec identities

- **A bare key is the file the `PATH` of the decoding environment finds plus the
  bundled tool of that name**, walked with no cwd (`None`), never a scoped
  override: a planted `/tmp/evil/rg` inherits nothing from
  `rg: allow`, and `git: allow` admits `/usr/bin/git` typed as a path.
- **A bare exclusion is also a veto on its `command_name_key`**, so it stops an absolute `/bin/bash` and a link to it alike.
- **A path or dir key splits its polarities.** The allowance is the position its
  walk reached, never re-read; the exclusion is the objects it held. A bare key
  is resolved where the grant is decoded; a *head* is resolved afresh at each run
  (`which::locate` keeps an answer for the rest of one top-level run, shared by
  command resolution, so the program launched is the program judged). Two keys
  naming one file join to Both: `RealPath`'s `Ord` is the host's file identity, by
  components off Windows and by Windows identity components on it.
- **A veto's key folds.** `` deny: [`name bash] `` compiles to a folding exact deny and a
  veto, and Seatbelt's veto regex folds, so a stored key would admit in the
  guard a macOS child's `/x/BASH` that the kernel refuses
  (`a_veto_holds_every_spelling_of_the_name`).
- **On Windows the key strips the executable extension** and lower-cases ASCII
  (`name_key_on`, over `WINDOWS_EXEC_EXTENSIONS`), so `bash.exe` meets
  `` deny: [`name bash] ``.
- **A copy under a new name escapes a veto**, a declared limit: it is a
  different file with no trace of the denied name. What holds is the
  projection the copy inherits ([[design/grant|grant]] §Concessions), and
  `deputy_prefixes` reports where a write becomes runnable.
- **A multi-call binary is one file**, so a path key cannot tell its applets
  apart; a bare deny still vetoes by name.
- **A head that names no file has no program**, so no table judges it: 127, or
  126 when the file is not executable, never a denial
  ([[decisions/261004_exec-rules|exec-rules]]).

### Carriers and the linker

- **A carrier is what `execve` forwards to before the program's own code
  runs**: a `#!` interpreter, up to four hops, and on macOS the root-owned
  shims. Both kernels check a script's interpreter (measured), so without
  carriers an admitted script would die in the kernel.
- **Admitted only on bytes the uid cannot author** (`trusted_real`, a
  link-following walk from `/` whose every object is vetted); `env` is not followed, since following it
  reads a `PATH` the confined code controls. System-only is not the floor: a
  root-owned interpreter under `/opt` is a carrier.
- **The guard never consults them; the kernel needs them.** Carriers and the
  Linux linker candidates are *support* (above), joined as allowances only where
  the author wrote none and dropped when an exec or an fs exclusion covers them.
  An excluded carrier therefore refuses the script that starts through it, and
  the confined child's hint says which exclusion, since an allowance cannot
  override it ([[decisions/261004_exec-carriers|exec-carriers]]).

### What the kernel cannot see

The complete contract, with every declared support and limit, is in
[[design/two-enforcers|two-enforcers]]; the cases below are the ones this page
argues from.


- **argv**: the kernel sees no arguments, and no verdict depends on them.
- **Which bundled tool a re-exec of ral runs**: a tool is ral's own binary to
  the kernel. A *nested* ral is bound by the kernel alone: it boots with a fresh
  stack, so whatever only the guard enforces, bundled-tool rules and verbs among
  it, does not bind it, and under an exec allow-list that admits ral it runs every
  bundled tool.
- **Exclusions are guardrails where writing meets running.** A copy, a program
  written into a folder that is both writable and runnable, or a script fed to an
  admitted interpreter is a different request. The write freeze that once covered
  such folders is gone
  ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]);
  the deputy findings name them.
- **On Linux an admission is of an inode**: an excluded path that is a hard link
  to an admitted file runs. A path grant does not own the bytes reachable through
  it either, so a hard link inside a granted region to a file outside reads or
  writes that file.
- **Landlock is allow-list only**: a block inside an allowed directory is
  rendered by subtraction over the tree as it stands at launch, so a program
  added afterwards to an expanded directory is denied until the next launch,
  and a veto under a trusted write is the guard's alone on Linux
  ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]).
- **Windows has no kernel exec layer**, so the window between check and exec
  stays open there.
- **macOS has no profile stacking**: a process inside one Seatbelt profile
  gets `EPERM` entering a second, so a nested restricting launch is refused
  with attribution, never run under the wider profile.
- **macOS's process row is a budget**: `RLIMIT_NPROC` at the user's count at
  launch plus 512 ([[design/two-enforcers|two-enforcers]]).

### The projection

- **An allowance renders as its resolved position, and as nothing else.** The
  projection (`SandboxProjection::of`) carries the read region as one list of
  live positives, run places and live support among them, beside the write
  prefixes, the floor and the exclusions; a surface spelling is never a
  predicate of its own (macOS lets a lookup stat it, no more), since a name that was a symlink at decode can later
  hold a real directory the grant never named. Exclusions render from the
  resolved real paths their walks held, by position (the identity reading is
  ral's own readers'). Each backend refuses a redirected allowance rather than
  following it: Linux's `open_nofollow` refuses the launch, Seatbelt matches the
  resolved path against the recorded one, and Windows stamps the checked object
  through a held handle, with no redirecting reparse point on its path (a symlink,
  junction or other name surrogate is refused; a cloud-files placeholder is not).
- **The floor is an allowance like any other.** It is resolved once per process, on
  the premise that its roots are the system's: its package-prefix entries are the
  rows `homebrew`, `macports`, `nix` and `pkgsrc` outside the process's home
  ([[design/grant|grant]] §Selectors). An override such as `HOMEBREW_PREFIX` can
  still be the user's, and a child that writes the parent of such a root could
  replace it with a symlink, so the floor renders frozen too and a redirected
  root shows nothing. What loading needs is not authored, so a floor
  entry that resolves through a non-Unicode name is dropped, not refused
  (`freeze_floor`); a place that `PATH` resolved to and that is not Unicode
  cannot be frozen as an allowance in the first place.
- **Windows refuses the universal prefix.** A launch whose fs projection holds
  `Everywhere` in either region is refused before anything is stamped: an
  AppContainer reaches only what is stamped for it, and the root would narrow to
  one drive. The guard still judges.
- **`live()` is why no allow beneath a deny ever reaches a backend.** Such an
  allowance is Both, dead, and a Windows ACL orders explicit allows
  before inherited denies, so handing one over would invert the rule.
- **Existing Windows ACLs are another source of explicit allows.** A directory
  deny walks its descendants, reasserting an explicit deny at protected DACLs
  and explicit ACL boundaries. The walk skips reparse points and refuses a
  descendant it cannot inspect or stamp. Its persistent completion witness
  describes the tree at that walk, not later ACL edits or replacements
  ([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).
- **`pinned_dirs` derive from *rendered* positions**: `RealPath::ancestors_within`
  of each exclusion's positions within each write position, the name's and the
  link's target's, so an alias's chain is pinned beside its target's ([[internals/seatbelt-profile|seatbelt-profile]]).
- **The discard device is excused before either region** — `/dev/null`, or on
  Windows exactly `\\.\NUL` in either slash spelling and any case — since
  nothing reaches the disk through it. **DOS reserved device names are refused
  before either** — `NUL`, `CON`, `PRN`, `AUX`, `COM1`–`COM9`, `LPT1`–`LPT9`,
  and `COM`/`LPT` with a superscript `¹`, `²` or `³`, which Windows reads as a
  digit, any case, any extension, trailing dots and blanks trimmed, so `COM1.log` and
  `nul .txt` too — since they name no object to judge. For `NUL` the refusal
  asks `Did you mean \\.\NUL?`.
- **`Refused::Guarded`** answers a write onto the boot-pinned enforcer binary, judged by
  inode so a hard link names it too, *before* the stack is folded: a stack with
  no fs opinion is `Unrestricted` and exactly the case to catch.

### Freshness

- **Allowances are positions; exclusions are objects.** An allowance keeps the
  position its walk reached at decode and is never walked again, in the guard or
  any backend. Walking it again would let a child that can write the folder
  holding an allowed folder replace it with a symlink and point it anywhere the
  user can reach, and both enforcers would follow. An exclusion keeps the
  objects its walk held and matches by position or by object, so it holds its
  object the moment it has another name. Creating a granted directory after the
  grant, or `rm -rf dir && mkdir dir`, keeps working: these are path grants, not
  inode pins.
- **A guest's grant is resolved inside the machine.** synod spells it with
  `FrozenPath::from_guest` and sends the authored `Capabilities`; the guest engine
  resolves it on its own filesystem, so a redirect made in one guest run cannot
  take effect in the next.
- **The projection renders what the guard judged**, once at spawn, because that
  is when the profile is written.
- **The exec table is a function of the layers**, resolved where each was
  decoded; the projection carries the very table in the `Admitted` that judged
  the launch (`ExecProjection::Restricted(ExecRules)`, by `ExecRules::for_kernel`),
  so the kernel list is no third language.

### Absent denies

- **An absent deny under a writable prefix reaches no Linux kernel**: a mount
  needs a mountpoint, and making one writes the host. ral's own creation is
  refused in process; a child may make the name in that launch, and the next
  launch finds an object to mask. macOS holds it from the start, Seatbelt's
  rules being negative over names. Windows cannot take access away beneath a
  grant at all, so it carves the allowance around the exclusion, and a name
  made in a carved directory inherits nothing granted (`docs/SPEC.md` §12.3;
  [[internals/holding-a-deny|holding-a-deny]]).

See also [[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]],
[[design/grant|grant]], [[design/two-enforcers|two-enforcers]],
[[related/access-control-algebra|access-control-algebra]] (PBel, and the
extension against the literature), [[map/core/capabilities|map: capabilities]].
