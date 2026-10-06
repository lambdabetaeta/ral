# Authority is a table of scoped rules

**Each dimension of a grant that ranges over places, fs and exec, denotes a
function from subjects to verdicts, and is represented by one structure: a
finite table of scoped rules, read as "the most specific speaker decides,
ties meet, silence denies".** A layer is a table, a stack is the meet of its
layers' tables, and the meet is *defined* by the pointwise law
`⟦a ∧ b⟧ = ⟦a⟧ ∧ ⟦b⟧`. The fs region and the exec rules are two instances
of it (`core/src/capability/table.rs`); everything that differs between them
is a choice of scope, subject and rank. Why the meet is a stack fold and not a
flattened grant is [[decisions/260906_object-not-name|object-not-name]]; why
the two dimensions became one structure is
[[decisions/261006_one-table-two-instances|one-table-two-instances]].

## The structure

- **Verdicts.** `Deny < Only(s) < Allow`, `Only` meeting by intersection
  (`Verdict`, `core/src/types/capability.rs`). Deny is the bottom.
- **Subjects** `s ∈ S`: what a table judges. A path for fs; for exec a
  `Subject` — a file by its real path, a bundled tool by name, or `Under(d)`,
  the inside of a directory.
- **Scopes** `k ∈ K`: where a rule applies. A table `T` is a finite map
  `K → Verdict`; a scope written twice meets (`meet_insert`), so a table never
  holds two opinions at one key.
- **Polarity picks identity.** A rule *speaks* to `s` when its scope holds `s`
  as the rule's polarity reads names: an allow (or `Only`) under `⊑S`, the
  name as stored; a deny under `⊑C`, every spelling some filesystem takes for
  it (`collision_key` per component). `⊑S ⊆ ⊑C`, both transitive, both
  firmlink-aware. The table names `Allow` or `Deny`, never an `Identity`;
  `core/src/path/` alone maps one to the other
  ([[invariants/grants-judge-objects|grants-judge-objects]]).
- **Rank.** `rank(k, v)` says how specific a rule is; the greater decides.
- **Own subject.** `k̂ = k.own()` is the subject a rule at `k` is itself judged
  at: a prefix's resolved path, a directory's inside, a file itself, a tool's
  name.

**Denotation.**

```
⟦T⟧(s) = ⋀ { v : (k, v) ∈ T speaks to s, rank(k, v) maximal among speakers }
⟦T⟧(s) = Deny   when nothing speaks
```

One function computes it, `Table::verdict` (`most_specific` over `speaking`).
The guard asks it, the projection renders what it admits, and no other code
makes a verdict.

## The meet: denies join, allows meet

```
a ∧ b = { (k, Deny) : (k, Deny) ∈ a ∪ b }
      ∪ { (k, m)    : (k, v) ∈ a ∪ b, v ≠ Deny, m = ⟦a⟧(k̂) ∧ ⟦b⟧(k̂), m ≠ Deny }
```

Every deny either side wrote is kept; an allow scope survives at its own
subject's met verdict, where that is not Deny. **A key met to Deny that no
layer wrote as a deny is left absent.**

**Why a default may not be written down.** The obvious pointwise
representation stores, at every key of either side, both sides' verdicts met.
That writes Deny at a key one side never mentioned: a *default* recorded as an
*authored* deny. Under `⊑S` the two are indistinguishable. Under `⊑C` they are
not, because a written deny holds every spelling of its name. On a byte-exact
host take

```
a = { allow /a/B }      b = { allow /a/B, allow /a/b }
```

Both layers allow `/a/B/x`. The pointwise table holds `/a/B ↦ Allow` and
`/a/b ↦ Deny`; at `/a/B/x` the allow speaks by `⊑S`, the manufactured deny by
`⊑C`, at one rank, and the tie denies. The kernel would render the same key,
and Seatbelt, matching caselessly, would refuse a child what the guard
admits. The meet is where the default is manufactured, so the meet is where it
is stopped (`the_meet_writes_no_default_as_a_deny`;
`ral/tests/capabilities.rs::a_stacked_one_sided_allow_denies_no_other_spelling`).

**The law.** For every subject `s`, `⟦a ∧ b⟧(s) = ⟦a⟧(s) ∧ ⟦b⟧(s)`. It holds
of any scope satisfying five conditions, which both instances meet:

