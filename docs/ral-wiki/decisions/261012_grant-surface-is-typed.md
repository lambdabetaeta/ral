---
status: active
generated_at_commit: 773daa1062bb84e64c799cf502a0a5288c173f20
verified_at_commit: 002c654a
anchors: [Place, Selector, FreezeCtx, Net, EditorVerb, ShellVerb, Holds, ExecGrant, Flag, drain_nested]
---

# The grant surface is typed data

**A grant is a record of tagged values, not a record of strings. The authority
model underneath was already typed (places, executables, a chain of network
modes, verbs); the surface now says so, so a misspelling is a type error
before the program runs, and the decoder parses nothing back out of a string.**

Landed in 002c654a. The exec and fs algebra it deliberately left alone was
then replaced by
[[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]];
where that changed a statement here, the section says so.

## Context: what was wrong

- **Policies were strings.** An exec entry was `'allow'`, `'deny'` or a list:
  one sum type spelled as "string or list", so `'alow'` failed only when the
  grant was decoded at run time. An exec record's labels (command names and
  sigils) are an open set no table can declare, so `exec` could only be
  `Holds::Decoded`.
- **A first-argument filter is not a subcommand policy.** It ignored the
  remaining arguments and understood neither options nor command semantics.
- **Places were strings with a prefix language** (`cwd:`, `tempdir:`,
  `gitdir:`, `~`, `xdg:NAME`, `path:`, `system:`, a trailing `/` for a
  directory), parsed in a cascade by `FreezeCtx::path` and `freeze_exec_grant`.
- **`net: true` and `detach: true` restricted nothing yet activated
  confinement.** They are neutral in authority under meet and widen, but
  `is_restrictive()` counted their presence, switching on resource limits,
  process braking and loader-variable scrubbing.
- **`editor` and `shell` were records of booleans.** A `false` field was a
  sticky veto under `--extend-base` and an omitted field defaulted to `false`;
  no record type expresses an optional field, which is a variant's job
  ([[invariants/optionality-via-variants|optionality-via-variants]]).
- **Spec and code disagreed:** SPEC listed `xdg:runtime`; `XdgKind` has no
  `Runtime`.

## The surface

