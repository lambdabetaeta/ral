---
status: active
generated_at_commit: b49b3823
verified_at_commit: 90479dea
anchors: [collision_key, Identity, Polarity, Allow, Deny, Region, live, Respelled, dealias, open_attributes, holds, command_name_key, respelled, evicts]
---

> **Mechanics superseded 2026-10-06** by
> [[decisions/261006_one-table-two-instances|one-table-two-instances]]. The
> decision stands — a deny under `Identity::Collision`, an allow under
> `Identity::Stored`, the rule's `Polarity` choosing — and the measurements
> below stand as written. The prefix sets that carried it are gone: fs is a
> `Region`, a table whose rules speak by polarity through
> `FrozenPath::contains::<P>`; `outside` is `Table::live`, and the
> stored-holding test behind the respelled refusal is `Table::respelled`. The
> corner cases are gathered in [[design/authority-tables|authority-tables]].

# Denies hold under every spelling

**A deny is judged under the coarsest identity any filesystem gives a name;
an allow under the name as stored.** One function, `lex::collision_key`,
computes the deny's key; the type of a prefix set says which relation it
uses, so a deny can never be asked the exact question.

## Context

The guard compared names byte for byte off Windows and by ASCII case on it,
for allows and denies alike. That was right only where every name has one
spelling. On case-insensitive APFS, `deny: [cwd:/.env]` with `.env` absent
lost to `echo key > .ENV`, which left `key` in the object `.env` names: the
leaf was absent, so it kept its spelling, and the bytes differed. The deny
re-resolves on every check, so once the object existed every *later* access
was refused; the hole was exactly the access that creates it, which is the
write a deny is there to stop. NFC against NFD was the same hole, and
non-ASCII case on NTFS too.

Case-insensitivity belongs to the volume, not the OS. synod's Linux guest
reads APFS and NTFS shares, ext4 and f2fs fold per directory, and APFS has a
case-sensitive variant. So the fold can be neither a `cfg` nor a question put
to the filesystem (Rejected, below).

## What was measured

macOS 26.5: default (case-insensitive) APFS and a case-sensitive APFS image.
Every Seatbelt row ran under `(allow default)` plus one rule, beside a
non-matching control that was admitted.

- **An existing name is already spelled as stored.** `realpath(3)` and
  `F_GETPATH` return the directory's spelling, for case and for normalisation,
  on both volume kinds. A deny frozen over an existing path carries it; only
  an absent name keeps the spelling it was written in.
- **The volume answers per path, and only for what exists.**
  `pathconf(_PC_CASE_SENSITIVE)` is `0` on the Data volume and `1` on the
  image, and `ENOENT` for an absent path.
- **Default APFS** takes two names for one exactly when they agree under
  *canonical caseless matching* (Unicode D145, `NFD(casefold(NFD(s)))`), on
  all 18 probe pairs: ß≡ss, ß≡ẞ, ſ≡s, K (Kelvin)≡k, ﬁ≡fi, σ≡ς, µ≡μ,
  Å≡Å (angstrom) and NFC≡NFD merge; ı/i, ı/I, İ/i and fullwidth Ａ/a stay
  distinct. **Case-sensitive APFS** merges canonical equivalents only.
- **Seatbelt** matches `literal`, `subpath` and `regex` under that same
  equivalence, on every volume, for absent and existing names alike. A deny on
  `Secrets` refuses `mkdir secrets`, a write under `secrets/` and
  `cat SECRETS/t`; a deny spelled `secrets` refuses the stored `Secrets`; an
  upper-cased ancestor in the rule still matches; an NFC rule refuses an NFD
  create and the reverse. Its fold is canonical caseless matching, nothing
  more: ı/i and Ａ/a are admitted.
- **Seatbelt's allows fold too.** On the case-sensitive volume, where `Work`
  and `work` are distinct objects, a deny on `Work` refuses `work`, and an
  allow over-grants: `subpath Work`, `subpath WORK` (a name nothing stores)
  and `literal …/Work/t` each admit reading `work/t`.
- **NTFS**, on the Windows CI runner, upcases each UTF-16 unit through the
  volume's `$UpCase` table and does not normalise. The table keeps ı apart
  from I, though Unicode's simple uppercase joins them: `fıle` and `FILE` are
  two names.
- **ZFS, from its source** (OpenZFS `zfs_vfsops.c`, `u8_textprep.c`, its
  tables decoded over every code point). `casesensitivity=insensitive` or
  `mixed` upcases each character by Unicode 5.0's simple uppercase, which
  sends ı to I, then applies the dataset's `normalization`: none, `formC`,
  `formD`, `formKC` or `formKD`, both fixed at creation. Under `formKC` and
  `formKD` it merges compatibility equivalents: fullwidth Ａ≡A, 𝐚≡a,
  no-break space ≡ space.