- **(P) polarity** — `holds⟨Allow⟩ ⊆ holds⟨Deny⟩`;
- **(S) self** — `k` holds `k̂` under Allow;
- **(T) transitivity** — if `k` holds `k̂′` and `k′` holds `s`, `k` holds `s`,
  under Allow when both did, else under Deny;
- **(R) locality** — if allow `k` and deny `d` both speak to `s` and
  `rank(k) > rank(d)`, then `d` holds `k̂`: a more specific scope at a subject
  lies within every less specific one there;
- **(C) chain** — the allow scopes speaking to one subject are nested, the
  inner one of rank at least the outer's.

*Proof sketch.* Fix `s`.

- *A side denies, say `a`.* Either (i) a deny `d` is among `a`'s top speakers
  at `s`, or (ii) nothing in `a` speaks (top allows alone cannot meet to
  Deny). (i) `d ∈ a ∧ b`, since denies join. An allow `k` of the meet that
  outranks `d` at `s` has `⟦a⟧(k̂) ≠ Deny`; by (R) `d` speaks at `k̂`, so some
  `a`-allow `k″` speaks at `k̂` above `d`, and by (T) `k″` speaks at `s` above
  `d`, contradicting `d`'s place at the top. Nothing outranks `d`; a tie
  meets to Deny. (ii) An allow `k` of the meet speaking at `s` needs some
  `a`-allow at `k̂`, which by (T) speaks at `s`. So the meet denies.
- *Both allow.* By (C) the top speakers of both sides form a chain; let `k`
  be the innermost. Everything that speaks at `k̂` speaks at `s` (T), and the
  top speakers at `s` of each side hold `k̂` (C), so `⟦a⟧(k̂) = ⟦a⟧(s)` and
  `⟦b⟧(k̂) = ⟦b⟧(s)`: `k` survives at `⟦a⟧(s) ∧ ⟦b⟧(s)`. Every deny of the meet
  speaking at `s` sat below its own side's top, hence below `k`; an allow of
  the meet outranking `k` would have outranked its own side's top. ∎

For fs, (R) is vacuous — no allow outranks a deny — and (C) is the nesting of
prefixes along one path. For exec, (R) says a deeper directory, a file, or a
carrier lies within the shallower directory that denies around it, and (C)
says a file rule sits inside the directories that contain it.

**The lattice laws** hold on the denotation outright and on the
representation up to normalisation: `a ∧ b = b ∧ a`, `(a ∧ b) ∧ c =
a ∧ (b ∧ c)`, and `(a ∧ a) ∧ (a ∧ a) = a ∧ a` — a table as written may hold an
allow its own rules outrank, which a meet leaves out, so `a ∧ a` is `a`
normalised. Pinned by property tests over a universe of directories, files,
tools and vetoes holding case pairs and an NFC/NFD pair, so a name one
filesystem keeps apart and another merges:
`the_meet_of_two_tables_judges_as_the_meet_of_their_verdicts` and
`the_meet_is_idempotent_commutative_and_associative` (`capability/table.rs`),
and for the kernel rendering
`the_kernel_rules_judge_as_the_table_and_its_carriers` (`capability/exec.rs`).
The pointwise representation, by contrast, broke the law on 3,869 of 20,000
random pairs of a model.

## What a table hands out

- **`live()`** — the allows still in force at their own subjects:
  `{ k : (k, v) ∈ T, v ≠ Deny, ⟦T⟧(k̂) ≠ Deny }`. For fs, the allow lists a
  backend renders; for exec, the files whose carriers the kernel needs
  (`allowed_files`).
- **`denies()`** — the scopes written as denies.
- **`respelled(s)`** — the most specific deny (greatest rank) that holds `s` only by a fold, provided *no* deny
  holds `s` as stored; else nothing: for fs the deepest such deny, for exec a
  file deny over a dir deny. A refusal names it, so the author learns
  the deny bit through case or Unicode form:
  `fs write denied by grant: /w/.ENV is the denied /w/.env under another spelling (case or Unicode form); a deny holds under every spelling`.
  A veto's key is folded already and holds identically under both polarities,
  so a vetoed name is refused plainly.

## Two instances