```ral
grant [
    exec: [
        allow: [`name git, `name rg, `absolute /bin/sh, `cwd tools/build, `dir `system],
        deny:  [`name bash, `dir `absolute /opt/homebrew],
    ],
    fs:   [read: [`cwd ., `xdg-data ., `absolute /usr/share], write: [`cwd build, `tempdir .], deny: [`cwd .git]],
    net:  `offline,
    editor: [`read],
    shell:  [`chdir],
] { … }
```

Omitted axes inherit; written axes restrict. The values are ordinary record,
list and variant syntax. A tag's payload is greedy and parentheses group only
inside `$[…]`, so `dir`'s payload is a tagged value written as is:
`` `dir `cwd tools ``.

- **Every place is tagged, and every anchor tag takes a subpath.** Lists are
  homogeneous and each tag has one payload type, so ``[`cwd, '/tmp']`` (a
  string beside a tag) and ``[`cwd, `cwd build]`` (a tag with and without a
  payload) cannot typecheck. That, not taste, forces `.` for the anchor itself
  and a tag on every path.
- **Places** name one path, resolved where the grant is frozen, so a `restrict`
  record still crosses the wire unfrozen and binds on the receiving side
  (`FOValue` carries variants). `absolute` takes an absolute path; `cwd`,
  `home`, `tempdir`, `gitdir` and one `xdg-*` tag per `XdgKind` are anchors
  (one tag per kind, so a misspelled kind is a type error; `xdg-runtime` is
  not a tag). A bare string, `~`, `xdg:NAME`, `path:`, `system:` and the
  trailing slash are gone.
- **Selectors live under `dir`.** `path` and `system` (now also `homebrew`,
  added by the Belnap decision, which moved Homebrew out of `system`) name
  directories the environment chooses. Their expansion has no containment
  guard, so a `$PATH` entry can be `$HOME` or `/`; writing one would let a
  child plant programs where the user's unsandboxed shell looks (`~/.local/bin`,
  a user-owned `/opt/homebrew`). Reading needs no selector: a directory is read
  by naming it as a place, and whatever a grant runs is readable. Splitting
  the types (selector tags only in `DirTarget`, not in `Place`) also makes
  ``allow: [`path]`` a static error, where one shared type would accept a value
  the decoder always refuses. Decided with Alex: selectors only under `dir`.
- **An exec target is `name`, a place, or `dir`.** Directory scope is spelled,
  never inferred from a stat (the trailing slash and `RawExecGrant::dirs` go).
- **Lists are sets, repeats are idempotent.** An entry written twice, in one
  list or through spreads of two computed lists, adds nothing; a target in both
  `allow` and `deny` is denied, as two spellings freezing to one key are; a
  place may be both read and written. A rule refusing repeats would have to
  tell a written list from a computed one, which the decoder cannot, and would
  refuse ``[...$base, `cwd .]`` for an overlap that changes nothing. (An early
  draft refused repeats; a later review made them idempotent.)
- **Nested tables are records,** as a profile is. A map, `[:]` included, is
  refused statically, as `return ["exec": …]` is (SPEC §12.12), with the
  record spelling ("a grant table is a record; for an exec table that admits
  nothing write `exec: [allow: []]`"). So is `()`, which stays the empty
  *profile* and is not an empty nested table: a profile may be `()`, the only
  difference between the two depths. The empty table is an empty list:
  `exec: [allow: []]`, `fs: [read: []]`. Decided with Alex.
- **`net` has one value, `` `offline ``.** A grant only narrows, so `net: true`
  restricted nothing and is not a spelling; omit `net` to inherit.
- **`editor` and `shell` are verb lists,** `detach` a `shell` verb (below).
- **No spelling widens.** Values restrict or are neutral. The socket plan's
  mandatory IPC restrictions are envelope invariants and cannot be lifted by
  grant syntax.

### Payloads

- **`P` (the `absolute` payload) is classified on the raw string, before any
  folding, host first.** Absolute under the host's rules is live; absolute only
  under the other platform's is a **dead grant** (`FreezeCtx::place` answers
  `None`: it names no path here, matches nothing, and in `deny` denies
  nothing), so one profile loads on both platforms; under neither is refused.
  Host folding would otherwise read `C:/../x` on POSIX as the relative `x`.
  The table (current; SPEC §12.2 carries it):

  | Raw `P` | POSIX | Windows |
  |---|---|---|
  | `/x`, `/` | absolute | dead |
  | `C:\x`, `C:/x`, `\\server\share\x` | dead | absolute |
  | `\x` (root-relative) | refused | refused (names no drive) |
  | `C:x` (drive-relative), anything relative | refused | refused |

  Before this, `/x` was dead on Windows and `C:\x` a relative-path error on
  POSIX; the table makes the two symmetric. `is_foreign_rooted` counted a
  leading `\` only because it ran after folding; on raw input it counts `/`
  alone. `/` is the universal prefix on POSIX and dead on Windows, so no
  authored place yields Windows' universal prefix: only the internally minted
  `FrozenPath::root` does. A later refinement made a well-formed forward-slash
  UNC `//server/share/x` live on both and refused malformed UNC roots; see
  [[design/capability-freeze|capability-freeze]]. Decided with Alex.
- **`S` (an anchor subpath) is lexical.** `.` is the anchor; `..` folds at
  freeze and may leave the anchor (`` `cwd ../sibling ``), the XDG escape guard
  still judging the result. The empty string, a leading separator and a drive
  prefix `X:` are refused on every platform, judged lexically so a profile
  fails alike everywhere. Before, a leading separator was silently stripped
  (`relative_suffix`), and on Windows `PathBuf::join` with `C:x` replaces the
  anchor.
- **Run-time prefixes are refused in every payload.** `~` and `xdg:NAME` are
  expanded by the resolver when a path is used, not when a word is evaluated,
  so `` `absolute ~/x `` would reach the decoder as a relative path and
  `` `cwd ~/x `` would name a directory called `~`. The refusal names the
  place meant (`` `home x ``, `` `xdg-config x ``).
- **One resolver.** `FreezeCtx` freezes a place to at most one path and a
  selector to many; the consumer checks the result suits its use (an
  unwrapped exec place that is a directory is refused with a hint to use
  `dir`).
- **The freeze guards are unchanged:** the XDG escape check, the `.git`
  pointer claim-back, the refusal of an unset home, the refusal of an anchored
  place that freezes relative (a relative `HOME` or `TMPDIR`), and freezing at
  decode time.

Each error names the right spelling: `` `absolute src `` says to write
`` `cwd src ``; `` `cwd /x `` says `` `absolute /x ``; `` `cwd '' `` says
`` `cwd . ``; `` `cwd 'C:x' `` says a drive belongs in `absolute`; `\x` on
Windows says it names no drive.

### `exec`

- **`name N`** keeps the name-grant semantics: the executable found through the
  host process's `PATH`, plus a bundled tool of that name; scoped `PATH`
  overrides cannot redirect it; a name deny vetoes the name everywhere. `N`
  must be a bare command name; a path-shaped payload is refused with a hint to
  `` `absolute `` or `` `cwd ``.
- **A place** is one executable (`ExecKey::Path`). A directory place keeps
  today's directory stat and the hint *did you mean `` `dir `absolute /usr/bin ``?*
- **`dir D`** is `ExecKey::Dir` for each path `D` resolves to:
  `` `cwd tools/build `` is one executable, `` `dir `cwd tools `` everything
  beneath `tools`, including programs created later.
- **Allow or deny, regardless of arguments.** `only`, legacy subcommand lists
  and `Verdict::Only` are removed, and an old argument restriction is never
  read as an unrestricted allow: unsupported policies fail before the grant
  body runs and a replacement needs an explicit allow or deny.
- **`exec: [allow: []]` admits no executable;** omitting `exec` inherits.
- **`fs`** is three lists of places (`read`, `write`, `deny`); `name`, `dir`
  and selectors are refused there. `fs: [read: []]` keeps only the floor.

### `editor` and `shell` as verb lists

`editor` takes `` `read ``, `` `write ``, `` `tui ``; `shell` takes
`` `chdir ``, `` `detach ``. Every verb is gated by `Shell::check` and none
reaches the OS projection; each withholds what its flag withheld, `chdir`
gating `cd` (which moves the session's cwd) while `within [dir: …]` scopes one
and is judged by fs alone.

- **Restriction.** A verb is permitted only where every layer that supplies the
  axis lists it. ``shell: [`chdir]`` therefore withholds `detach`: the safe
  direction, since detach is the authority that outlives the session. Listing
  every verb is neutral, like omission; an empty list withholds every verb
  (`deny_all` holds empty sets).
- **Extension joins.** A list has no spelling for a veto, so under extension an
  allowance never removes authority: a base cannot withhold a verb against an
  extension (`chdir: false`, `detach: false` were sticky before), and a base
  that omits the axis keeps every verb. This is the Belnap decision's rule 3,
  adopted here because the new surface leaves no other reading of a list;
  the Belnap algebra computes the same thing as `Capabilities::extend` over
  `Join`/`join_top`.
- **Representation.** `EditorVerb` and `ShellVerb` enums (`Ord`,
  `VariantArray`, `EnumString`, kebab-case), `Capabilities.editor` and `.shell`
  as `Option<BTreeSet<_>>`; `EditorPolicy`, `ShellPolicy` and
  `Capabilities.detach` are deleted. `Flag` is `Editor(EditorVerb) |
  Shell(ShellVerb)`: `vote` is membership, `key` spells the grant, and
  `GrantStack::permits` and every caller's gate and order are unchanged. On the
  wire a verb set is a sequence of kebab-case tag names.

### `net` is a chain

`Capabilities.net` is `Option<Net>`, `enum Net { Offline }`. Meet and join are
both `Offline`, so the veto stays sticky. The effective mode is the meet of the
grant's opinion and the host ceiling; `Option<Net>` is a point of
`Offline < Managed < Open IP` minus the top, and `Net::Managed` is where a
grant would ask for Managed. It is not added: the socket plan keeps Managed
controller-only. A place can name a socket path and the tag does not stop a
process connecting, so fs authority and service authority are separate: named
IPC refusal is enforced at launch, independently of fs grants, by that plan,
and a future endpoint delegation needs its own authority design. The
projection is online iff no layer says `Offline`; its own `net: bool` stays
until that plan's `NetworkMode` replaces it. Typed places establish none of
that plan's socket guarantees and can land independently.

## The checker

Tagged values give exec a closed vocabulary and nested-table ascription checks
optional keys; together they remove the blocker the `GRANT` table comment
named.

- **Contextual handler inference is separated from shape construction.**
  `check_option` treated every `Holds::Shaped` as a catch-all handler. Now
  `Holds::CatchAll` (`within`'s `handler`) alone invokes it; `Shaped` infers an
  ordinary value and unifies it with the minted type; one helper builds the
  expected type for inline checking and scratch ascription; key lookup and
  clash reasons are shared. No second grant schema and no separate profile
  checker.
- **`Holds::Table(&'static Table)` for nested tables.** A nested table has
  optional keys, which a row without presence flags cannot express, so it is
  *ascribed*, as a contract file is
  ([[decisions/260930_a-table-never-enters-the-unifier|a-table-never-enters-the-unifier]]),
  by the same recursive function, never unified against a record type.
  - Every nested value is inferred normally (written and computed records
    follow one rule; duplicate-field checks, spreads and polymorphism are
    preserved; no contextual record inference is added).
  - Each nested check is queued with its span and **drained once, in
    `typecheck`** (`contract::drain_nested`), after the pending labels settle
    and before the contract ascription, in a scratch unifier, against the type
    as it then stands. Deferring makes the check independent of statement
    order. The one-shot sessions (`alias_arm_scheme`, `binding_value_scheme`,
    `catch_all_stands_in`) re-infer bodies `typecheck` already accepted and
    drop their queue unread.
  - **The check never constrains.** A `Shaped` value constrains a bound
    variable (`net: $v` fixes `$v`); a `Table` value is only checked. A type
    still unresolved when the queue drains, including a generic function's
    parameter that no later instantiation constrains, goes to the runtime
    decoder, which cannot tell a record from a map and reads both, as the
    top-level decoder does.
  - Ascription is ral's unification: a value whose variant row a `case` has
    closed to a subset of the vocabulary is refused, as wherever ral meets a
    wider closed row. Grants add no subtyping.
  - The top-level option syntax and the profile contract are untouched.
- **Closed rows come from the Rust enums.** The verb rows are minted from
  `EditorVerb` and `ShellVerb`, the `xdg-*` tags from `XdgKind`, through
  `VariantArray` and kebab-case names, so a new verb or XDG kind is one
  variant; anchors, `absolute` and selectors are declared once. `GRANT` holds
  `exec` and `fs` as `Table`, and `net`, `editor`, `shell` as `Shaped`
  (`<offline>`, `[<read | write | tui>]`, `[<chdir | detach>]`); `EXEC`'s
  `allow` and `deny` are `[ExecTarget]` (`name: String`, the place tags,
  `dir: DirTarget`, `DirTarget` being place and selector tags); `FS` lists are
  `[Place]`. A `Ty::List` cannot sit in a `static`, which is why these are
  `Shaped`.
- **Every clash at a closed row names the row's tags and offers the nearest.**
  A variant clash surfaced as `RowExtraField`, shared by records and variants
  and worded as a missing record field with the generic `available:` hint
  first. So `RowExtraField` now says whether its row is a record's or a
  variant's and a variant clash is worded as a tag, everywhere in ral and not
  only in grants; the nested ascription knows which vocabulary it checks
  (place, exec target, `dir` payload, verb) and words the clash at the
  value's span; for a `Shaped` key the key's reason takes precedence over the
  generic row hint. The messages: `` `cwdd `` is not a place, with the places
  and the nearest; `` `path `` names directories to run programs from, so write
  `` `dir `path `` in an exec list (and fs lists take places); `net: true`
  says a grant only narrows, omit `net` or write `` net: `offline ``.
- **The runtime decoder still validates everything:** profiles typed at a
  variable, values unresolved when the queue drains, and `restrict` records
  off the wire. A nullary tag takes no payload there too; an explicit `()` is
  refused.
- **The drift test was repaired.**
  `every_declared_grant_key_is_handled_by_decode_capability_map` compared a
  prefixed refusal with the unprefixed `unknown_key`, so it passed whatever the
  decoder did. It now feeds every declared key, at every depth, a valid
  representative value and requires decoding to succeed, and a second test
  refuses an unknown key at each depth in that table's words.
- The `GRANT` table comment says what is checked statically and what stays
  decoded.

## The model

- **`Verdict` lost `Only`** (this plan: `Allow | Deny`; the Belnap decision then
  replaced it with `Evidence`). Everything serving `Only` went: its meet and
  widen set operations, `ExecDenial::Subcommand`, the argv-first check,
  `admit_label`, its diagnostic and rendering branches, the kernel
  projection's `Only`-to-`Allow` conversion. The one entry point
  `GrantStack::admit(program, args)` stays: arguments still belong to the
  admitted launch though permission no longer depends on them.
- **`ExecGrant`'s map is private.** It was `pub` and written directly
  (projection, deputy, tests), so the decoder's "one insertion path" did not
  exist. Outside `lattice.rs` it is built only by `Default` and
  `FromIterator`, read through `iter()` and `get()`, pruned through `retain`
  (removes entries, rewrites none; exarch's `drop_dead_exec_grants`), and tests
  `collect`. A key is a frozen surface (`FrozenPath`'s `Ord` puts the surface
  first): `` `cwd tools/x `` and `` `absolute /work/tools/x `` are one key,
  while two spellings through a symlink are two keys, kept apart and met only
  when compiled. The Belnap decision later added `every`, replaced the meet
  insertion by `join_insert` (the plan's `meet_insert`) and `Widen` by `Join`;
  the privacy and the single insertion path stand.
- **Activation is stated per axis.** `Capabilities::is_restrictive` was true
  when some supplied axis restricts: exec and fs whenever supplied, `net` when
  `Offline`, a verb set unless it lists every verb. The Belnap decision
  replaced the exec and fs clauses with `is_top`, making activation
  denotational. `GrantStack::is_restrictive` folds the layers and
  `Shell::has_active_capabilities` delegates to it; no activation marker is
  added to the decoder, wire format or launch state. Transitive consumers
  were audited: resource limits, process braking and loader scrubbing, pipeline
  braking, the assertion in `guard/grant.rs`, and exarch's `grant_summary`,
  which had a second private predicate calling a layer ambient when exec, fs and
  net said nothing, so a layer withholding `detach` read as "every command,
  path, and network call is permitted" and no summary printed a verb. Ambience
  is now `!is_restrictive()` and a supplied verb set is printed, an empty one as
  "(none)".
- **A neutral policy launches with ambient behaviour.** A profile containing
  only `net: true`, `detach: true` or fully enabled boolean policies activated
  protections before; its neutral replacement does not. Accepted and
  documented rather than kept through a hidden marker. A neutral layer inside a
  restrictive stack cannot turn protections off; profiles keeping an fs, exec,
  offline or verb restriction keep their activation. Also accepted: an
  extension can add a verb its base lacks.
- **The wire changed; the engine protocol was bumped** (15 to 16 here, 17 by the
  Belnap decision). The seed and the warrant have one pinned binary at both
  ends; the engine does not: `Run.caps` carries the grant stack to synod's
  guest, whose engine ships in media built earlier. The bump plus rebuilt guest
  media makes a stale guest fail the packaging check and the attach handshake,
  not the first dispatch
  ([[design/engine-protocol|engine-protocol]]). `Capabilities` formerly ignored
  unknown fields, so an old `detach: false` would have vanished, widening, in a
  peer that read it; it now denies unknown fields, as `FsPolicy` already did.
- **Decoder and freeze types** (`guard::freeze`, public): `Place` is
  `Absolute | Cwd | Home | Tempdir | Gitdir | Xdg(XdgKind, _)` and `Selector`
  is `Path | System` (`Homebrew` joined). `FreezeCtx::place` returns
  `Result<Option<FrozenPath>, PolicyError>` (`None` the dead grant) and
  `FreezeCtx::selector` a `Vec`. `FreezeCtx::path`, `paths`, `absolute`, the
  special cases in `freeze_exec_grant` and `looks_like_path_or_sigil` are
  deleted; `parse_literal_sigil` and `relative_suffix` left the freeze, and
  `parse_xdg_token` survives only for the run-time prefix in `path::sigil`.
  exarch's `deny_layer`, the one production caller outside the decoder,
  freezes its credential and restrict-file paths (absolute host paths) as
  `Place::Absolute` through `FreezeCtx::place`; the test helpers that froze
  sigil strings build places too. The per-axis decoders collect lists into
  sets; `allow` and `deny` share target decoding and executable-path
  validation; verb lists decode through the enums' `EnumString`, an unknown
  verb listing the offered ones.
- **Hints were rewritten, not dropped.** Retained checks keep their hints in
  the new syntax; advice recommending subcommand lists is removed; refusals of
  obsolete policies explain that argument restrictions are no longer supported.
  No compatibility parser, and no fallback to `Allow`.

## No argument filtering, and what a future one must be

Argument filtering is removed, not carried into the new syntax. A future
argument policy needs a separate design over the actual argument vector: it
must preserve argument boundaries and say whether a match covers each whole
argument and whether extra arguments are permitted; never reconstruct shell
text by joining argv; define meet, extension and refusal messages first
(regex string equality is not policy intersection); and keep the central
admission function without scaffolding for a matcher now. Such a guard would
govern direct ral launches only: the OS enforcers cannot impose argv
predicates on execs performed inside admitted children
([[invariants/exec-argv-is-words|exec-argv-is-words]]).

## Migration principles

There is no compatibility layer. Allow and deny judgments were preserved,
including under nesting; first-argument policies have no automatic replacement
and are rejected rather than broadened. Per-name verdicts became grouped target
lists (`git: 'allow'` to `` allow: [`name git] ``); an anchored file key keeps
its anchor and is never widened to its containing directory
(`'cwd:tools/build'` to `` `cwd tools/build ``, `'cwd:tools/'` to
`` `dir `cwd tools ``). A boolean-era shell/detach pair migrated jointly: the
new list names exactly the verbs with true votes, and one that would name every
verb is written as omission (in a base) except in an extension, where it adds
verbs. A neutral `net: true` line was deleted and `net: false` became
`` `offline ``. Messages that spell grant syntax (`sandbox/diag.rs` hints,
`ExecKey`'s `Display`, the guard's refusals) render a file as an anchored place
and a directory with its `dir` constructor, so deny summaries keep scope
without the trailing-slash convention. The `name[sub,sub]` legend and its
prompt rendering were removed.

## Rejected alternatives and what was left out

- Reading a trailing slash, or a stat, as directory scope (the earlier design).
- Bare strings as paths, a shared type for selectors and places (a static error
  becomes a decoder error; a selector in `fs` is a planting vector).
- Refusing repeats in a list; a `Net::Managed` now; verbs as a record of
  optional booleans; a verb veto surviving extension.
- Contextual record inference for nested tables; a second grant schema or
  profile checker; unifying a nested table against a record type.
- A compatibility parser, a fallback of old restrictions to `Allow`, a hidden
  activation marker to keep neutral layers hardened.
- Reading `P` after host folding (misclassifies `C:/../x`); counting `\x` as
  absolute on raw input.
- Out of scope, still: the run-time `~`/`xdg:` prefixes on ordinary paths
  (`path::sigil::expand_path_prefix`; after this, `xdg:` survives only there,
  and whether ordinary paths keep it is a separate question); package-manager
  selectors beyond `homebrew`.

## What the Belnap decision changed

The plan kept, deliberately and explicitly, every exec and fs judgment, and
said nothing here entrenches them. The Belnap decision then changed each in one
place: lifetimes (fs prefixes had re-resolved on every check, exec `Path` and
`Dir` keys kept their frozen identity; now allowances are frozen and exclusions
re-resolve), what reading includes (the guard read the read list alone while
both kernels read every write prefix and run allowance), ral's own binary
(Landlock and Seatbelt admitted ral's image; the guard judged it as a host
file; now an ordinary file everywhere), `system`'s expansion (Homebrew folded
in; now `` `dir `homebrew ``), scope ranking and table composition, extension
(the `Option` lift reading an omitted base axis as empty), exec and fs
activation (presence-based; now `is_top`), `Verdict` (to `Evidence`), and
`Widen` (to `Join` with role types). What stands from this decision: the
surface, `Place`, `Selector`, `FreezeCtx::place`/`selector`, `ExecKey`,
`ExecGrant`'s private map and single insertion path, `Net`, the verb sets with
their membership, restriction, extension and activation, and the checker.

## Review history

Drafted against 0c52a8f4; revised at 6bfbb097 after three review rounds
(Claude, Fable, Codex on gpt-6-astra). Findings, in order:

1. First round: the plan was reshaped to lead into the Belnap plan (this one
   replaces the surface, decoder and checker; that one the algebra).
2. Second round: `dir` payloads take no parentheses (they group only in
   `$[…]`); every list is a set; `ExecGrant`'s map must become private (it was
   `pub` and written directly, so the one insertion path did not exist); nested
   checks drain only in `typecheck`; the drift test compared the prefixed
   refusal to the unprefixed message and passed vacuously; state which
   judgments the Belnap plan changes. Decided with Alex in this round:
   selectors only under `dir`, Windows-absolute paths as dead grants on POSIX,
   nested tables as records only.
3. Third round: bump the engine protocol (a stale guest would pass the
   packaging check) and deny unknown fields on `Capabilities`; exarch's deny
   layer was the one production caller of the string freezer and moves to
   places; classify `P` on raw input, not after folding; make repeats
   idempotent rather than refused; name the diagnostic and summary changes
   (`RowExtraField` wording, `grant_summary`'s second activation predicate).

## Relations

- It supersedes the surface half of
  [[decisions/260727_exec-surface-fails-loudly|exec-surface-fails-loudly]]: the
  `dir` constructor replaces the trailing slash it kept, and the empty
  subcommand list has no home once subcommand lists are gone.
- The surface now mirrors the partition of
  [[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]:
  `name`, a place and `dir` are its `Name`, `Path` and `Dir` keys.
- The exec and fs algebra is the subject of
  [[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]].

## Where

`authority::grant::freeze` (`Place`, `Selector`, `FreezeCtx::place`, `FreezeCtx::selector`),
`authority::policy::lattice` (`Net`, `EditorVerb`, `ShellVerb`, `Flag`, `ExecGrant`),
`typecheck::contract` (`Holds`, `drain_nested`). See [[design/grant|grant]],
[[design/capability-freeze|capability-freeze]],
[[design/two-enforcers|two-enforcers]] and `docs/SPEC.md` §12.

**Amended 2026-10-22 by [[decisions/261021_one-path|one-path]].** `drop_dead_exec_grants` is the decoder's own
predicate: a bare exec name that finds neither a file on the decoding `PATH` nor
a tool this platform bundles makes no allowance and keeps its veto. The
decoder returns a resolved `GrantLayer`, not a `Capabilities`.