- **Linux, from documentation.** Casefold (ext4, f2fs: NFD plus casefold) is
  canonical caseless matching by construction.

## Decision

- **The key.** `collision_key(name)` is
  `NFKD(casefold(NFKD(casefold(NFD(upper(NFKD(name)))))))`: compatibility
  caseless matching (Unicode D147) of the uppercase image of the
  compatibility decomposition, through ICU4X (`icu_normalizer`,
  `icu_casemap`, one Unicode version for every step). ASCII is its
  lowercase; a name that is not UTF-8 is its own key. Measured over every
  code point, the shipped key splits no class that APFS, Seatbelt or ZFS
  under any setting merges; NTFS, Linux casefold and a case-sensitive volume
  are finer, which is fail-closed. NFKD comes first: `𝚤` decomposes to ı,
  which ZFS upcases, yet has no uppercase of its own. A key is a function,
  so it holds the join of the classes: `𝚤` meets I, though no one volume
  merges them.
- **Two relations, one kernel.** `lex::path_within` takes an `Identity`:
  `Stored` (bytes; ASCII case on Windows) or `Collision` (`collision_key` per
  component, so no fold crosses a component boundary).
- **The set's type picks.** `PrefixSet<Allow>` meets and judges `Stored`;
  `PrefixSet<Deny>` joins and judges `Collision`; `outside` drops an allow by
  the deny's relation. `fs_verdict` is unchanged: the type does the work.
  Nothing outside `core/src/path/` chooses an identity.
- **Exec reads names the same way.** An exec rule holds a name as its
  polarity reads it: an allow dir or file as stored, a deny dir or file under
  `Collision` (`RealPath::within::<P>`, one `holds` in `capability::exec`
  choosing `P` from the rule). A veto's key is `command_name_key`, the host's
  name key under `collision_key`. Renaming defeats a veto anyway, which would
  argue for leaving it stored; but `deny: [bash]` is one grant key compiling
  to a folding exact deny and a veto, and Seatbelt's veto regex folds, so a
  stored key would refuse a macOS child `/x/BASH` that the guard admits. A
  folded match ranks exactly as a stored match of the same
  key, so exec's precedence is unchanged
  ([[decisions/261004_exec-rules|exec-rules]]).
- **Allows meet, denies join.** The exec meet keeps a key met to Deny only
  where a layer wrote that deny (`met`): a key one layer never mentioned is a
  default, and written down as a deny it would hold every other spelling of
  that name. `evicts`, which widening (`--extend-base`) sweeps with to drop an
  allow dir a deny dir clashes with, asks containment under `Collision` both
  ways: mutual containment is one rank, where the guard's tie already denies,
  so the sweep agrees with the verdict.
- **The denial says why.** When a deny holds the access and none holds it
  as stored (and, for exec, no veto names it), fs and exec refuse in one
  sentence, at the guard and at head admission alike:
  `fs write denied by grant: /w/.ENV is the denied /w/.env under another
  spelling (case or Unicode form); a deny holds under every spelling`.
- **Windows 8.3 aliases are named, not folded.** A walk that meets a `~`
  opens the leaf, file or directory, with attribute access alone and asks the
  kernel its long name; a name it cannot learn refuses the access. A short
  spelling is an alias, not a fold, so no key could close it: the fix names
  the object. A short spelling can only make an allow miss, a refusal, but it
  makes a deny miss, an admission. With a deny on `C:\w\secretfile.txt`, a
  leaf left spelled `SECRET~1.TXT` is compared with the long name by the
  in-process guard, which misses and admits the read; the kernel's ACE, being
  on the object, holds for a child.

## Why this key

- **At least as coarse as every identity in play.** A deny that under-merges
  lets a spelling through, so the key takes case-insensitive APFS and
  Seatbelt, NTFS's `$UpCase`, Linux casefold and ZFS under every setting
  together. Where a volume is finer, down to byte-exact ext4 and
  case-sensitive APFS, it over-merges: fail-closed.
- **Exact on ASCII.** The key of an ASCII name is its lowercase, and a
  non-ASCII character whose key is ASCII (K, ſ, ﬁ, Ａ) lands on that same
  lowercase, so the ASCII fast path is no approximation.
- **Stable.** Unicode's case-folding and normalisation stability policies
  mean a newer table never splits a class an older one merged.
- **One Unicode version.** `icu_normalizer` and `icu_casemap` are one ICU4X
  release, so normalisation and case folding cannot disagree on a table.

## Why a deny folds and an allow does not

A fold merges names, and merging a deny's names withdraws authority while
merging an allow's grants it. **Each polarity takes the identity whose error
is a refusal.**

- A deny that under-merges is the hole: the create of the name slips through.
  One that over-merges refuses a distinct sibling nobody meant, on a
  case-sensitive volume.