| | fs — `Region` | exec — `ExecRules` |
|---|---|---|
| scope `K` | `NormalizedPrefix` | `ExecScope { Dir, Carrier, File, Tool, Name }` |
| subject | a plain `&Path`, the access canonicalised or walked | `Subject { Tool, File, Under }`, from a `Program` |
| `k̂` | the prefix's `resolved` path | `Under(d)`, `File(f)`, `Tool(t)` |
| holds | `NormalizedPrefix::contains::<P>`, `path_within(path, resolved, P::IDENTITY)` | `RealPath::within::<P>`; a file or carrier by mutual containment; a tool by name; a name by `command_name_key` |
| rank | `(is a deny, depth)`: a deny outranks every allow at any depth, the deeper ranking above among each; verdicts read as deny-wins, since every fs allow is `Allow` | `Dir(depth) < Carrier < Exact < Name`, verdict-blind |
| where the denies live | each layer's `deny_paths`, in both the read and the write region | in the table, beside the allows |
| tables per stack | two, one per `FsOp`, `None` when no layer holds an fs opinion | one, `None` when no layer holds an exec opinion |
| when frozen | re-frozen against the live `Resolver` at every guard check, once at spawn for the projection | compiled afresh per question; path and dir keys are frozen forms, never re-read |
| extra scope | — | `Carrier`, added only by `kernel()` |

**Exact beats a covering deny dir in exec; a deny wins at any depth in fs.**
Each is its own dimension's law. An fs deny is a carve-out protecting
*content* — credentials under a readable `xdg:config`, the agent's profile
under a writable cwd — and every backend that holds it hangs it on a subtree
an allow cannot reopen: a bwrap mask hides the whole tree, an ACE inherits
down it, and a Windows ACL orders explicit allows before inherited denies, so
it must never be handed an allow beneath a deny at all. An exec rule names a
*program*, and the kernel's rule list is ordered, so last-match-wins can state
an exception: `deny dir /x/B` with `allow file /x/b/tool` admits `/x/b/tool`,
and the deepest directory decides between nested ones. Deny-wins in exec
would break both, for reasons unrelated to any spelling.

**Exec has three languages, one map each.**

- `ExecKey { Name, Path, Dir }` — the grant *as written*: `ExecGrant` is
  `BTreeMap<ExecKey, Verdict>`, entered through `meet_insert` (its
  `FromIterator` too). It rides the
  wire, `--extend-base` widens it (`ExecGrant::widen`, its `evicts` sweep), and
  exarch's prompt prints it through `Display`, a dir with its trailing `/`.
- `ExecScope` — the grant *compiled against this host* (`ExecRules::compile`):
  a bare name becomes the bundled tool of that name, the file the host `PATH`
  finds, and for a deny a `Name` veto; a path key a `File`, a dir key a `Dir`
  at `Verdict::from(!v.is_denied())`, so no dir carries `Only` into a table,
  each its frozen resolved form.
- `ExecRule { Dir, File, Veto }` — the table *rendered for the kernel*
  (`ExecRules::kernel`): a path and a bool, or a vetoed name. No `Only`, no
  tools, no carriers as such.

They cannot be one type. A bare key is no scope until the host `PATH` is
consulted, so an authored grant is not closed under meet and must be compiled
per question ([[decisions/260906_object-not-name|object-not-name]], fault B).
And the kernel's vocabulary is strictly poorer than the guard's: it sees no
argv and no bundled tool, and an allow-only renderer must be able to take the
allows as they stand.

**The kernel's list.** `kernel()` adds `(Carrier(c), Allow)` for each carrier,
then emits each `Dir` at its own verdict and rank, each distinct file among
`File` and `Carrier` scopes *once* at the table's verdict for it, and each
`Name` as a `Veto`, sorted by rank with denies after allows within a rank. The
kernel's last-match-wins is then the guard's highest-rank-wins by
construction, and Landlock, which renders allows only, may keep them as they
stand ([[decisions/261004_exec-carriers|exec-carriers]]).

### Worked cases

On a case-sensitive volume, where `Tools` and `tools` are distinct
directories. On a case-insensitive one each pair is one object with one
stored spelling, `realpath` returns it, and the answers are those of the
stored side; on Windows the ASCII pairs are one key already. *Respelled*
marks a refusal that names the denied spelling.

