---
verified_at_commit: 002c654a
verified_at_date: 2026-10-13
anchors: [GrantStack, Capabilities::extend, Fragment, Axis, join_top, Evidence, Table::meet, Table::withheld, region, rules, for_invocation]
against: [design/grant, design/authority-tables, internals/capability-enforcement, design/two-enforcers, decisions/260906_object-not-name]
---

# Composing capability policies

**Authority is a policy in the sense of Bruns and Huth's PBel: a function from
requests to Belnap's four values, composed pointwise and closed to a decision
only where it is enforced.** ral uses a small fragment of PBel and nothing
else. [[design/grant|grant]] builds the policy a session runs under by composing
pieces: a base profile, an optional `--extend` fragment, and any `--restrict`
files. This page explains the composition rules, shows what they do, and reads
them against the literature. It is the formal-foundations companion to
[[design/grant|grant]] and
[[internals/capability-enforcement|capability-enforcement]]; the table
representation and the proofs are
[[design/authority-tables|authority-tables]], the decision and its rationale
[[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]].

## Two operations, two jobs

Every rule does one of three things to a given command or path: **allow** it,
**exclude** it, or **say nothing**. Pieces combine two ways:

- **restrict** (`--restrict`, nested `grant`): *narrow*. The result permits
  something only if every layer permits it. This is *stacking*: each piece is a
  layer of the `GrantStack`, and every question folds the layers' answers to the
  access at hand. Nothing flattens two layers into one (see below).
- **extend** (`--extend`): *join*. The ceiling is the base extended once by the
  file's fragment (`Capabilities::extend`), producing one layer. Every later
  layer restricts, so **authority is a ceiling, extended once, then only
  restricted**; within ral there is no extension at all.

## The one rule: an exclusion beats every allowance

An exclusion wins over every allowance, on both axes, in the guard and the
kernel, whatever their specificity. Neither a narrower directory nor an exact
file reopens an excluded region, and nor do a carrier, the dynamic linker or
ral's own binary; no backend makes an exception. In the extension, **an
allowance never removes authority; only an exclusion does.** So:

- a base exclusion survives a fragment that does not mention it;
- a base exclusion survives a fragment that *re-allows* it: `--extend` cannot
  reopen a veto;
- a fragment may *add* an exclusion, and it bites;
- a fragment that supplies an axis the base leaves unrestricted leaves it
  unrestricted, less the fragment's exclusions.

To allow something a base forbids, choose or write a base that allows it.

### Guardrails, not walls

An exclusion names programs by path and name. Where writing meets running, a
copy, a newly written program or a script fed to an interpreter is a different
request, and nothing here pretends otherwise. The exclusions that guard
binaries are guardrails; what they stop is the honest request.

### Examples

Base `reasonable` allows `git` and `curl`, reads `xdg-config`, and **excludes
`` `xdg-config gh ``** (where the GitHub OAuth token lives).

- **Add a folder.** Fragment: `` read [`home notes] ``. `~/notes` becomes readable; `gh`
  stays excluded because the fragment never mentions it.
- **Reopen a blocked command.** A base that excludes `bash`. Fragment:
  `` exec [allow: [`name bash]] ``. bash stays excluded. To get bash, use a base
  that allows it.
- **Add an exclusion.** Fragment: `` fs [deny: [`home secrets]] ``. `~/secrets` is
  blocked on top of whatever the base allowed.
