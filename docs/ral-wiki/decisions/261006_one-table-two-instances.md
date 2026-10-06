---
status: active
generated_at_commit: 90479dea
verified_at_commit: 90479dea
anchors: [Table, Scope, ExecScope, Subject, Region, ExecKey, ExecRules, live, respelled, kernel, region, rules, refreeze, contains, own, meet_insert]
---

# One table, two instances

**The fs and exec dimensions of a grant are one structure, `Table<K: Scope>`,
whose meet is defined by the pointwise law `⟦a ∧ b⟧ = ⟦a⟧ ∧ ⟦b⟧`; the fs
region and the exec rules are its two instances.** The mathematics, the
instances side by side and every platform corner case are
[[design/authority-tables|authority-tables]]. Supersedes the mechanics of
[[decisions/261004_exec-rules|exec-rules]] and of
[[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]],
whose laws stand; supersedes the `Namespace` tag that rode along with
[[decisions/260726_guest-namespace-prefixes|guest-namespace-prefixes]], whose
fold stands.

## Context

The two dimensions answered the same question — which rule speaks to this
subject, and which of the speakers decides — with two unrelated mechanisms.

- **fs** was a pair of prefix sets, `PrefixSet<Allow>` and `PrefixSet<Deny>`,
  with `meet` for allows, `union` for denies, `outside` dropping an allow a
  deny covered, `covering` finding a deny's match, and `meet_prefixes` the
  atom the kernel and the deputy fold shared. `allow_region` and
  `deny_region` folded each across a stack.
- **exec** was a compiled table of four maps (files, tools, dirs, vetoes)
  with a hand-written `met`, `dirs_over`, `dir_verdict` and `kernel_verdict`;
  the authored `ExecGrant` was itself three maps, `names`, `paths` and `dirs`,
  the last keyed to a `bool`.
- **A namespace tag.** Every `FrozenPath` carried a `Namespace`, `Host`
  or `Guest`, and fs containment keyed on `(namespace, resolved)`, so a guest
  prefix never overlapped a host one.

Once denies came to hold under every spelling, exec's meet needed a page-long
proof that writing no default as a deny preserved
`verdict(a ∧ b, p) = verdict(a, p) ∧ verdict(b, p)`, argued case by case over
dirs, files, tools and vetoes; fs held the same law by an argument nobody had
written down, through different code. Two proofs of one law over two
representations is one too many.

The tag was a bug. It named the machine relative to whoever minted the prefix
— `Guest` from the host's side — but was compared as though absolute. Inside
synod's guest, synod's own prefixes arrived tagged `Guest` and a nested
`grant` minted its prefixes there tagged `Host`, so every nested grant met
synod's base to nothing. The fix that had mattered was the fold:
`from_guest`'s POSIX `fold_dots_posix`.

## Decision

- **One table.** `Table<K: Scope>` is a `BTreeMap<K, Verdict>` entered through
  `meet_insert`. A `Scope` supplies a rank, `holds::<P: Polarity>`, and
  `own()`. `verdict`, `live`, `denies`, `respelled` and the meet are written
  once, generically; nothing in `table.rs` names an `Identity`.
- **The generic meet law.** Denies join; an allow scope survives at
  `⟦a⟧(k̂) ∧ ⟦b⟧(k̂)` where that is not Deny; a key met to Deny that no layer
  wrote is left absent. The pointwise law is the meet's definition, and the
  lattice laws are property-tested once, over a universe of case and NFC/NFD
  pairs.
- **fs is `Region = Table<FrozenPath>`**, subject a plain `&Path`, rank
  `(is a deny, depth)`, one region per `FsOp` carrying the layer's `deny_paths`,
  re-frozen against the caller's `Resolver` (`refreeze`), folded by `region`.
  The projection renders `live()` and `denies()` as surface strings; the
  deputy folds a `Region` of allows per dimension and reads `live()`.
- **exec is `ExecRules = Table<ExecScope>`**, `ExecScope { Dir, Carrier, File,
  Tool, Name }` over `Subject { Tool, File, Under }`, rank
  `Dir(depth) < Carrier < Exact < Name`, folded by `rules`.