| Grant | Subject | Verdict | Why |
|---|---|---|---|
| allow dir `/x/Tools`, deny dir `/x/tools/evil` | `/x/Tools/evil/t` | Deny, respelled | the deny is deeper and holds `/x/Tools/evil` by `⊑C`; `/x/Tools/good/t` is Allow |
| allow dir `/x/b`, deny dir `/x/B` | `/x/b/t` | Deny, respelled | one rank, the tie meets; fs's `live()` drops the allow likewise |
| deny dir `/x/B`, allow file `/x/b/tool` | `/x/b/tool` | Allow | `Exact` over `Dir`; fs has no such rank |
| allow file `/x/tool`, deny file `/x/Tool` | `/x/tool` | Deny, respelled | one rank, the tie meets |
| stack `a = {allow /a/B}`, `b = {allow /a/B, allow /a/b}` | `/a/B/x` | Allow | the counterexample: `/a/b` is not written as a deny |
| stack `a = {allow /a, deny /a/B}`, `b = {allow /a/b, allow /a/c}` | `/a/b/x` | Deny, respelled | `/a/B` joins the met denies; `/a/b` and `/a` are dropped, each denied at its own subject by one side; `/a/c/x` is Allow |
| two layers, one allowing `/a/B`, the other `/a/b` | `/a/B/x` | Deny | each allows only its spelling and the meet keeps neither |
| stack `a = {allow /a, deny /a/b}`, `b = {allow /a/b/c}` | `/a/b/c/x` | Deny | a deeper allow in one layer cannot lift a deny another places above it |
| allow dir `/x`, veto `sh` | `/x/SH`, `/x/ſh` | Deny | a veto's key folds; `/x/shx` is Allow |

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
  exactly the write a deny exists to stop: under `deny: [cwd:/.env]` with
  `.env` absent, `echo key > .ENV` once left `key` in the object `.env`
  names. Every later access was refused; the hole was the access that creates
  the object ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]).
- **Seatbelt matches `literal`, `subpath` and `regex` under the same
  equivalence on every volume, absent and existing names alike, and folds
  allows too.** On a case-sensitive volume, where `Work` and `work` are
  distinct objects, `subpath Work`, `subpath WORK` and `literal …/Work/t`
  each admit reading `work/t`: a Seatbelt allow over-grants a child across
  case there. SBPL cannot say otherwise; it is a stated residual. Hence
  **macOS renders nothing differently**: a rendered deny and the guard already
  mean one thing.
- **NTFS upcases each UTF-16 unit through `$UpCase` and does not normalise**,
  and simple uppercase sends ı to I, so ı, i and I merge. Hence
  `collision_key` is D145 *of the uppercase image*,
  `NFD(casefold(NFD(upper(name))))` through ICU4X (`icu_normalizer`,
  `icu_casemap`, one Unicode version for every step). Measured over every code
  point of Unicode 16, the uppercase image differs from D145 in exactly the
  class {I, i, ı}. `lower(upper(·))` without casefold misses ß≡ẞ, which APFS
  merges. NFKC_Casefold is coarser than any filesystem: fullwidth Ａ≡a is
  distinct on APFS and under Seatbelt. ASCII keys are their lowercase, and a
  non-ASCII character whose key is ASCII (K, ſ, ﬁ) lands on that same
  lowercase, so the ASCII fast path is exact. Unicode's stability policies
  mean a newer table never splits a class an older one merged.
- **Linux ext4 and f2fs casefold is NFD plus casefold, per directory.** The
  volume, not the OS, owns case-insensitivity: synod's Linux guest reads APFS
  and NTFS shares, and APFS has a case-sensitive variant. So the fold is
  neither a `cfg` (one grant would mean three things) nor a question put to
  the filesystem: `pathconf(_PC_CASE_SENSITIVE)` and
  `FileCaseSensitiveInformation` answer per directory and only for what
  exists (`ENOENT` for an absent path), a same-uid writer can create the tail
  with the other flag after the query, Linux has no query for virtiofs or 9p,
  and it would make the algebra depend on the disk at check time. Storing the
  key in `resolved` instead would rename a distinct object on a case-sensitive
  volume, `resolved` being rendered into bwrap destinations, Seatbelt rules and
  the Windows SID hash. Refusing a deny on an absent path breaks the documented
  `cwd:/.env`. Masking every colliding sibling on Linux and Windows enumerates
  directories, races, and builds machinery to enforce an over-approximation.
