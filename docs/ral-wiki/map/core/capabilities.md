---
generated_at_commit: f56fff48
generated_at_date: 2026-10-21
covers_paths: [core/src/capability/, core/src/capability.rs, core/src/guard/, core/src/guard.rs, core/src/sandbox/, core/src/sandbox.rs, core/src/path/, core/src/path.rs]
---

# Map: core / capabilities & sandbox

The [[design/grant|grant]] mechanism in two halves: an in-process decision layer
and an OS process sandbox that enforces it for external commands — each
authoritative exactly where the other is blind ([[design/two-enforcers|two
enforcers]]). Authority is attenuated by intersection — a `grant` block can only
narrow.

## The model — `core/src/capability/`

Authority as data, at rank 3: `Capabilities` (one layer), `GrantStack` (their
dynamic composition) and the folds over it, with no reader of its own. The
in-process guard (`core/src/guard/`) and the OS sandbox
(`core/src/sandbox/projection.rs`) are its two consumers, in the order
`capability < sandbox < guard`
([[decisions/261006_capability-is-data|capability-is-data]]). Every judgment
combines the layers' *evidence* ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]); there is no `Capabilities::meet`
([[decisions/260906_object-not-name|object-not-name]]). Why `Capabilities`,
the live judgment, and `SandboxProjection` are distinct and not one is argued in
[[design/capability-carriers|capability-carriers]].

Submodules:

- `lattice.rs` — the types and their algebra: the single always-frozen
  `Capabilities`, resolved by `guard::freeze` at decode
  ([[design/capability-freeze|freeze boundary]]); `FsPolicy`, `Net`
  (`Offline`, the one opinion a layer holds on the network), the verb sets
  `BTreeSet<EditorVerb>` and `BTreeSet<ShellVerb>` (each `Option`: absent
  inherits), `GrantStack`, `Meet`, `Join`, `Axis` (`join_top`, `is_top`),
  `Evidence { positive, negative }` and `Fragment`, the extension file's
  result, consumed only by `Capabilities::extend` (roles are types: a policy's
  omitted axis is `t`, a fragment's is `⊥`); and the authored exec grant
  `ExecGrant { every, rules }`, both fields private, built only by `Default`,
  `FromIterator<(ExecKey, Evidence)>`, `Join`, `join_top` and the wire
  decoder, every entry through `join_insert`, read through `every`, `iter` and
  `get` — `ExecKey::{Name, Path, Dir}`, bare keys, frozen path keys and frozen
  dir keys; `every` is a flag only `join_top` sets and has no negative form;
  `Display` spells a key as a grant does (a dir as `` `dir ``). It rides the
  wire (as a pair sequence, `{"every", "rules"}`) and is never matched
  directly; `ExecRules` is what judges.
  `Flag` (`Editor(EditorVerb)`, `Shell(ShellVerb)`) and
  `GrantStack::permits(Flag)` are the one verb vocabulary: membership, a meet
  over the layers, silence permitting. `is_restrictive` is the denotational
  activation predicate, each axis through `is_top`: exec unless `every` with no
  exclusion, fs unless it writes the root and excludes nothing, `Net` when
  `Offline`, a verb set unless it lists every verb;
- `table.rs` — `Table<K: Scope>`, authority as a table of scoped evidence
  ([[design/authority-tables|authority-tables]]): `Scope` (`holds::<P>`,
  `own`; no rank), `supports`, `excludes`, `evidence`, `live`, `denies`,
  `withheld` (a `Withheld`: `MissingAllowance`, or `Excluded` citing an
  exclusion that applies, chosen deterministically), and `Meet`/`Join for
  Table`, denies joining and allows meeting at their own subjects; the meet's
  obligations and the generic law test are its property tests;
- `fs.rs` — the fs instance: `FsOp`, `FsScope { Prefix(FrozenPath),
  Place(RealPath) }` (a run place reads as a prefix), both regions
  `Table<FsScope>`, and `region(stack, resolver, op, places)`, each supplying
  layer's read and write prefixes (writing reads) and, for `places`, its run
  places `P≤L`, with the floor joined; allowances stay as frozen and only deny
  paths are re-frozen against the `Resolver`. `GrantStack::read` is the read
  judgment: `region(…, false)`, then `region(…, true)` only on a missing
  allowance;
- `exec.rs` — the exec instance
  ([[decisions/261004_exec-rules|exec-rules]]) and its judgment: `Program`
  (`Tool` or `File { path, real }`) and the `Subject` it is judged as (`Tool`,
  `File`, `Under`, `Any`); `ExecScope` (`Any`, `Dir`, `File`, `Tool`, `Name`),
  `Name` negative only and `Any` positive only by construction; `ExecRules =
  Table<ExecScope>`; `ExecRules::compile`, one layer's `ExecGrant` against this
  host, a bare key resolved on the host `PATH` (live per run), a path key's allowance
  at the real path frozen at decode and its exclusion at the real path its
  surface reaches now (`FrozenPath::live_real`); `rules`, the stack's table, compiled
  per question; `places`, the run places (positive `File` and `Dir` real
  paths). `GrantStack::admit(program, args, resolver)` is the one exec
  judgment, the table then the read judgment and the only mint of `Admitted` (the program, its argv and the table
  that judged them, private fields, all a launch reads); a refusal is data,
  `Refused { program, args, why: ExecDenial }`, `ExecDenial::{Run(Withheld<ExecScope>),
  Read(Withheld<FsScope>)}`; `admits(&Program)` is head-only admission.
  `ExecRules::for_kernel(support)` is the table less bundled-tool scopes, joined
  with the live support as `File` allows, what the kernel is handed;
  `allowed_files` the live file scopes the carriers are computed over. Vetoes key on
  `path::command_name_key`;
- `deputy.rs` — `deputy_prefixes`, the confused-deputy report: the prefixes a
  grant makes both `exec`-admitted and `fs`-writable, the live part of
  `R ∧ W`, the exec region (each exec layer's `Dir` keys with full evidence,
  the root for `every`) met with `fs::region(FsOp::Write)`, so each writable
  island under a runnable directory is named and a directory wholly excluded
  on either side is not. **What it locates is where a write becomes runnable, not an
  escalation**: within one projection the dropped binary is spawned under the
  confinement that wrote it, and only a runner outside the projection turns
  the shape into an escape — so it reports and never denies. Findings are recorded only into an open audit trail, as a layer is pushed
  (`ral --audit --capabilities <profile> -c true`; [[design/grant|grant]]).

The `load` module (rank 10), not this one, holds `load/profile.rs` —
`load_capabilities_from_path` / `_from_str` for `.ral` capability profiles,
compiled under `grant`'s own declared table (`typecheck::contract`,
`Form::Grant`): a profile's dimensions *are* the form's options, so a returned
row is held to them statically and the decoder below is where a profile
returning a map meets the same six keys.

## The guard — `core/src/guard/`

The model's in-process reader, at rank 7: every runtime yes/no over the dynamic
capability stack is a free `guard::check_*(&Context, …)` function that folds
the whole stack (`ctx.grants`) — a module boundary rather than a typestate
([[decisions/260605_witness-collapse|witness-collapse]]) — and the one place a
grant string becomes a frozen path.

- `enforce.rs` — the guards over the model's judgments: `check_exec`
  (`GrantStack::admit` and the wording of its refusal), `check_fs_exact` (the
  object `Shell::locate` walked to), `check_flag`, `check_device_name`, and
  `deny_head`, the refusal of a head before its argv is known. The exec and fs
  checks return `Result<_, Denial>`, a `Denial { check, error }`, over
  `FsRefusal::{Guarded, Withheld}`: the fact the
  trail keeps and the error the caller raises. The fs guard is split so the
  judgment is reusable without the report: the fs judgment is the decision —
  `FsRefusal::Guarded` first, for a write onto a boot-pinned sandbox binary
  (`sandbox::pinned_binary`, by inode), before any grant is folded — and
  `check_fs_exact` words it. `GrantStack::admits_fs_exact` is the quiet twin a
  write asks before reading its before- and after-images;
- `shell.rs` — the guard's `impl Shell`: `check(Flag, who)`, `check_exec`,
  `check_fs_read` (a read by name, canonicalised leniently, excusing the
  discard device `LexicalPath::is_discard` and refusing a reserved device name
  before either region is consulted), `locate`, `locate_existing`,
  `locate_if_admitted`, `sandbox_projection`, and the grant scope guards
  (`with_capabilities` = `with_layers(GrantStack::of(c), f)`,
  `push_session_capabilities`, `audit_deputy_prefixes`,
  `has_active_capabilities`). `Shell::locate` is the door every open takes:
  the same name refusal, `path::walk` to the object, then `check_fs_exact` on
  `Located::real`. `Shell::refused` mints a `Denial`'s `Break` and records its check
  through `Shell::record_check` with no `Mooring`, so on the trail alone;
- `freeze.rs` — the one place a grant's `Place` or `Selector` becomes a
  `FrozenPath`, against one anchor: `FreezeCtx` (`FreezeCtx::of(&Shell)`, an owned
  `EnvVars` and `CwdPath`: the `Resolver` pair, `FreezeCtx::at` for a caller with no shell) and its `place` (`None` is a dead grant, an `absolute`
  payload classified on the raw string by `live_absolute` through
  `lex::grant_eligibility` (`EligibleOn` / `GrantRefusal`) over `lex::parse_bytes`:
  a `HeadKind::Device` or `HeadKind::Malformed` head, a mixed-separator opening
  (`/\x`, `\/x`, `GrantRefusal::MixedHead`) included, is refused first, though
  Windows reads it as absolute; a `\??\` NT object-manager head
  (`HeadKind::Driveless` with `Namespace::Object`, `GrantRefusal::NtObject`),
  read as Win32 reads it, is refused under every tag with its own refusal; then
  `HeadKind::Posix` is `EligibleOn::Posix`, a non-verbatim `//srv/sh` share is
  `EligibleOn::Both`, and a `HeadKind::Drive` or other `HeadKind::Unc`, a
  verbatim root included, is `EligibleOn::Windows`, dead under POSIX) and
  `selector`; `host_absolute`, a path the host holds classified on its
  native text and frozen as an `absolute` grant, refused with
  `GrantRefusal::ForeignOrRelative` where a payload would be dead, so exarch's
  deny layer fails closed; the tags `absolute`,
  `cwd`, `home`, `tempdir`, `gitdir` and `xdg-*`, `PolicyError`, the `gitdir`
  refusals (`From<BadPointer>`), `Selector` (`Path | Toolchain(Toolchain)`) and
  the `path` selector (`path_dirs`, over the pure `path_dirs_in`), and
  `refuse_the_world`, the guard both it and the toolchain overrides call;
  `FrozenPath` freezing refuses a real path that is not Unicode;
- `toolchains.rs` — every other directory selector as one table: `enum
  Toolchain` (`system` and the package managers), each a few `Location`s of
  overrides and defaults, from one exhaustive `match`; the pure `expand_in`
  over an injected environment, home and `exists`, answering native
  `PathBuf`s (shown only in a refusal), its live wrapper `expand`
  (what `FreezeCtx::selector` calls), and
  `FLOOR`, the four system-wide prefixes (`homebrew`, `macports`, `nix`,
  `pkgsrc`) that `fs::floor` reads through `floor` and keeps only outside the
  process's home ([[design/grant|grant]] §Selectors states the rules);
- `decode.rs` — `decode_capability_map`, which walks a `grant [...]` /
  `--capabilities` `Value` map into a frozen `Capabilities`, one dimension
  decoder per `exec` / `fs` / `net` / `editor` / `shell` key, nested tables
  and verb lists read as sets —
  every one of them authority, none of them a recording switch
  ([[design/audit|audit]]); its exec targets freeze to `ExecKey::{Name, Path, Dir}` (a
  `dir` selector expanding to one `Dir` per directory), collected through
  `ExecGrant::from_iter`;
- `grant.rs` — `SpawnGrant` (`Inherit`, `Base`, `Restrict`), a spawn's grant
  as it crosses to a child that has not yet resolved it, frozen against the
  child's own `FreezeCtx`.

Path resolution for grant matching is `core/src/path/`: a fixed staged rule,
plus `which.rs` for PATH search.

- expand — `sigil.rs` (run-time `~` and `xdg:` expansion of ordinary paths,
  never in a grant; the places `cwd`, `tempdir`, `gitdir` are policy-only,
  expanded at freeze by `guard::freeze`; `git.rs` backs `gitdir` discovery, following a `.git` pointer file only to a
  git directory whose `gitdir` back-pointer or `core.worktree` names the working
  tree back; a bad pointer is a path-level `BadPointer`, phrased as a grant error by the caller), `tilde.rs` (`~` and `~/sub` only; an unset `HOME` is `None`,
  and each caller picks its own honest answer; `TildePath::shown` is the one
  spelling `~`, `home` and a `~/…` command head share, and `TildePath::expand`
  the path such a head runs). The expansion `expand_path_prefix` answers is
  native, in the host's own spelling, for the resolver to hand on to the OS.
  `sigil::SigilHead` is the one reading of a path's head — `Tilde`, `Xdg`, or
  `UnknownXdg` (an `xdg:` no kind answers to, passed through literally) — that
  expansion, `literal_relative` and a grant's refusal of a sigil all ask. An
  `xdg:` sub-path naming a volume of its own under Windows rules — a drive
  component, or a share or namespace head opening it — would re-root the join, so `expand_path_prefix` leaves the whole path as written: it fails
  closed, a prefix matching nothing rather than a fabricated path, while a
  grant refuses the gap outright in `guard::freeze`;
- lex — `lex.rs`: one structural parser, `parse_bytes`, reading a byte slice under `PathRules` into `Parsed { raw, rules, kind: HeadKind, spelling: HeadSpelling, head_len }`, from which resolution, `.`/`..` folding and every head judgment project. `HeadKind` names the structural head (`Posix`, `Drive { letter, volume }`, `Unc { volume }`, `Driveless`, `DriveRelative { letter }`, `Relative`, `Device` for `\\.\`, and `Malformed` for a two-separator or `\\?\` head that is no well-formed drive, share or device); `HeadSpelling` records the opening separator and namespace facts once (`namespace: Option<Namespace>` — `Verbatim { exact }` for `\\?\`, `Device { exact }` for `\\.\`, `Object` for `\??\`, with `exact` / `Namespace::is_exact` true when all three separator bytes of that 4-byte prefix are `\` — `mixed_opening` for `/\` or `\/`, and `opens_with_slash`). `fold_dots_under` (over `fold_bytes`) is one kernel under either rules from any host, `fold_dots` its host instance on a held path: `head_len` marks where the head ends and only the names past it fold, joined with the rules' separator, so a fold preserves `HeadKind`, `volume` and (save when `..` climbs into the `??` component of a `\??\` driveless path) `HeadSpelling::namespace`; under Windows rules a non-namespace `Drive`, `Unc`, `Driveless`, `DriveRelative` or `Relative` head (`Parsed::respell_head`) is respelled with `\` too (`C:/a/b` folds to `C:\a\b`, `//srv/sh` to `\\srv\sh`), so such a path has one separator throughout, while every head whose kind or meaning turns on its spelling is kept as written: a `Posix` `/` (as `\` it would be `Driveless`), a slash-spelled verbatim head (`//?/C:/x` is a device path Win32 normalises, so its tail past `head_len` folds), and a `Device` or `Malformed` path, or an exact `\\?\` verbatim path (`Namespace::is_exact`, `head_len == raw.len()`: Win32 hands its rest to the kernel uninterpreted, `/` a name byte and `.`/`..` names), all head and kept whole, `..` included; a relative path's drive-shaped first name keeps a `.\` before it; `is_unparsed_head`, a Windows path absolute to the kernel in which `std::path` finds no prefix and so would descend or climb from the current drive, which the walk and the lenient canonicaliser's ancestor climb refuse. Every head predicate projects from `parse_bytes`: whether a path is absolute under either rules set (`absolute_under` / `Parsed::is_absolute`: POSIX, `HeadKind::Posix`; Windows, `Drive`, `Unc`, `Device` or `Malformed`) and on this host (`is_absolute`, `absolute_under` the host's rules, the one absoluteness ral asks, `anchor` included; `Path::is_absolute` is banned in clippy.toml in its favour); `is_working_directory` (`Posix`, or `Drive` / `Unc` under Windows); `is_bare_root` (one separator alone under the given `PathRules`); `has_drive` (non-namespace `Drive` or `DriveRelative`); `Parsed::is_mixed_head` and `Parsed::is_nt_object`, shared by `grant_eligibility` and `classify_subpath` (`SubpathRefusal`), which `guard::freeze` maps to its messages; `is_network` (`HeadKind::Unc`), `is_verbatim_drive` (`Namespace::Verbatim` + `HeadKind::Drive`) and `is_volume_guid` (`has_os_namespace_head` with a `Volume{` `namespace_tail`), which the Windows DACL backend asks rather than parsing prefixes itself; `Parsed::has_os_namespace_head` (`Namespace::is_exact`), what `show` leaves unfolded; `Parsed::without_verbatim_prefix`, what the Windows identity fold slices; and `Parsed::namespace_tail`, what the discard device check reads under `HeadKind::Device`; `utf8`, a path's native spelling as text or `NotUtf8` naming it, the one door for a path that must travel as a string (`--capabilities` profiles, `SourceFile`); a final component's `extension`, and whether a component is a Windows drive or stream (`is_stream_or_drive`); `relative_suffix` strips a sub-path's leading separators under the rules, and `join_under` joins a sub-path onto a base string one component at a time
  with the separator of the given rules, on every host alike (`GuestPath::join`
  is it under Posix rules), and refuses (`Rerooting`) a sub-path naming a volume
  of its own under Windows rules — absolute there, or a drive at any component;
  `join_relative` is it under the host's rules onto a host-held path, and `join_slash` is that
  for a `/`-separated constant subpath the program spells, panicking if one
  re-roots; `identity.rs`: name identity and containment, `windows_collision_key` naming a kernel object by an absolute path under `collision_key` per component, coarser than NTFS's own fold so a lock merges every spelling NTFS equates; `device.rs`: device names, and `win32_trimmed`, a final component with the trailing dots and blanks Win32 drops, the one copy of that rule (reserved device names, batch images);
- complete — `complete.rs`, the path half of completing a typed word:
  `PartialPath` splits it into the directory to list and the needle, says
  which entries are offered, and spells a chosen one back as ral shows a path
  (typed `~` kept, `/`-separated; expanded via `TildePath::shown` only when the
  spelling must be quoted);
- slug — `slug.rs`, `project_slug`: a launch cwd as one directory-name
  component (`/Users/x/proj` → `-Users-x-proj`), for keying per-project state;
- canonicalise — `canon.rs`, for a read by name;
- locate — `walk.rs`, for an open: `walk` descends from the root through
  directory handles with no symlink followed by the kernel, splicing each
  link into the name and re-walking, and hands back a `Located` — the
  directory handle, the leaf, and the symlink-free `real` path — whose every
  operation is handle-relative with `FollowSymlinks::No` (`cap-primitives`
  supplies the `*at` calls on Unix and Windows). A `..` is physical, the
  kernel's rule, even from a link target (the user's own path keeps its
  logical fold); names are spelled as the disk stores them on macOS
  (`getattrlistat`), so case-variant names meet their deny; the splice count
  is bounded by `MAX_HOPS`; every restart, the given name and each splice
  alike, is put to one `rooted` check, so a link target relative to a
  drive's current directory or with an unparsed head is refused as the name
  itself would be; `walk_vetted` is the same walk with a per-object
  check, which `carriers::trusted_real` runs;
- match — `identity::path_within`, the form-blind containment kernel, which takes
  an `Identity` and folds it over the alias pairs. `Stored` is bytes, or under
  Windows `windows_stored_components`, preserving stored case and unifying `/` vs
  `\`, and `\\?\`-verbatim spellings; `Collision` compares components by
  `identity::collision_key`, compatibility caseless matching of the uppercase
  image (ICU4X), the coarsest identity any filesystem gives a name. A rule's
  `Polarity` (`identity::Allow`, `identity::Deny`) picks `Stored` or `Collision`
  ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]);
  on Windows `walk::dealias` names every leaf by its stored case and long name,
  using the parent's stored name for an absent leaf, and refuses one it cannot name;
- name a device — `device::is_discard_device`, behind
  `LexicalPath::is_discard`: `/dev/null`, or on Windows exactly the
  device-namespace spelling `\\.\NUL` (either slash, any case), and no other
  path. Every other Windows path whose last component is a DOS reserved device
  name — `NUL`, `CON`, `PRN`, `AUX`, `COM1`–`COM9`, `LPT1`–`LPT9`, `COM¹`–`COM³`,
  `LPT¹`–`LPT³`, any case,
  any extension, trailing dots and blanks trimmed — is *refused*, not
  excused: `device::reserved_device_refusal` phrases the error (for `NUL`, as the
  question "did you mean `\\.\NUL`?"), and `guard::check_device_name`
  asks it at the three fs doors, `check_fs_read`, `Shell::locate` and
  `Shell::locate_existing`, ahead of any grant. A resolved path keeps its last
  component, so the check needs no raw spelling. Like the identity rules above
  both take `windows` as a parameter rather than reading `cfg!`, so neither
  table is dark on the other host.

`resolver.rs` composes the stages — `Resolver::resolve` and
`Resolver::anchor` are the *sole* constructors of a `LexicalPath`
(`forms.rs`, with its grant-side twin `FrozenPath`), so canonicalisation
cannot run before lexing: the ordering is in the types, not convention.
`resolve` takes surface text and expands sigils first; `anchor` takes a path
the host already holds, a walked entry say, and expands nothing, so a file
named `~` stays that file and a non-UTF-8 name is never renamed by a string
round trip.
A `Resolver` is an `EnvVars` and a `CwdPath`: `~` reads `env.home()`, `xdg:`
the `XDG_*_HOME` of `env` through `get_or_host`, so a `within [env: …]` overlay
reaches both. `Resolver::at` builds one for a caller with no shell to mint one; `resolve_in_script` is
`resolve` for a path written in a ral script, which reads it beside itself:
it is `resolve` itself with the script's directory for the cwd, so the two
doors agree: sigils expand first, only a still-relative result joins the
script's directory, and an absolute one, a malformed head included, is kept
as `resolve` keeps it, never lent the directory's drive. The directory is
the native parent of the file's `SourceOrigin`, never a re-parsed rendering
of it, anchored as `anchor` takes it, so a script under a directory named
`~` stays there. A virtual source has no directory,
and its paths resolve against the cwd.

(`which::ral_path_entries` lists `RAL_PATH`; `use` searches it through the
shell's resolver, and the plugin loader folds it into its own candidates.)

`tree/walk.rs` is the one directory-tree walk, `Walk`, and the one
`ignore::WalkBuilder::build` site; `tree/glob.rs` is glob's pattern language,
enumerated by it. A `Walk` is told what it may enter (`tracked`: hidden and
ignored entries skipped; `everything`; a depth; a `keeping` predicate; link
following), polls the cancel scope at each entry, and skips every walk error
but an unfollowable link, which is still an entry. `sightings` yields entries
judged by no grant, for host-side walks (the sandbox's linker discovery).
`admitted` judges each entry by its own path, anchored with `Resolver::anchor`
and admitted silently with `admits_fs_exact`, and names it in the shape its
root was written (`Resolver::is_relative`): a cwd-relative root gives
cwd-relative names where it can, any other absolute ones. A name reads back as
itself: one a sigil would claim is spelled `./~/x` (`sigil::literal_relative`),
and one no ral string can hold is refused when asked for.
`glob` and exarch's `grep-files`, `grep-hidden`, `find-files` and
`explore-dir` all walk through it.

`which.rs` is the one `PATH` walk. **Its anchor is an `Option<&CwdPath>`: the
shell's cwd cell, or `None` for a shell-less caller — and `CwdPath` has one
door, so no call site can choose "here" for itself**
([[decisions/260731_one-walk-one-anchor|one-walk-one-anchor]]).

- `names_a_path` is the one answer to "is this command name a path?": it
  bears a separator under the host's `PathRules` (`PathRules::is_separator`:
  `/` everywhere, `\` under Windows alone), so off Windows `a\b` is a bare
  name for `PATH`. `names_a_path_for` takes the rules, and the lexer's
  `Word::Path`, `ir::CommandName::of`, the walk itself and the grant decoder's
  `` `name `` check all ask it.
- `path_dirs` is the sole directory list behind `locate`, `commands_on_path` and
  `search`; it **drops empty `PATH` elements on every platform**, so a trailing
  `;` or `:` never means the cwd. `.` and `./bin` are honoured, as written.
  `locate_in` is the same walk off Windows, unmemoised, over directories a
  caller already holds (`path_dirs_absolute` for bwrap's pin), so no list is
  re-joined into a string to be split again.
- `search` returns `PathSearch::{Executable, FoundNotExecutable, Missing}` from
  one traversal — the executable half memoised, the presence half not — so
  `runtime::command::vet` reads a verdict rather than taking a second walk.
- On Windows, `%PATHEXT%` suffixes **append**: `build.ps1` yields
  `build.ps1`, `build.ps1.EXE`, …, never `build.exe`.
- `ExecRules::compile` walks the process's own `PATH` with
  no anchor, so neither a scoped `PATH` nor a relative entry can
  redirect a bare key.

A `FrozenPath` (`forms.rs`) carries its `surface` form (lexical — what
the author wrote, kept for display) and its `real` form (symlinks
followed), both fixed by one disk consultation at the freeze door; a guest
prefix is minted by `from_guest` from a `GuestPath` (`namespace.rs`) under the
POSIX fold, and `refreeze`
re-resolves an exclusion against a live `Resolver` (an allowance is never
re-frozen), the bare root minted rather than frozen.
[[invariants/grants-judge-objects|Every authority is judged on `real`]]:
`contains::<P>` for fs, `FrozenPath::real` for the exec table,
— and no other door: `identity::path_within` and its string twin are `pub(super)`,
so the form-blind kernel does not leave `core/src/path/`, `surface_path` is
private to `forms.rs`, and outside the module the surface leaves the type
only as a *string* (`as_str`, `into_string`) for rendering. That is enforced
rather than documented because the `xdg-*` freeze guard once chose the form for
itself — asking on the surface while the check it guarded matched the real
form — and read a symlink out of `HOME` as contained.

The set-level algebra over prefixes is not in `path/`: it is
`capability::table`, which names polarities and never identities, so every
spelling question stays here. `forms.rs` also holds `RealPath`, the exec subject and the identity of a run place,
ordered as the host identifies files; `FrozenPath::real` for an allowance,
`FrozenPath::live_real` for an exclusion.

XDG base directories resolve through one resolver, `path/basedir.rs`
(`XdgKind`, the pure `resolve_xdg_in`; `host::xdg` reads the host env, the `xdg:` sigil and the freeze an `EnvVars`; `app_dir` and `app_dir_or_temp` spell an application's own `$XDG_<kind>_HOME/<app>/` for ral and exarch alike): an absolute `XDG_*_HOME` override else the
home-joined Linux default on every platform, and `None` where there is neither
— a host fact answers with an `Option`, so each caller picks its own fallback
rather than inheriting a fabricated home (state that must land somewhere —
exarch's directories, ral's crash log, the Windows sandbox ledger — takes the
temp dir through `app_dir_or_temp`, keeping itself out of the cwd). Both the `xdg-*` grant places
(`sigil.rs`, `guard::freeze`) and the host's own config/data loaders (`host::xdg`) defer to
it, so a grant and the rc/history/plugin paths can never name different
directories — [[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]].

## OS sandbox — `core/src/sandbox/`

External commands inside a `grant` block run under an OS sandbox enforcing the
declared **filesystem and network** capabilities — and **exec**, wherever a
backend carries the allow-list into the kernel, so a grant whose only opinion
is `exec` engages the sandbox too (`sandbox::EXEC_ENFORCED`, the one statement
of that fact, read by `SandboxProjection::of`). Exec is
guarded in-process on every platform (`guard::check_exec`) before the
spawn; both Unix
backends additionally render the same rules into the kernel, catching the
re-execs the in-process guard never sees (`sh -c`, `find -exec`) — macOS as
Seatbelt `process-exec` forms, positives then negatives, Linux as a Landlock `Execute`
ruleset the payload enters inside the bwrap envelope over admits the parent
opened in the host (`linux/landlock.rs`;
Landlock being allow-list only, a deny *inside* an allowed directory is
subtracted from it, entry by entry over the tree at launch; `Any` lowered against
the bind plan, which is computed before the ruleset). `carriers.rs` computes the
live support the kernel must admit beside the table for a script to start,
under any confined launch whose fs or exec is restricted: system-vouched `#!`
interpreters and macOS shims, by `trusted_real` — regular files only, judged
against the effective uid — never `env`'s target
([[decisions/261004_exec-carriers|exec-carriers]]). The AppContainer on
Windows has no path-exec filter, so there the in-process guard stands alone (the deny-by-default fs projection still bounds
which images a child can *read*, and so load, at all). Exec-deny ACEs and
directory execute/traverse semantics remain unimplemented. Native Windows
verification of stored-case walks, verbatim image launch, and launch refusals
is outstanding; cross-compilation alone does not establish them.

Inside a *guest* — a VM whose engine runs under `ral-daemon`, signalled by
`RAL_GUEST`, which carries the jail's cgroup root — the per-command OS backend is not engaged at all: every spawn
is already confined by the [[map/core/io-process|spawn jail]] (a fresh
unprivileged uid and a per-exec cgroup), the daemon disables the
unprivileged user namespaces bwrap needs, and the guest has no network
device for `net` to govern; the in-process guards apply unchanged
(`docs/SPEC.md` §12.11).

- `pin()` and `boot()` — startup: `pin` pins ral's own executable (`reexec::OWN`) and, on Linux, the bwrap
  envelope (`linux::pin_envelope`, walked on the process's own `PATH`
  before any shell exists); `boot` is `pin` and, on Windows, the boot-time orphan sweep
  (`windows::session::boot_recover`) that deletes a crashed prior session's
  AppContainer profiles, matching their ledger owner by PID, image and creation
  time. A test binary is
  the same [[invariants/single-binary|multicall executable]] a confined child
  re-execs, so it must serve these flags from its own pre-`main` `#[ctor]` (it
  reaches `main` only through libtest). `invocation::serve(&role, installers)`
  (`core/src/invocation.rs`) is the one pre-`main` dispatch, over the role
  `core/src/role.rs`'s `classify` named — run by `ral`, exarch and every
  test `#[ctor]` alike, surfacing a served role's exit code so the caller can
  terminate. It serves a `Warrant` role first (`serve_warrant`), before any
  pin, so the trampoline pins and opens nothing before it is confined; the
  pipeline anchor spawns nothing and runs unpinned. Every
  other role — the engine, the shell — runs `boot` first, and the
  `BundledTool` multicall only `pin`s, so an `--engine` process has bwrap
  pinned exactly as the shell does. Skip it and `OWN` stays unpinned, so the
  per-command launcher refuses to re-exec a binary it cannot vouch for.
- `reexec.rs` — `Pinned`, an executable pinned at boot: the one shape every
  `Command` the sandbox execs is built from, so nothing a session does to its
  environment can choose the file. Two are pinned: this executable
  (`OWN`), so a confined re-exec runs the same binary even under an
  on-disk swap, and on Linux the bwrap envelope (`linux::ENVELOPE`). Its
  methods are `arg0` (the on-disk path, `argv[0]` of every exec), `exec_path`
  (what `execve` is handed), on Linux `fd` (the pinned descriptor) and `names`
  (the inode's current name, `None` once unlinked), and on macOS `verify`;
  `reexec::launch(role)` is the one launch door for the unconfined
  re-execs (the pipeline anchor, a bundled tool), through the pin when there is
  one, and `reexec::own()` is ral's own pin as a
  `Result`, so a sandboxed launch refuses when ral could not pin itself. The `Pin`
  variants say where a swap is even askable: `Fd` on Linux (the retained
  descriptor, so `/proc/self/fd/N` resolves to the boot inode), `Stat` on
  macOS (a `(dev, ino)` snapshot re-checked before each spawn), and
  `Unguarded` on Windows, which has no parent-side self re-exec for a guard
  to protect. On Windows there is no child re-entry at all — confinement is
  the AppContainer token the *parent* attaches at `CreateProcessW`, so a child
  asking to confine itself (`--warrant`) is refused with 126, and the pinned
  self serves to grant the container read on the bundled-tool re-exec image.
  `Pinned::verify`, the parent-side swap guard, is macOS's alone, which execs
  the trampoline by name; Linux execs it from ral's pin lent at
  `Slot::Trampoline`, so there is no name to verify.
- `projection.rs` — the sandbox's own view of authority:
  `SandboxProjection::of(&GrantStack, &Resolver, Option<&Admitted>)`
  meet-folds the stack once, at spawn, into `FsProjection` (`read:
  Vec<FsScope>`, the live positives of the read region with run places and live
  support among them, beside the write prefixes, the floor and the exclusions;
  allowances stay frozen up to rendering, exclusions are re-resolved through
  `render_paths`; `FsRules`, whose `rendered()` derives `pinned_dirs`; no write
  reach classification) and `ExecProjection::Restricted(ExecRules)`,
  `ExecRules::for_kernel` of the `Admitted`'s own table joined with the live
  support (`sandbox::carriers`), so the profile renders the table that judged
  the launch rather than a second compile. It takes no `Shell`;
  `Shell::sandbox_projection` (`guard/shell.rs`) is the door.
- `projection_enforceable` (`sandbox.rs`) — rejects an offline (`` net: `offline ``)
  projection on a backend with no kernel network enforcement, so an unenforceable
  request fails closed rather than running ignored.
- `Refusal` (`sandbox.rs`) — why no confined child exists: nothing ran.
  `Unavailable` is a confinement this host cannot establish, whether `projection_enforceable` saw it coming,
  there was no bwrap on `PATH` to pin at boot (`linux::envelope`, asked at the
  first launch that needs it), or the pinned envelope failed to spawn.
- `Launch::new` (`process/launch.rs`) builds the unsandboxed external exec
  image; `build_launch` reaches it directly when no projection is active.
- `launch.rs` (`sandboxed_command`) — the per-command launcher. `build_launch`
  (`runtime/command/process.rs`) routes an external or bundled child through here
  whenever a projection is active and the process is not already confined,
  confining that *one* child, the `Admitted`'s program: a `Program::File`
  by the real path the guard judged, its spelling as `argv[0]`, or a
  `Program::Tool`. macOS
  and Linux share one trampoline: the pinned ral itself, whose whole argv is
  `ral --warrant` (`Role::Warrant`). The confinement to enter and the program to
  run cross in a `Warrant` (`sandbox/warrant.rs`, its fields private) compiled in
  the parent. The confinement is the `Confinement` trait — `spell`, `parse`,
  `enter` — implemented on macOS by `Seatbelt(String)`, the profile
  `macos::build_profile` compiled, and on Linux by `Option<Landlocked>`, spelled
  `unconfined`, `landlock` or `landlock+refer`;
  the program is a `Run`, `File(path, args)` or `Tool(tool, args)`.
  `Warrant::confine` is the only way to the program: receive and verify the
  warrant, enter the confinement (`macos::apply_profile`, `Landlocked::enter`),
  close the handoff range, and yield a `Confined`, whose `run` `execve`s the
  host program or runs the bundled tool in-process (`uutils::run`, shared with
  the `--ral-bundled-tool` multicall). `serve_warrant` serves the child: a
  failure before the program starts exits 126, and an `execve` refusal takes its
  code from `SpawnFailure` (`From<&io::Error>`) — 126, or 127 for a missing
  program.
  The encoding is NUL-terminated fields behind a `ral-warrant/2` magic — a
  `file` is its real path, then its `argv[0]` spelling, then the exclusion covering its script's interpreter (empty if none), then its argv; a `tool`
  its name, then its argv — canonical (decode re-encodes and compares) and at
  most 8 MiB. It rides fd 99,
  never argv: a sealed memfd on Linux, checked again after sealing, and on macOS
  a prefilled socketpair whose writer closes before the child exists; the child
  refuses an unsealed memfd or a non-socket. The fixed descriptor layout is
  defined once as `Slot` in `warrant.rs`: 98 bwrap `--args`, 99 the warrant,
  100 `--info-fd` (a socketpair, which unlike a pipe cannot be reopened through
  `/proc`), 101 the Landlock ruleset, 102 ral's pin, the trampoline bwrap
  execs, 103.. the seccomp programs (sealed
  memfds), 107.. one mount handle per bind, open-ended and last. One `Handoff` lifts every source above every target and a
  single `pre_exec` `dup2`s each home; the child sweeps everything from 98 up
  with `close_range` once confined, and `Landlocked::enter` consumes the ruleset. `bwrap_command` refuses to launch if
  bwrap's pin fd sits on one of these slots: the `dup2` would close it before
  the exec of `/proc/self/fd/<N>`, which would then run whatever landed there.
  macOS spawns the trampoline directly; Linux spawns it under `bwrap`
  (`bwrap_command`), giving the full shape **bwrap → ral trampoline
  → Landlock → `execve`**. The real argv is `bwrap --args 98 --
  /proc/self/fd/102 --warrant`: `bwrap_argv` is pure in descriptors and returns bwrap's options alone, which
  bwrap takes from `--args`. The order is forced rather than chosen: a Landlock
  domain handling any fs right forbids `mount(2)`, bwrap's first act, so the
  layer can only be entered *inside* the envelope bwrap has already built.
  Linux's confinement names no path: the parent opens every admit by handle,
  never through a symlink, so each rule reaches the file a grant froze.
  `landlock::build` plans the
  handled set from the parent's one probe (`plan`), creates the `Ruleset` by raw
  syscall, and admits each allowed file as
  itself and each live allowed directory as its hierarchy less what the table
  blocks beneath it (`expand`, which stops where a bind mount loops back onto
  a directory it is listing), each opened in the host by `open_real`
  (`RESOLVE_NO_SYMLINKS`); an absent admit is dropped, a file admit whose path
  has since become a directory is dropped, not admitted as a hierarchy, a
  symlink since planted refuses the launch, and a restricting exec projection
  is refused where Landlock is unavailable. The payload probes nothing: it takes
  the ruleset at its slot, adds only `Refer` on its own root, which exists
  nowhere else, and fails closed if a promised ruleset never arrived.
  `bwrap_command` takes the `Envelope` (bwrap's pin, the trampoline's, and
  the `HostEnvelope` probed once by running that trampoline as a pipeline
  anchor), the image (`Program::File`'s real path, the file the trampoline
  execs), the handoff so far and the rendered projection, and refuses
  outright under a bwrap that builds no envelope, in bwrap's own words, or one
  without `--ro-bind-fd` (`HostEnvelope::builds`). `Binds::open` is the one
  place a launch opens what it mounts: the read-only defaults, the read and
  write prefixes, the image, each allowed file and live allowed directory of
  the `ExecRules` of the `ExecProjection`, and the cgroup tree in the `Over` layer, each object
  (`render_objects`: a path's canonical spelling, its other spellings mounted
  too unless within another bind) opened once by `open_real` and lent per
  bind at `Slot::Mount(i)`. A read-only bind within another bind is dropped, a
  writable one only within a writable one; an absent object drops its binds,
  and so does a system default this user may not reach, while a name now
  reaching outside what was rendered refuses the launch. Every allowance is
  bound as its frozen real path, a redirected one refusing the launch, and each
  deny mask goes on wherever a `Shown` bind shows the denied path
  (`Binds::names`): a mount laid inside one bind of an object does not show
  at another, so a prefix written through a symlink, two binds of one
  handle, is masked at both. A deny hiding `/proc` is refused,
  the envelope starting its trampoline through it. The two pins join the `Over` layer by copies of their own
  descriptors, read-only at the names their inodes hold now (`pinned`, over
  `Pinned::names`), an unlinked pin binding nothing. `bwrap_argv` emits
  `--ro-bind-fd`/`--bind-fd` from the same vector, `Shown`,
  before `/proc` and `/dev`, `Over` after. `--chdir` refuses
  non-UTF-8, and so does every rendered name, rather than go lossy into
  bwrap's argv. Windows builds the
  target's `Launch` directly and `windows::session::confine` attaches its
  projection's AppContainer LowBox `SECURITY_CAPABILITIES`, so the parent's own
  spawn is the confinement point — never a re-exec child; a bundled tool there
  is a plain `ral --ral-bundled-tool <tool>` carrying no warrant, and
  `serve_warrant` refuses `--warrant` outright. The launcher also takes an
  `Ownership` (`Kept` / `Surrendered`, the second variant `cfg(unix)` since only there does the verb that makes the
  distinction exist): it reaches the Linux backend alone, which decides the
  two ties between the session and the envelope — death (`--die-with-parent`)
  and address (`--info-fd`) — so a `detach`ed survivor keeps the birthing
  frame's projection, namespaces included, for life while dropping both
  ([[map/core/runtime|runtime]]). The grant body itself evaluates
  locally, external children being confined per-command
  ([[decisions/260617_sandbox-external-children|sandbox-external-children]]).
- Backends: `macos.rs` (Seatbelt, `macos-base.sbpl`; `Profile`, a record whose
  field order is the rule precedence (no freeze section), and in every profile ral's own file is
  write-denied and its ancestors unlink-denied), `process/limits/fork_brake.rs` (macOS's
  process budget: the confined child's `RLIMIT_NPROC`, the user's count at
  launch plus 512, bounded by the system ceiling and inherited hard limit;
  failed `setrlimit` refuses the launch, and `limits::failure_hint` explains a
  possible fork refusal on status 128 without asserting its cause), `linux.rs` (bwrap: the
  `Pinned` envelope, exec'd by descriptor and never by name, its own file
  read-only bound inside every envelope after the projection's binds; the
  argv — `--new-session`, the ipc/uts/cgroup namespaces and, by host fact, the
  pid one, `--proc` on both projections, the seccomp deny-set
  (`sandbox/linux/seccomp.rs`: `Filter`, `Syscall`, `explain`); `InfoFd`, the
  payload's pid read back for a `Kept` launch; `linux/host.rs`, `HostEnvelope`,
  the probed host facts the render is pure in), and
  `windows.rs` (Job Objects capping the child tree at 512 processes, plus the
  AppContainer backend in `windows/`: `appcontainer.rs` owns profiles and
  LowBox `SECURITY_CAPABILITIES`; `dacl/mod.rs` exposes the path-derived
  capability-SID engine; `session.rs` composes them). Within `dacl/`,
  `stamp.rs` owns completion witnesses, `mutex.rs` per-path locks (named by the path's Windows collision key, folded beyond ASCII and at least as far as NTFS folds, where the witness and capability name are not: a lock errs toward merging, a witness toward splitting, and the stamp store is locked by an environment-spelled path two sessions may spell apart), `ledger.rs`
  profile recovery, and `acl.rs` queried descriptors and borrowed ACEs.
  `windows/local.rs` owns Win32 local allocations. Kernel handles use
  `OwnedHandle`; the path mutex guard stays on its acquiring thread.
  Ported units retain their MXC provenance.

The Windows backend is **path-keyed**: each `(canonical path, kind)` grant —
kind ∈ {rw, ro, ls} — derives a deterministic capability name
`ral.fs.<kind>.<128-bit-truncated SHA-256 of the canonical path>` — hashed as-is,
never case-folded, so a case-sensitive directory's two distinct names cannot
merge into one authority — and thence a capability SID via
`DeriveCapabilitySidsFromName` (`dacl::fs_capability_name`).
`dacl::ensure_frozen_grant` stamps that SID's
inheritable ACE once and never reverts it. **A read-write grant is two
permanent mutations, and neither witnesses the other**: the mandatory-integrity
check runs before the `AppContainer` pass, and an unlabeled object defaults to
`Medium`, refusing every `Low`-IL child whatever the DACL says — so
`ensure_low_integrity_label` also stamps a `Low` `SYSTEM_MANDATORY_LABEL_ACE`,
asked *before* the ACE's witness is consulted and witnessed under its own stamp
key and its own SACL probe. For each mutation two witnesses gate skipping it —
the completion store (`stamps.json`, atomic tmp+rename, per-path
named-mutex merge) recording completed propagations, and a probe of the root's
own DACL confirming its ACE remains. Before an ACE is reapplied, its old
completion witness is removed under the path mutex; recording follows
the apply, so a crash mid-propagation leaves no witness and the next grant
re-stamps idempotently while a child in the interim fails closed. A spawn's
kernel-checked reach is then exactly the capability SIDs `session::confine`
mints into its token, so attenuation shrinks-only: a narrowed grant or subagent
gets a token lacking the wider paths' capabilities. An ACE lives on the NTFS
object and Windows does not re-inherit on a same-volume rename, so stamped
authority is object-sticky where a grant rule is path-based — `dacl/mod.rs`'s module
header records the resulting drift in both directions
([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).
Every frozen allowance is checked before any stamp filter and stamped through a held handle on the very object checked, no redirecting reparse point on its path (a redirected one refuses the launch; a name surrogate redirects, a cloud placeholder does not; [[internals/confined-launch|confined-launch]]); the projection key covers the whole `read` list, places and live support included; and a launch whose fs projection holds the universal prefix is refused before anything is stamped.
Within that shape: an AppContainer profile is still minted per *distinct fs
projection* (`SessionSandbox` maps `bind_spec` identity to profile) for the
deny-by-default token and named-object namespace separation, carrying no fs
authority, and `DaclManager` is the profile ledger — teardown deletes profiles
but restores no ACEs. **A deny stamps nothing**: the `AppContainer` pass of the
access check matches a deny ACE against the token's ordinary groups only, never
its capability SIDs, so Windows cannot take back what a grant's ACE admits
([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]],
amendment of 2026-10-20). So `session::confine` first carves each exclusion out
of the allowances around it: `sandbox::carve::carve`, a pure planner over an
injected lister and tested on every host, replaces such an allowance with its
existing entries less the excluded ones, down to each exclusion, and grants each
directory on the way only its own listing (`GrantKind::List`, an ACE nothing
inherits), so a name made there later, the excluded one included, reaches
nothing granted; links among the entries are neither granted nor followed. An
exclusion on or around an allowance, the program image included, cannot be
carved, and `session::refuse_deny_meeting_grant` refuses it before anything is
stamped. For every exclusion `dacl/deny.rs`'s `ensure_unreachable` then runs `AuthzAccessCheck` at each
launch, in an `AuthZ` context carrying the child's package and capability SIDs,
on every object beneath it, refusing the launch if any right is granted — an
`ALL APPLICATION PACKAGES` ACE, or a hard link carrying a granted file's ACE.
The walk checks reparse points as themselves without following them, polls
cancel per entry, and refuses an exclusion it cannot read. The child's program image
is granted read-only so a user-installed binary or the bundled-tool self image
can load at all, handed to `session::confine` as a `RealPath`, so the
canonical form the pin and the stamp's local-prefix allow-list demand is the
type's, not the caller's care; and `` net: `offline `` is enforced by withholding the network
capability SIDs — a LowBox token without them cannot open a socket, so
`net_enforced()` holds on Windows.

`macos-base.sbpl` is the policy-independent Seatbelt base every rendered macOS
profile inherits, and `build_profile` lays the grant's rules over it. Both are
read rule by rule — what each admits, what it withholds and why, and what the
backend pays to express an object policy in Seatbelt's name language — in
[[internals/seatbelt-profile|seatbelt-profile]].

Path-scoped *exec* confinement on Linux is a Landlock layer the payload enters
inside the envelope; a deny inside an allowed directory is subtracted over
the tree at launch, and only a veto under a trusted admit stays with the
in-process guard —
[[decisions/260906_landlock-exec-layer|landlock-exec-layer]].

`diag.rs` (with per-platform readers in `diag/macos.rs` / `diag/linux.rs`) turns
a kernel-reported sandbox denial into an actionable hint on the
failing command's `Error`: it reads the kernel log over the call's wall window
(Seatbelt on macOS, the seccomp record inside bwrap on Linux), keeps only lines
attributable to the call's descendant PIDs, and appends them. **The operand's
class decides the remedy, and the hint answers once per class present**: a
`file-read*` path is offered to the grant's `read` set and a `file-write*` one
to `write` (advice that fails twice, otherwise), a `process-exec` path to the
`exec` set — the layer a re-exec through `sh -c` or `find -exec` reaches, and
the only one that sees it — a `network-*` denial to the `net` axis alone, and a
`mach-*`/`ipc-*` operand to nothing at all, because a door is the base
profile's to decide and no grant widens one. `Denied` is that taxonomy as a
type, so a service name cannot arrive where a path is expected; withheld doors
carrying a reason (`macos.rs::withheld_doors`, the `.sbpl`'s prose as data) are
quoted with it and lead the hint, since they are the one class the reader
cannot act on — the securityd denial that broke `cargo fetch` was once ranked
below a `.GlobalPreferences.plist` probe and answered with a grant that would
have changed nothing. macOS logs fully-resolved paths, so the hint names the
exact path with the symlink caveat; on Linux, an operandless denial first tries
`describe_denial`, which consults
[[decisions/260906_seccomp-is-a-typed-deny-set|the seccomp deny-set]] by
syscall number and, when it names one, repeats that rule's own reason instead
of guessing at an fs grant — a foreign-ABI record (an `arch=` mismatch) is
named as such rather than misread as an unlisted syscall; only where the
deny-set has nothing to say does the hint fall back, and it then says the
record names no operand rather than guessing at a set to widen. Windows has no
kernel denial log to scrape at
all, so its arm gates on the exit code alone: only an access-denied-shaped
exit (`ERROR_ACCESS_DENIED` / `STATUS_ACCESS_DENIED`) under an active sandbox
yields the fixed, pathless hint — never a fabricated path.

This boundary is what [[map/exarch|exarch]] reuses as its sandbox. Bundled
tools route through the *exec* chokepoint in-process; their **filesystem**
access has no in-process guard, so a bundled tool is never inlined — it is
spawned as a `ral --ral-bundled-tool` child and, under a restrictive grant,
floored by the OS profile of the per-command sandbox it runs in
([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
[[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]]).
That single binary carrying both ral and its coreutils is part of why ral
is a [[invariants/single-binary|single-binary]]. `docs/SPEC.md` gives the
formal capability calculus.

Every `fs`/process constructor in this layer is a reviewed *syscall site*: the
workspace bans the raw constructors via clippy `disallowed_methods`, so each call
site carries an `#[allow(… reason = "[…]")]` classifying it as a surfaced
exec image (`Launch::new`), silent infrastructure (the self re-exec, the
pinned binaries' exec (`Pinned::command`), the `ps` denial sampler, the
boot-time binary pin, the stamp-store and profile-ledger lifecycle), or
test scaffolding. The site
shapes and their rail rendering live in [[map/exarch/io-surface|io-surface]]; here
the sites are only declared and accounted, with `core/tests/syscall_sites.rs`
failing CI on any unaccounted constructor.