- **`own()` replaces scope-against-scope containment.** The meet asks a rule's
  survival at the rule's own subject, so the only relation a scope must
  define is `holds`, between a scope and a subject.
- **`Carrier` is a scope.** `kernel()` adds the carriers to a copy of the table
  as `(Carrier(c), Allow)` and asks `verdict` for each file once, so an
  authored exact deny or a veto beats a carrier by rank alone, where a
  hand-written `kernel_verdict` decided it before. A carrier holds a file by
  mutual containment, as a `File` rule does, where it was set membership, so
  firmlink spellings agree.
- **The authored grant is one map too.** `ExecGrant(BTreeMap<ExecKey,
  Verdict>)`, `ExecKey { Name, Path, Dir }`, the written twin of `ExecScope`;
  `Display` spells a key as a grant does, a dir with its trailing `/`, which is
  what exarch's prompt prints. Exec now has three languages, one map each:
  `ExecKey` as written, `ExecScope` compiled against this host, `ExecRule` as
  the kernel reads it.
- **No namespace.** `Namespace` is deleted; a prefix is `surface` and
  `resolved`, and the guest's spelling is the door's business
  (`from_guest`).

### Rejected

- **A three-state dir key** (allowed, authored deny, unset). "Unset" is a key
  that must never speak and never render, which is what absence already is,
  and it needs a third arm in every reader. The meet manufactures the default,
  so the meet stops it.
- **A trait with scope-vs-scope containment** for the meet. It duplicates
  `holds` with a second relation that must agree with it under both
  polarities; `own()` reduces the one to the other.
- **One fs table with op-tagged scopes.** A deny binds both ops, so it would be
  written twice or need an op-polymorphic scope, and read and write are never
  asked together. Two regions, one per op, are each a plain table, zipped
  where the projection needs both.
- **A `Place { namespace: Option<Namespace>, path }` subject** that kept the
  tag, the guard asking with none and the meet with one. It keeps the bug's
  carrier alive and makes the subject richer than the question.

## Consequences

- One meet, one law, one set of property tests:
  `the_meet_of_two_tables_judges_as_the_meet_of_their_verdicts`,
  `the_meet_is_idempotent_commutative_and_associative`,
  `the_kernel_rules_judge_as_the_table_and_its_carriers`.
- `respelled` is one function for fs and exec, naming the most specific
  folded deny (the deepest for fs, a file deny over a dir deny for exec), so
  the two refusals say the
  same sentence by construction.
- A nested `grant` inside synod's guest narrows synod's base as it does
  anywhere else.
- Widening is untouched: `--extend-base` still unions authored layers
  (`ExecGrant::widen`, `FsPolicy::widen`), with `evicts` the sweep, since
  widening is a single-layer operation on what an author wrote, not a meet of
  tables.
- The fs projection no longer re-mints a frozen surface through the host's
  fold: a no-op on a host prefix, and on a Windows host the one step that
  would have turned a guest's `/work` back into `\work`. A guest prefix is now
  re-frozen only by `refreeze` in the guard that matches it, inside the
  machine.

## Where

`core/src/capability/table.rs` (`Table`, `Scope`, `verdict`, `live`,
`denies`, `respelled`, `Meet for Table`), `core/src/capability/fs.rs`
(`Region`, `region`, `impl Scope for FrozenPath`),
`core/src/capability/exec.rs` (`ExecScope`, `Subject`, `Rank`, `ExecRules`,
`compile`, `kernel`, `allowed_files`, `rules`),
`core/src/capability/sandbox.rs` (`surface`), `core/src/capability/deputy.rs`
(`deputy_prefixes`), `core/src/path/forms.rs` (`contains`, `refreeze`,
`from_guest`), `core/src/path/lex.rs` (`Polarity`, `Allow`, `Deny`),
`core/src/types/capability.rs` (`ExecKey`, `ExecGrant`, `meet_insert`).