- **Each polarity takes the identity whose error is a refusal.** A deny that
  under-merges is the hole; one that over-merges refuses a sibling nobody
  meant. An allow that folded would over-grant on every case-sensitive volume,
  a grant on `Work` admitting `work`; held as stored it errs only towards
  refusing. Across layers the split carries on: denies join and allows meet
  by stored name, so two layers that spell one directory differently grant it
  to neither.
- **A folded match ranks as a stored one.** `holds` decides whether a rule
  speaks, never at what rank: a deny met by fold sits at its own depth, ties
  still meet, and a deeper allow still beats a shallower deny
  (`a_folded_match_ranks_as_a_stored_one`). `evicts`, the `--extend-base`
  sweep that drops an allow dir a deny dir clashes with, asks containment
  under `Collision` both ways: mutual containment is one rank, where the
  guard's tie already denies, so the sweep agrees with the verdict.
- **The over-deny is accepted and said.** On a case-sensitive volume — Linux,
  case-sensitive APFS, a case-sensitive NTFS folder — a distinct `secrets`
  beside a denied `Secrets` is refused in process and not by the Linux or
  Windows kernel, whose mask or ACE hangs on the object the volume's own lookup
  finds — which is also why those kernels hold an *existing* deny under every
  spelling already, Landlock's rules being by descriptor too. The refusal says
  why:
  `… is the denied … under another spelling (case or Unicode form); a deny holds under every spelling`.
- **A name that is not UTF-8 is its own key**; no filesystem folds one. The
  `Stored` identity compares `&Path`s, never `to_string_lossy` forms, because
  two distinct non-UTF-8 paths decode to one replacement-character string and
  would compare equal
  (`path_within_does_not_collide_distinct_non_utf8_paths`). The Windows branch
  necessarily accepts the lossy form: its identity fold is defined on strings,
  a `NormalizedPrefix` already freezes to lossy strings, and failing closed
  there would make a *deny* fail open.

### Windows path identity

- **`/` and `\` alike, ASCII case folded, the `\\?\` verbatim prefix stripped
  in either slash spelling, a verbatim UNC head folded to `\server\share`,
  drive letters compared as components** (`windows_identity_components`). A
  `//?/`-spelled access must meet a `\\?\`- or plain-spelled deny: real Windows
  honours only the backslash form, but this matcher holds the separators
  interchangeable, and folding the two differently would leave a deny a
  differently spelled access slips past.
- **`windows` is a parameter in `lex`, never a `cfg!` read**, in
  `starts_with_identity`, `starts_with_collision`, `is_foreign_rooted`,
  `is_discard_device` and `reserved_device_refusal`, with one platform gate at
  each sole call site, so both tables are pinned by tests on every host and
  neither is dark on the other.
- **8.3 short names are an alias, not a fold**, so no key closes them. With a
  deny on `C:\w\secretfile.txt`, a leaf left spelled `SECRET~1.TXT` misses the
  long name and admits the read. The walk instead opens a `~`-bearing leaf,
  file or directory, with attribute access alone and asks the kernel its long
  name (`walk::dealias`); a name it cannot learn refuses the access. A short
  spelling makes an allow miss, a refusal, but a deny miss, an admission; the
  kernel's ACE, being on the object, holds for a child regardless.
- **The Windows allow side keeps its ASCII fold**, so the guard over-grants
  across ASCII case in a case-sensitive folder while the ACL is exact. A stated
  residual: making the Windows walk spell stored names is its own change.

### Firmlinks and the root

- **macOS firmlinks make `/tmp` and `/private/tmp` one directory.**
  Containment folds the alias table it shares with `canon`
  (`canon::firmlink_toggle`), because under Seatbelt `realpath(3)` can fail on
  `/tmp` itself and a grant authored in one spelling must cover an access in
  the other.
- **Depth counts components of the alias-folded form** (`identity_depth`).
  `/private/tmp` spells longer yet is one deep; counting characters would let
  a shallow alias outrank `/tmp/a/b` beneath it
  (`depth_counts_components_not_characters`). Three Windows spellings of one
  directory likewise report one depth.