- **"minimal plus git".** `minimal` does not *exclude* git, nor Homebrew; it
  just does not list them, so `` exec [allow: [`name git]] `` adds `git` cleanly,
  and a Homebrew `git` is admitted by name with nothing in the way. A base
  should exclude only what is truly off-limits and leave "not included, but
  fine to add" unlisted.
- **`dangerous` plus an exclusion.** `dangerous` omits every axis. Extended
  positively it stays unrestricted; extended with an exclusion it becomes
  "every request, less that exclusion". An omitted *base* axis is `t`, not an
  empty allowance.

## Roles are types

A PBel policy is total, so an omitted axis must denote something: the identity
of the role it plays. In a **policy** (a base, a restriction layer, the ceiling,
the stack) an omitted axis is `t`: it restricts nothing. In an **extension
fragment** an omitted axis is `⊥`: it states nothing. The two roles are two
types: `Capabilities` is a policy and the loader returns a `Fragment`, which only
`Capabilities::extend` consumes and which can never be pushed onto a stack.
Reading an omission in the wrong role is a type error, not a convention.

Extension of base `B` by fragment `F` is `B ◁ F`, per axis:

| `B` | `F` | `B ◁ F` |
|---|---|---|
| any | omitted | `B` |
| omitted (`t`) | supplied `Q` | `t ⊕ Q`: every request, less `Q`'s exclusions |
| supplied `P` | supplied `Q` | `P ⊕ Q` |

`◁` is not commutative, by design: its operands play different roles, whereas `⊕`
between supplied policies is commutative. **The extension invariant** is stated
of policy denotations: if `F` has no negative evidence at a request, whatever
`B` permits `B ◁ F` permits; if `F` has negative evidence there, `B ◁ F`
refuses; and `B ◁ F` permits only what `B` permits or `F` supports. It holds of
the guard and of the policy part of every kernel table, not of a kernel table
as a whole, since support (carriers, the linker) depends on the launch.

## Running and writing read

An allowance to write something allows reading it, in the layer that states it.
A grant reads what it, and the grants enclosing it, let run by path, directory
or name; a nested grant never makes something readable by naming it. So a
restrict file that reads only the working directory does not hide the `cargo`
that the ceiling runs from `~/.cargo/bin`: to hide it, exclude it. A bundled
tool runs ral's own image, so it reads ral's binary, and an fs exclusion of that
binary refuses every bundled tool; ral's binary is otherwise an ordinary host
file, admitted exactly where a grant admits it. A nested ral is bound by the
kernel alone, a guardrail like any other.

## Why this is the right default: the sources

- **Both choices are standard.** "Exclusion wins" is **deny-overrides**; "allow
  wins" is **permit-overrides**: two of the rule-combining algorithms in the
  OASIS **XACML** standard. The literature offers the menu; you choose by threat
  model.
- **For a sandbox, deny is the right default.** Saltzer & Schroeder's *fail-safe
  defaults* (1975): base access on what is explicitly permitted, and deny when
  in doubt. Deny-overrides is that principle applied to composition.
- **One rule everywhere keeps policies predictable.** Bruns & Huth's stated goal
  is "analyzable composition", and Tschantz & Krishnamurthi formalize *safety*,
  *monotonicity* and *independent composition*. Different rules for commands vs
  paths, or a specificity that lets an exact file reopen a directory, would let
  an unrelated line change a verdict elsewhere; one uniform rule keeps it.
- **The path/prefix world has a known footgun taxonomy.** Al-Shaer & Hamed
  catalog firewall-rule anomalies over prefix containment: *shadowing*,
  *generalization*, *correlation*, *redundancy*. ral takes the conservative
  discipline: no specificity at all, so a broad allowance over a narrow
  exclusion is always *Both*, and refuses.
- **The operators form an algebra.** Bonatti, di Vimercati & Samarati give policy
  *union*, *intersection* and *difference*; restrict is the intersection and
  extend a union that an exclusion survives.

## PBel, and the fragment ral uses

The value space is **Belnap's FOUR**, presented as the product bilattice over
the Booleans: evidence is a pair `(p, n)` of positive and negative support
(`Evidence`), with `(0,0)` Neither, `(1,0)` Allow, `(0,1)` Deny, `(1,1)` Both.
The orders are `x ≤k y` iff `p ≤ q ∧ n ≤ m`, and `x ≤t y` iff `p ≤ q ∧ m ≤ n`.
After Bruns and Huth (§3, §4.1) ral's policies use these terms and no others:

- basic policies `(t if X)` and `(f if X)`: an authored allowance or exclusion of
  target `X`, and the constant `t`;
- information join `⊕`: `(p, n) ⊕ (q, m) = (p ∨ q, n ∨ m)`, identity ⊥, which
  collects the evidence of two supplied fragments;
- truth meet `∧`: `(p, n) ∧ (q, m) = (p ∧ q, n ∨ m)`, identity `t`, which
  composes restricting layers;
- the wrapper `↓`, `p↓ = p[⊤ ↦ f][⊥ ↦ f]`: permit exactly when `p ∧ ¬n`, applied
  once, at the boundary that enforces (Bruns and Huth §6, "top-level wrappers").

Both `⊕` and `∧` are associative, commutative and idempotent, and a negative
bit, once present, survives both. Within this fragment no composition followed
by `↓` tells Both from Deny; FOUR is kept regardless, as PBel's carrier, so that
adding negation, knowledge meet or priority later does not change the
representation. Neither is not an authored deny: **a missing allowance is never
stored as an exclusion.** Net is the one axis kept as a decision rather than as
evidence. Every enforced policy has the shape `↓(E₀ ∧ E₁ ∧ … ∧ Eₙ)`, with every
`⊕` before every `∧`, which is why an extension cannot follow a restriction and
undo it: `(B ∧ R) ⊕ E` would permit what `R` withholds, but a pushed layer is
immutable and `◁` takes a `Fragment`, which only the loader constructs, once,
before the first push. One extension file is an interface choice; several would
fold by `⊕` before extending the base. The order of the stack still matters
within `∧`, because a layer reads the run places of the layers beneath it, so the
same layer elaborates differently above and beneath another
([[design/authority-tables|authority-tables]]).

Not in the fragment: priority, negation, provenance of evidence, argument
policies, any public policy-expression language. Ours, not the reference's: the
request domains (running as reading, looking down the stack), polarity-dependent
path identity, omission indexed by role, allowances by position with exclusions that
hold their objects, support, denotational activation, and the absence of specificity.

## What the reference gives, and what is ours

From Bruns and Huth (Imperial College DTR11-6, 2011; ACM TISSEC 14(1)):

- §2: FOUR as the simplest non-trivial bilattice, after Ginsberg (1988); the
  pair presentation is the product construction of bilattices (Ginsberg; Fitting,
  *Bilattices and the semantics of logic programming*, 1991).
- §3: core PBel, with basic policies `(b if rp)`.
- §4.1: the derived operators (`⊕`, `⊗`, `p > q`) and the wrappers `↓`, `↑`.
- §6: the idioms. Resolve conflicts at the top level; absolute prohibitions;
  SPL's AND as `(p ⊕ q)[⊤ ↦ f]`, which is deny-overrides.
- §11: related work. Three-valued composition is subsumed by PBel; D-algebras are
  compared.

Ours, and our obligations rather than the reference's:

- the request domains: running as reading at the stack, ral's binary as an
  ordinary file, a bundled tool's read of it, writing as reading;
- polarity-dependent identity (allowances as stored, exclusions under every
  spelling) and, on top of it, allowances by position and exclusions that hold the object their walk reached;
- omission indexed by role, the two role types and `◁`;
- verbs as sets of positive evidence, and net as a decision;
- the floor, carriers and linker as `⊕` support, and the guard/kernel contract
  that support does not obey ([[design/two-enforcers|two-enforcers]]);
- the universal positive `every`, compiled to `Any`, and its lowering against the
  Linux envelope;
- denotational activation;
- the `homebrew` selector;
- the absence of specificity, and the backend support and limits.

Not borrowed: OpenAI's Codex CLI restricts no program at the OS level (its
command rules are lexical checks on argv before spawn, and it does nothing about
copies), which agrees with treating exclusions as guardrails.

## Why restrict is stacking and not a flattened meet

An authored exec layer maps keys (bare names, paths, directories) to evidence,
and a key that is a bare name, `git: [status]`, is admission-or-restriction for
*whatever `git` resolves to at check time*. Take A = `/usr/bin/` allowed and
`git: [status]`, B = `/usr/bin/` allowed and silent on `git`. `A ∧ B` should
refuse `git push` (A does). No authored map says that: keep A's key and the
result also refuses `git` where B's directories never admitted it; drop it and
the result admits `git push`. The meaning of the one-sided key is "A's
restriction ∧ B's directory evidence on the resolved path", which exists only
once the name is a file. So the authored form is not closed under intersection,
and the meet is the stack: every question compiles each layer against the host
and intersects the answers, where the same question *is* closed
([[decisions/260906_object-not-name|object-not-name]]).

**Restrict is a meet of tables.** Compiled, a layer is a table of scoped rules
over objects, denoting a function from subjects to evidence, and `Table::meet`
is defined by `ev(a ∧ b, s) = ev(a, s) ∧ ev(b, s)`: exclusions join, and an
allowance survives only at its own subject where both sides support it. A
default written down as an exclusion would, under the spelling-folding relation
exclusions use, refuse names neither layer excluded. The proof sketch and the
property tests are [[design/authority-tables|authority-tables]].

## Cross-check: the code matches

The composition site is `exarch::policy::for_invocation`, which builds one
ceiling, `base.extend(fragment)`, and pushes it; then each `--restrict` file,
the deny layer for the restrict files and the credential files, and last the
self layer, an exec exclusion of exarch's own binary, as separate restriction
layers. Credential, profile-file and self exclusions never move into the
extension, where they could be forgotten. The `deny_layer` is `t ⊕ (f if X)`,
built by `join_top` over a fragment of exclusions.

- **restrict**: each file is a layer; `capability::fs::region` and
  `capability::exec::rules` meet the layers' tables with `Table::meet`, so a
  conjunct per layer and `↓(S ∧ R)` permits nothing `↓S` did not (restriction attenuates).
  Pairing of run places looks *down* the stack, so a pushed layer changes no
  layer beneath it.
- **extend**: `Capabilities::extend` applies `◁` per axis through
  `Axis::{join, join_top}`, over a `Fragment` the loader alone constructs. On
  exec, `join_top` sets `every` and keeps allowances beside it, so they name run
  places without restricting; verbs join by union, fs by union of lists, net by
  `Offline`.
- **at the point of use**: `GrantStack::admit` and the fs guard
  (`check_fs_op`, `Shell::locate`) fold every layer and decide once, with `↓`,
  by `Table::withheld`, which cites an exclusion that applies or names the missing
  permission. Any layer's exclusion excludes; any layer's missing allowance
  refuses.
- **activation**: `Capabilities::is_restrictive` asks each axis whether it
  denotes `t` (`Axis::is_top`), so a layer that restricts nothing activates
  nothing.

## See also

[[design/grant|grant]] (the lattice this instantiates),
[[internals/capability-enforcement|capability-enforcement]] (where these rules
run), [[design/two-enforcers|two-enforcers]], [[related/system-c|system-c]] (the
type-based pole of the same calculus).

Cite:
- Saltzer & Schroeder, *The protection of information in computer systems*,
  **Proceedings of the IEEE** 63(9), 1975 — the *fail-safe defaults* principle.
- OASIS, *eXtensible Access Control Markup Language (XACML)* — the standard
  rule-combining algorithms (deny-overrides, permit-overrides, …).
- Bruns & Huth, *Access control via Belnap logic: intuitive, expressive, and
  analyzable policy composition*, **ACM TISSEC** 14(1), 2011,
  [doi:10.1145/1952982.1952991](https://doi.org/10.1145/1952982.1952991).
- Bonatti, De Capitani di Vimercati & Samarati, *An algebra for composing access
  control policies*, **ACM TISSEC** 5(1), 2002,
  [doi:10.1145/504909.504910](https://doi.org/10.1145/504909.504910).
- Tschantz & Krishnamurthi, *Towards reasonability properties for access-control
  policy languages*, **SACMAT** 2006,
  <https://cs.brown.edu/~sk/Publications/Papers/Published/tk-reasonability-ac-pol/>.
- Al-Shaer & Hamed, *Firewall policy advisor for anomaly discovery and rule
  editing*, **IFIP/IEEE IM** 2003; *Discovery of policy anomalies in distributed
  firewalls*, **IEEE INFOCOM** 2004.
- Belnap, *A useful four-valued logic*, 1977; Ginsberg, *Multivalued logics*,
  **Computational Intelligence** 4(3), 1988; Fitting, *Bilattices are nice
  things*, 2002,
  <https://id144254.securedata.net/melvinfitting/bookspapers/pdf/papers/BilatticesPhiLog.pdf>.
