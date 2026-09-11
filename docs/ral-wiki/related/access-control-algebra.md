---
verified_at_commit: a2d120d2
verified_at_date: 2026-09-11
anchors: [GrantStack, Capabilities::join, FsPolicy::join, ExecPolicy::meet, ExecPolicy::join, join_literal_exec, evaluate_exec, allow_region, deny_region, check_fs_op, layer_exec_verdict, for_invocation]
against: [design/grant, internals/capability-enforcement, design/two-enforcers, decisions/260906_object-not-name]
---

# Composing capability policies

[[design/grant|grant]] builds the policy a session runs under by composing
pieces: a base profile, an optional `--extend-base` overlay, and any
`--restrict` files. This page explains the composition rules in plain terms,
shows what they do with examples, and points at the literature they come from.
It is the formal-foundations companion to [[design/grant|grant]] and
[[internals/capability-enforcement|capability-enforcement]].

## Two operations, two jobs

Every rule does one of three things to a given command or path: **allow** it,
**deny** it, or **say nothing**. Pieces combine two ways:

- **restrict** (`--restrict`, nested `grant`) — *narrow*. The result permits
  something only if every layer permits it. This is *stacking*: each piece is
  a layer of the `GrantStack`, and every verdict folds the layers' answers to
  the access at hand. Nothing flattens two layers into one — see below for
  why it cannot.
- **extend-base** (`--extend-base`) — *widen*. The result adds the overlay's
  grants on top of the base (`Capabilities::join`), producing one layer.

## The one rule: a deny is a floor

In both operations, **a deny wins.** Once any layer denies something, nothing
else can take that deny away:

- a base deny survives an overlay that doesn't mention it;
- a base deny survives an overlay that *re-grants* it — `--extend-base` cannot
  reopen a veto;
- an overlay may *add* a deny, and it bites.

To allow something a base forbids, you choose or write a base that allows it; you
never reopen the floor from an overlay. Commands and paths follow this same rule.

### Examples

Base `reasonable` allows `git` and `curl`, reads `xdg:config`, and **denies
`xdg:config/gh`** (where the GitHub OAuth token lives).

- **Add a folder.** Overlay: `read ~/notes`. → `~/notes` becomes readable; `gh`
  stays denied because the overlay never mentions it. The deny holding here is
  what stops an unrelated grant from re-exposing the credential the base carved
  out.
- **Reopen a blocked command.** Base `minimal` denies `bash`. Overlay:
  `exec [bash: allow]`. → bash stays denied. To get bash, use a base that allows
  it.
- **Add a block.** Overlay: `fs [deny: ~/secrets]`. → `~/secrets` is blocked on
  top of whatever the base allowed.
- **"minimal plus git".** This is *not* reopening a block: `minimal` does not
  *deny* git, it just doesn't list it, so `exec [git: allow]` adds it cleanly. The
  lesson — a base should `deny` only what is truly off-limits and leave "not
  included, but fine to add" simply unlisted.

## Why this is the right default — the sources

These composition rules have standard names; ral picks the conservative one.

- **Both choices are standard.** "Deny wins" is **deny-overrides**; "allow wins"
  is **permit-overrides** — two of the rule-combining algorithms in the OASIS
  **XACML** standard. The literature offers the menu; you choose by threat model.
- **For a sandbox, deny is the right default.** Saltzer & Schroeder's *fail-safe
  defaults* (1975): base access on what is explicitly permitted, and deny when in
  doubt. Deny-overrides is that principle applied to composition.
- **One rule everywhere keeps policies predictable.** This body of work exists to
  make policies you can *reason about*: Bruns & Huth's stated goal is "analyzable
  composition," and Tschantz & Krishnamurthi formalize *safety*, *monotonicity*,
  and *independent composition* — whether you can predict a policy from its
  parts. Different rules for commands vs paths would break independent
  composition, letting an unrelated overlay line change a verdict elsewhere; one
  uniform rule keeps it.
- **The path/prefix world has a known footgun taxonomy.** Al-Shaer & Hamed
  catalog firewall-rule anomalies over prefix containment — *shadowing*,
  *generalization*, *correlation*, *redundancy*. A broad allow over a narrow deny
  is *generalization*; "most-specific match wins" is the discipline that tames
  it, the same one grant's symlink-resolving `path_within` enforces
  ([[internals/capability-enforcement|capability-enforcement]]).
- **The operators form an algebra.** Bonatti, di Vimercati & Samarati give policy
  *union*, *intersection*, and *difference*; ral's restrict/extend-base are the
  intersection and a deny-respecting union.

## The deeper structure (optional)

For the formal shape: the value space is **Belnap's four-valued logic** — allow,
deny, neither, conflict — which carries *two* orderings at once, a **bilattice**
(Ginsberg; Fitting), applied to access control by Bruns & Huth's PBel language.
`restrict` is a meet on one ordering (permission); `extend-base` is a join on the
other (assertedness), with deny-overrides as the fixed rule for conflicts. The
practical upshot is the only thing needed day to day: **deny wins, both ways.**
That this is *not* a single lattice — it genuinely has two orderings — is why one
operation never "absorbs" the other; that is expected, not a defect.

## Why restrict is stacking and not a flattened meet

An exec layer is `(allow_dirs, deny_dirs, literals)`, and a literal keyed by a
bare name — `git: [status]` — is admission-or-restriction for *whatever `git`
resolves to at check time*. Take A = `/usr/bin/` allowed and `git: [status]`,
B = `/usr/bin/` allowed and silent on `git`. `A ∧ B` should refuse `git push`
(A does). No `(dirs, literals)` triple says that: keep A's literal and the
result also refuses `git` where B's directories never admitted it; drop it and
the result admits `git push`. The meaning of the one-sided key is "A's
restriction ∧ B's directory verdict on the resolved path", which exists only
at the access. So the representation is not closed under intersection, and
the meet is the stack: `evaluate_exec` asks each layer about the concrete
command and intersects the answers, where the same question *is* closed.
(This was a real hole: a flattened `meet_literal_exec` dropped the one-sided
restriction — [[decisions/260906_object-not-name|object-not-name]].)

## Cross-check: the code matches

The composition site is `exarch::policy::for_invocation`, which produces a
`GrantStack` whose layers are `base ∨ extend`, then each `restrict`, then the
deny layers for the restrict files and the credential files.

- **restrict** — each file is a layer; `allow_region` intersects fs regions
  across layers, `deny_region` unions them, `evaluate_exec` intersects the
  layers' exec verdicts. Deny-overrides. ✔
- **extend-base** — `FsPolicy::join` unions denies *and* prefixes;
  `ExecPolicy::join` and `join_literal_exec` make `Deny` win; `ExecMap::join`
  lets a deny-dir win the exact-key clash. Deny-overrides, uniform with fs. ✔
- **at the point of use** — `check_fs_op` and `Shell::locate` test denies
  before grants and fold every stack layer; `layer_exec_verdict` checks vetoes
  first. Any layer's deny denies. ✔

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