- **Two spellings of one directory at equal rank tie, and the deny meets the
  allow** (`a_firmlink_alias_does_not_outrank_a_deny`).
- **The root `/` is minted, never frozen.** `realpath("/")` answers the
  *current drive* on Windows, so a frozen ceiling covered one volume, and a
  session launched from `D:` lost a `%TEMP%` on `C:` — the shape GitHub's
  Windows runners have. Folded to zero components it is the universal prefix;
  `NormalizedPrefix::root` and `refreeze` both special-case the bare root
  (`the_root_survives_a_re_freeze_as_the_universal_prefix`).

### Symlinks

- **A prefix carries `surface` and `resolved`, and containment is on
  `resolved` on both sides.** A grant that lexically nests under a shallower
  ceiling but resolves outside it collapses to the empty meet: `{allow /a}`
  met with `{allow /a/link}`, `link → /x`, keeps nothing. A survivor would
  reach the OS sandbox, where `bwrap --bind` follows the source symlink and
  Seatbelt matches lexically, so a child could read the link's target
  (`a_symlinked_grant_cannot_escape_a_shallower_ceiling`, with its positive
  control).
- **A symlinked exec dir covers where it points, at its target's depth**, allow
  or deny alike, so a deeper allow inside the target still beats it
  (`a_symlinked_deny_dir_ranks_at_its_target_depth`).
- **A symlink to an admitted file is admitted**, since it runs that file; a
  symlink under an admitted directory to a file outside it is not. The
  launcher runs the real path it judged, the user's spelling as `argv[0]`.

### Namespaces

- **synod mints guest prefixes with the POSIX fold**
  (`NormalizedPrefix::from_guest`, `lex::fold_dots_posix`), because the guard
  that matches them runs inside the machine, and on a Windows host
  `fold_dots` rebuilt `/work` as `\work`, a relative path in the namespace it
  claimed to name ([[decisions/260726_guest-namespace-prefixes|guest-namespace-prefixes]]).
- **A prefix carries no namespace.** Whose machine a path names is settled by
  the door that mints it; the meet compares resolved forms and nothing else,
  and a guest prefix is re-frozen only by the guard that matches it, inside the
  machine ([[decisions/261006_one-table-two-instances|one-table-two-instances]]).

### Exec identities

- **A bare key is the file the *host* `PATH` finds plus the bundled tool of
  that name**, walked with `SearchCwd::nowhere` on the process's own `PATH`,
  never a scoped override: a planted `/tmp/evil/rg` inherits nothing from
  `rg: allow`, and `git: allow` admits `/usr/bin/git` typed as a path.
- **A bare deny is also a veto on its `command_name_key`**, outranking every
  other rule, so it stops an absolute `/bin/bash` and a link to it alike.
- **Path and dir keys are frozen resolved forms, never re-read**, and two keys
  naming one file meet: `RealPath`'s `Ord` is the host's file identity, by
  components off Windows and by Windows identity components on it
  (`two_path_keys_naming_one_file_meet`).
- **A veto's key folds.** `deny: [bash]` compiles to a folding exact deny and a
  veto, and Seatbelt's veto regex folds, so a stored key would admit in the
  guard a macOS child's `/x/BASH` that the kernel refuses
  (`a_veto_holds_every_spelling_of_the_name`).
- **On Windows the key strips the executable extension** and lower-cases ASCII
  (`name_key_on`, over `WINDOWS_EXEC_EXTENSIONS`), so `bash.exe` meets
  `deny: [bash]`.
- **A copy under a new name escapes a veto**, a declared limit: it is a
  different file with no trace of the denied name. What holds is the
  projection the copy inherits ([[design/grant|grant]] §Concessions), and
  `deputy_prefixes` reports where a write becomes runnable.
- **A multi-call binary is one file**, so a path key cannot tell its applets
  apart; a bare deny still vetoes by name.
- **A head that names no file has no program**, so no table judges it: 127, or
  126 when the file is not executable, never a denial
  ([[decisions/261004_exec-rules|exec-rules]]).

### Carriers