- An allow that folded would over-grant on every case-sensitive volume: a
  grant on `Work` would admit `work`. Held as stored, it errs only towards
  refusing.
- Across layers the split carries on: denies join and allows meet by stored
  name, so two layers that spell one directory differently grant it to
  neither.

## Rejected

- **Fold only on macOS and Windows (`cfg`).** Case-insensitivity belongs to
  the volume, not the OS, and a `cfg` would give one grant three meanings.
- **Ask the filesystem** (`pathconf` on the nearest existing ancestor,
  `FileCaseSensitiveInformation` on Windows).
  - It makes `covers`, `outside` and `covering` depend on the disk at check
    time; the prefix algebra is a pure function of two policies.
  - An absent tail inherits nothing knowable: the flag is per directory on
    Windows and ext4, and is set on an empty directory, so a same-uid writer
    can create the tail with the other flag after the query.
  - Linux has no query for virtiofs or 9p.
  - All it buys back is the over-deny of distinct siblings on a
    case-sensitive volume, which Seatbelt refuses anyway.
- **Fold allows too.** The over-grant above; the Windows allow side's ASCII
  fold is a residual, not a precedent.
- **Store the key in `real`.** It is rendered into bwrap destinations,
  Seatbelt rules and the Windows SID hash, and on a case-sensitive volume a
  folded spelling names a different object. A comparison key belongs in the
  comparison, not in the name.
- **Refuse a deny on an absent path.** It breaks the documented `cwd:/.env`
  grant.
- **Mask every existing colliding sibling** on Linux and Windows, so that the
  kernel matches the guard's over-deny. It enumerates directories, races, and
  adds machinery to enforce an over-approximation.
- **NFKC_Casefold.** It misses ı≡I, which ZFS merges, and deletes
  default-ignorable code points (ZWJ, soft hyphen), which no filesystem in
  play ignores.
- **std `to_lowercase(to_uppercase(·))`.** It misses ß≡ẞ, which APFS merges.
- **Leave exec deny keys stored.** One grant would mean two things, fs
  folding and exec not, and on macOS the kernel folds exec denies anyway, so
  guard and kernel would disagree on one rule.
- **`RealPath::within(dir, Identity)`.** It lets exec choose an identity.
  Polarity is the caller's statement of which way a rule speaks; identity is
  `path`'s ([[invariants/grants-judge-objects|grants-judge-objects]]).

## Consequences

- **macOS renders nothing differently.** Seatbelt matches every rule under
  canonical caseless equivalence, absent and existing names alike, on every
  volume. On APFS that is all a rendered deny needs: the classes the key adds
  are distinct objects there.
- **Linux and Windows hold an existing deny under every spelling already.** A
  bwrap mask is mounted over whatever object the kernel's lookup finds, under
  the volume's own identity; Landlock rules are by descriptor; a Windows ACE
  attaches to an object.
- **An absent deny reaches no Linux or Windows kernel**, a mount or an ACE
  needing an object, so for a spawned child only macOS holds it. A rendering
  that materialised an object would inherit the volume's identity, which is
  exactly enough; one that refused by name would have to refuse the whole
  `collision_key` class under the covering write prefix.
- **Over-deny, accepted.** On a case-sensitive volume — Linux, case-sensitive
  APFS, a case-sensitive NTFS folder — a deny on `Secrets` also refuses the
  distinct `secrets` in process, where the Linux and Windows kernels would
  not; and on every volume but a compatibility-normalised ZFS one, a deny on
  `secrets` also refuses the distinct `ｓｅｃｒｅｔｓ`. That is an
  over-approximation in the guard, not a weakness in either enforcer. The
  denial says why.
- **Residuals, stated.** On a case-sensitive APFS volume Seatbelt's *allows*
  also admit case variants for a spawned child, which SBPL cannot express
  otherwise. The Windows allow side keeps its ASCII fold, so the guard
  over-grants across ASCII case in a case-sensitive folder while the ACL is
  exact; making the Windows walk spell stored names is its own change. On
  Linux an exec deny inside an admitted dir never reaches the kernel, so a
  child is not refused the distinct `/x/evil/t` the guard refuses under a
  deny on `/x/Evil`. On an OpenZFS volume under macOS, Seatbelt's canonical
  fold lets a child create an absent denied name spelled with ı or a
  compatibility form. HFS+ is not in play: its case-insensitive compare skips
  sixteen invisible format characters (ZWJ, bidi marks, BOM) and pairs
  Georgian Asomtavruli with Mkhedruli, neither of which the key folds.

See [[invariants/grants-judge-objects|grants-judge-objects]],
[[design/two-enforcers|two-enforcers]],
[[internals/seatbelt-profile|seatbelt-profile]].