- **A carrier is what `execve` forwards to before the program's own code
  runs**: a `#!` interpreter, up to four hops, and on macOS the root-owned
  shims. Both kernels check a script's interpreter (measured), so without
  carriers an admitted script would die in the kernel.
- **Admitted only on bytes the uid cannot author** (`trusted_real`, a
  link-following walk from `/`); `env` is not followed, since following it
  reads a `PATH` the confined code controls.
- **The guard never consults them; the kernel needs them.** They enter only
  in `kernel()`, as `Carrier` scopes, ranked under `Exact` so an authored deny
  on a carrier file wins and under `Name` so a vetoed name is not allowed
  (`a_carrier_with_an_exact_deny_is_denied_to_the_kernel`,
  `a_carrier_whose_name_is_vetoed_is_not_allowed`). A carrier holds a file by
  mutual containment as a `File` rule does, so firmlink spellings agree
  ([[decisions/261004_exec-carriers|exec-carriers]]).

### What the kernel cannot see

- **argv**: `Only` renders as an allow, and a subcommand restriction is the
  guard's alone.
- **Which bundled tool a re-exec of ral runs**: a tool is ral's own binary to
  the kernel.
- **Landlock is allow-list only**: a deny or veto inside an allowed directory
  is the guard's alone on Linux
  ([[decisions/260906_landlock-exec-layer|landlock-exec-layer]]).
- **Windows has no kernel exec layer**, so the window between check and exec
  stays open there.
- **macOS has no profile stacking**: a process inside one Seatbelt profile
  gets `EPERM` entering a second, so a nested restricting launch is refused
  with attribution, never run under the wider profile.
- **macOS's process row is a budget**: `RLIMIT_NPROC` at the user's count at
  launch plus 512 ([[design/two-enforcers|two-enforcers]]).

### The projection

- **The fs rendering is lexical surface strings.** Each live prefix flattens
  to `surface` once, in `sandbox_projection`, and every backend widens it into
  its own name class at render time; `resolved` has no reader below the fold.
- **`live()` is why no allow beneath a deny ever reaches a backend.** Under
  deny-wins such an allow is dead, and a Windows ACL orders explicit allows
  before inherited denies, so handing one over would invert the rule
  (`the_live_allows_are_those_no_deny_holds`).
- **`pinned_dirs` derive from *rendered* names**: the ancestor closure of each
  rendered deny within a rendered write name, so an alias's chain is pinned
  beside its target's ([[internals/seatbelt-profile|seatbelt-profile]]).
- **The discard device is excused before either region** — `/dev/null`, or on
  Windows exactly `\\.\NUL` in either slash spelling and any case — since
  nothing reaches the disk through it. **DOS reserved device names are refused
  before either** — `NUL`, `CON`, `PRN`, `AUX`, `COM1`–`COM9`, `LPT1`–`LPT9`,
  any case, any extension, trailing dots and blanks trimmed, so `COM1.log` and
  `nul .txt` too — since they name no object to judge. For `NUL` the refusal
  asks `Did you mean \\.\NUL?`.
- **`Guarded`** answers a write onto the boot-pinned enforcer binary, judged by
  inode so a hard link names it too, *before* the stack is folded: a stack with
  no fs opinion is `Unrestricted` and exactly the case to catch.

### Freshness

- **The guard re-freezes every prefix against the live disk on every check**,
  so a deny holds its object the moment it exists, under any spelling.
- **The projection freezes once, at spawn**, because that is when the profile
  is written. That freshness is the whole difference between the two readers
  of one fold.
- **Exec tables are compiled afresh on every question**, because a bare key
  follows the host `PATH`; the projection renders the very table in the
  `Admitted` that judged the launch.

### Absent denies

- **An absent deny under a writable prefix reaches no Linux or Windows
  kernel**: a mount needs a mountpoint and an ACE an object, and making either
  writes the host. ral's own creation is refused in process; a child may make
  the name in that launch, and the next launch finds an object to mask or
  stamp. macOS holds it from the start, Seatbelt's rules being negative over
  names (`docs/SPEC.md` §12.3;
  [[internals/capability-enforcement|capability-enforcement]]).

See also [[design/grant|grant]], [[design/two-enforcers|two-enforcers]],
[[related/access-control-algebra|access-control-algebra]] (restrict as meet
against the literature), [[map/core/capabilities|map: capabilities]].
