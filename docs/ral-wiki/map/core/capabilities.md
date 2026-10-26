---
generated_at_commit: 77866699fd8865982c0481d278a7d3286109ca65
generated_at_date: 2026-10-26
covers_paths: [core/src/authority/, core/src/authority.rs, core/src/path/, core/src/path.rs, core/src/own.rs, core/src/own/, core/src/shell/grant.rs, core/src/shell/grant/, core/src/shell/runtime/state/checks.rs, core/src/shell/runtime/walk.rs]
---

# Map: core / capabilities & sandbox

The [[design/grant|grant]] mechanism in two halves: an in-process decision layer
and an OS process sandbox that enforces it for external commands — each
authoritative exactly where the other is blind ([[design/two-enforcers|two
enforcers]]). Authority is attenuated by intersection — a `grant` block can only
narrow.

## The model — `core/src/authority/policy/`

`authority::policy` owns the resolved model and its judgments: `GrantLayer`
(one layer, resolved), `GrantStack` (their dynamic composition) and the folds
over it. `Capabilities` is the authored value; `authority::grant` owns its
resolution and all construction that reads the host, including the system reads.
The in-process guard (`core/src/authority/guard/`) and the OS sandbox
(`core/src/authority/sandbox/projection.rs`) enforce the resolved model
([[decisions/261006_capability-is-data|capability-is-data]]). Every judgment
combines the layers' *evidence* ([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]); there is no `Capabilities::meet`
([[decisions/260906_object-not-name|object-not-name]]). Why `Capabilities`,
the live judgment, and `SandboxProjection` are distinct and not one is argued in
[[design/capability-carriers|capability-carriers]].

Submodules:

- `lattice.rs` — the types and their algebra: `Capabilities`, the authored
  value (over spellings; nothing judges it and no layer keeps it);
  `GrantLayer`, one layer in force, resolved where it was decoded
  (constructed in `authority::grant`, or by `shell::grant::decode_capability_map`;
  [[design/capability-freeze|freeze boundary]]), holding its rules and nothing
  beside them; `GrantPath<R> { written, real }`, a spelling and what its walk
  made of it, `R` an `Extent` (`Everywhere`, or `Under { position, passed }`: a
  position and what the walk passed on the way) for an allowance
  (`Everywhere` is the root spelling `/` on any host, or a Unix walk that reached `/`)
  and an `Excluded` for an exclusion; `Net` (`Offline`, the one opinion a layer
  holds on the network), the verb sets `BTreeSet<EditorVerb>` and
  `BTreeSet<ShellVerb>` (each `Option`: absent inherits), `GrantStack` (with
  `push`, `remove`, `rehold`, `summary`), `Meet`, `Join`, `Axis` (`join_top`,
  `is_top`), `Evidence { positive, negative }` and `Fragment`, the extension
  file's result, consumed only by `GrantLayer::extend` (roles are types: a
  policy's omitted axis is `t`, a fragment's is `⊥`); and the authored exec grant
  `ExecGrant { every, rules }`, both fields private, built only by `Default`,
  `FromIterator<(ExecKey, Evidence)>`, `Join`, `join_top` and the wire decoder,
  read through `every`, `iter` and `get` — `ExecKey::{Name, Path, Dir}`; `every`
  is a flag only `join_top` sets and has no negative form. `GrantSummary` is what
  crosses hosts: spellings, never a position or an identity. `Flag`
  (`Editor(EditorVerb)`, `Shell(ShellVerb)`) and `GrantStack::permits(Flag)` are
  the one verb vocabulary: membership, a meet over the layers, silence
  permitting. `is_restrictive` is the denotational activation predicate, each
  axis through `is_top`: exec unless `Any` with no exclusion, fs unless it writes
  `Everywhere` and excludes nothing, `Net` when `Offline`, a verb set unless it
  lists every verb;
- `table.rs` — `Table<A, D>`, authority as two sets, allowances and exclusions
  ([[design/authority-tables|authority-tables]]): `Allows` (`contains`, `own`) and
  `Denies` (`matches`, answering a `MatchedBy`), `supports`, `excludes`,
  `evidence` (computed, never stored), `live`, `withheld` (a `Withheld`:
  `MissingAllowance`, or `Excluded` citing the exclusion that matches the least
  way), `rehold_with`, and `Meet`/`Join for Table`, exclusions joining and
  allowances meeting at their own subjects; the meet's obligations and the
  generic law test are its property tests;
- `fs.rs` — the fs instance: `FsOp`, `FsAllow { Prefix(GrantPath<Extent>),
  RunPlace(RealPath) }`, `FsDeny(GrantPath<Excluded>)`, `FsRules = Table<FsAllow,
  FsDeny>`, and `FsLayer { read, write }`, the two tables a layer holds, each
  with every exclusion; `region(stack, op, baseline)`, each supplying layer's
  tables met (writing reads) and the system reads from `Baseline` joined;
  `Baseline` also supplies the bundled-tool image and protected boot identities,
  assembled in `authority.rs`, without a policy-to-sandbox dependency;
  `ReadRegion`, the read and
  the running regions (run places `P≤L`) folded once for a walk that judges many
  objects. `GrantStack::judge`, `judge_dir` and `locate` are the one place
  evidence becomes authority: the pinned-binary veto (`Refused::Guarded`) first, then
  the fold, then the handle `Judged<O>`; `GrantStack::read` is the read
  judgment, `region` then the running region only on a missing allowance;
- `exec.rs` — the exec instance
  ([[decisions/261004_exec-rules|exec-rules]]) and its judgment: `Program`
  (`Tool` or `File { arg0, located }`, one owned `Located`) and the `Subject` it
  is judged as (`Program`, `Dir`, `Tool`, `Any`); `ExecAllow` (`Any`, `Dir`,
  `File`, `Tool`) and `ExecDeny` (`Name`, `Dir`, `File`); `ExecRules =
  Table<ExecAllow, ExecDeny>`; `rules`, the stack's table; `places`, the run
  places. Authored exec targets are resolved by `authority::grant`, not here;
  `GrantStack::admit(baseline, program, args)` is the one exec judgment, the table
  then the read judgment and the only mint of `Admitted` (the program as
  `Program<Judged<Reading>>`, its argv and the table that judged them, private
  fields, all a launch reads); a refusal is data, `Refused { program, args, why:
  ExecDenial }`, `ExecDenial::{Run(Withheld<ExecDeny>), Read(Withheld<FsDeny>)}`;
  `admits(&Program)` is head-only admission. `ExecRules::for_kernel(support)` is
  the table less bundled-tool allowances, joined with the live support as `File`
  allows, what the kernel is handed; `allowed_files` the live file allowances the
  carriers are computed over. Vetoes key on `path::command_name_key`;
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

The `shell::load` module holds `shell/load/profile.rs` —
`load_capabilities_from_path` / `_from_str` for `.ral` capability profiles,
compiled under `grant`'s own declared table (`typecheck::contract`,
`Form::Grant`): a profile's dimensions *are* the form's options, so a returned
row is held to them statically and the decoder below is where a profile
returning a map meets the same six keys.

## The guard — `core/src/authority/guard/`

The model's in-process enforcer: free `guard::check_*(&GrantStack, …)`
functions fold the whole stack — a module boundary rather than a typestate
([[decisions/260605_witness-collapse|witness-collapse]]). It consumes resolved
authority and returns domain data, never runtime values or control flow.

- `enforce.rs` — the guards over the model's judgments: `check_exec`
  (`GrantStack::admit` and the wording of its refusal), `check_fs_exact` (the
  access a walk found, for a probe's name), `fs_denial` (the wording of a
  `Refused`), `check_flag`, `check_device_name`, and `deny_head`, the refusal of
  a head before its argv is known. The exec and fs checks return
  `Result<_, Box<Denial>>`, a `Denial { check, error: Refusal }`, over
  `Refused::{Guarded, Withheld}`: a `fact::Check` and diagnostic data
  (`Refusal { message, hint }`), given runtime status only by the shell.
  Flag and device checks return `Result<(), Refusal>`. The judgment is reusable
  without the report: `GrantStack::refusal_fs` decides — `Refused::Guarded`
  first, for a write onto a boot-pinned sandbox binary (the supplied
  `Baseline`'s protected identities), before any grant is folded. `GrantStack::admits_fs` is the quiet twin a write asks before
  reading its before- and after-images.

## The shell adapter — `core/src/shell/runtime/state/checks.rs`

- `impl Shell`: `check(Flag, who)`, `check_exec`,
  `baseline`, `locate`, `locate_dir`, `locate_existing`, `locate_if_admitted`
  (each excusing the discard device `LexicalPath::is_discard` and refusing a
  reserved device name before either region is consulted), `judge` (an object a
  caller already holds), `admits_fs`, `sandbox_projection`, and the grant scope
  guards (`with_layer`, `with_layers`, `push_session_capabilities`,
  `audit_deputy_prefixes`, `has_active_capabilities`). `Shell::locate` is the
  one path every read and open takes: the name refusal, `path::walk` to the object,
  then `GrantStack::judge` on its transcript, handing back the handle vouched
  for. `Shell::refused` mints a `Denial`'s `Break` and records its check
  through `Shell::record_check` with no `Mooring`, so on the trail alone.
  `From<Refusal> for Error` preserves the message and hint at this boundary.

## Construction — `core/src/authority/grant/`

`authority::grant` owns freezing, directory selectors, authored resolution
and host system-read discovery. It builds the resolved model that `policy`
judges, without naming a shell or runtime value; the `Value` decoder and
spawn adapter live in `shell::grant`.

- `paths.rs` — `Polar` and grant-path walking: allowances keep positions,
  exclusions hold objects;
- `resolve.rs` — authored `Capabilities`, filesystem policies and exec keys
  resolved into layers and tables, including PATH lookup;
- `host.rs` — platform system-read discovery and resolution; `authority.rs`
  attaches the sandbox's boot pins to form the runtime `Baseline`;
- `freeze.rs` — the one place a grant's `Place` or `Selector` becomes a
  `FrozenPath` (a folded spelling, which `decode` then walks), against one anchor:
  `FreezeCtx::at` (an owned `host::EnvVars` and `CwdPath`: the `Resolver` pair,
  with optional scratch) and its `place` (`None` is a dead grant, an `absolute`
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
  `cwd`, `home`, `tempdir`, `scratch`, `gitdir` and `xdg-*`, `PolicyError`, the `gitdir`
  refusals (`From<BadPointer>`), `Selector` (`Path | Toolchain(Toolchain)`) and
  the `path` selector (`path_dirs`, over the pure `path_dirs_in`, which
  classifies each absolute `PATH` entry raw, as an `absolute` payload, before
  the freeze folds it), and
  `refuse_the_world`, the guard both it and the toolchain overrides call, over
  `the_world`, the one predicate for a root, the home or an ancestor of it,
  which system-read discovery uses too;
  `FrozenPath::freeze` refuses a path that is not Unicode;
- `toolchains.rs` — every other directory selector as one table: `enum
  Toolchain` (`system` and the package managers), each a few `Location`s of
  overrides and defaults, from one exhaustive `match`; the pure `expand_in`
  over an injected environment, home and `exists`, answering native
  `PathBuf`s (shown only in a refusal), its live wrapper `expand`
  (what `FreezeCtx::selector` calls), and
  `SYSTEM_READS`, the four system-wide prefixes (`homebrew`, `macports`, `nix`,
  `pkgsrc`) used by grant's host system-read discovery, keeping only what is
  neither in the process's home nor `the_world` ([[design/grant|grant]]
  §Selectors states the rules).

## Runtime decoding — `core/src/shell/grant/`

`shell/grant.rs` supplies `Shell::freeze_ctx` (env, logical cwd and scratch)
and the `PolicyError → Break` conversion; substantive freezing stays in
`authority::grant::freeze`.

- `decode.rs` — `decode_capability_map`, which walks a `grant [...]` /
  `--capabilities` `Value` map into a resolved `GrantLayer`, one dimension
  decoder per `exec` / `fs` / `net` / `editor` / `shell` key, nested tables
  and verb lists read as sets —
  every one of them authority, none of them a recording switch
  ([[design/audit|audit]]). Its exec targets freeze and walk through
  `authority::grant` directly into exec rules: a `dir` selector expands once
  per directory, a bare name reads the decoding environment's `PATH`, a path
  allowance holds the position its walk reached, and an exclusion the objects
  held;
- `spawn.rs` — `SpawnGrant` (`Inherit`, `Base`, `Restrict`), a spawn's grant
  as it crosses to a child that has not yet resolved it, frozen against the
  child's own `FreezeCtx` through `Shell::freeze_ctx`; `GrantNarrower` is the
  host-supplied `fn(&str, &FreezeCtx) -> Result<GrantLayer, String>` that
  resolves a named base.

## Path resolution

Path resolution is `core/src/path/`: lexical stages, then one walk, plus
`which.rs` for PATH search.

- expand — `sigil.rs` (run-time `~` and `xdg:` expansion of ordinary paths,
  never in a grant; the places `cwd`, `tempdir`, `scratch`, `gitdir` are policy-only,
  expanded at freeze by `grant::freeze` (`scratch` against the spelling of the
  session's scratch that the `FreezeCtx` carries, which plain ral has none of); `git.rs` backs `gitdir` discovery, following a `.git` pointer file only to a
  git directory whose `gitdir` back-pointer or `core.worktree` names the working
  tree back, judged by the positions the two walks reach, never by identity; a bad pointer is a path-level `BadPointer`, phrased as a grant error by the caller), `tilde.rs` (`~` and `~/sub` only; an unset `HOME` is `None`,
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
  grant refuses the gap outright in `grant::freeze`;
- lex — `lex.rs`: one structural parser, `parse_bytes`, reading a byte slice under `PathRules` into `Parsed { raw, rules, kind: HeadKind, spelling: HeadSpelling, head_len }`, from which resolution, `.`/`..` folding and every head judgment project. `HeadKind` names the structural head (`Posix`, `Drive { letter, volume }`, `Unc { volume }`, `Driveless`, `DriveRelative { letter }`, `Relative`, `Device` for `\\.\`, and `Malformed` for a two-separator or `\\?\` head that is no well-formed drive, share or device); `HeadSpelling` records the opening separator and namespace facts once (`namespace: Option<Namespace>` — `Verbatim { exact }` for `\\?\`, `Device { exact }` for `\\.\`, `Object` for `\??\`, with `exact` / `Namespace::is_exact` true when all three separator bytes of that 4-byte prefix are `\` — `mixed_opening` for `/\` or `\/`, and `opens_with_slash`). `fold_dots_under` (over `fold_bytes`) is one kernel under either rules from any host, `fold_dots` its host instance on a held path: `head_len` marks where the head ends and only the names past it fold, joined with the rules' separator, so a fold preserves `HeadKind`, `volume` and (save when `..` climbs into the `??` component of a `\??\` driveless path) `HeadSpelling::namespace`; under Windows rules a non-namespace `Drive`, `Unc`, `Driveless`, `DriveRelative` or `Relative` head (`Parsed::respell_head`) is respelled with `\` too (`C:/a/b` folds to `C:\a\b`, `//srv/sh` to `\\srv\sh`), so such a path has one separator throughout, while every head whose kind or meaning turns on its spelling is kept as written: a `Posix` `/` (as `\` it would be `Driveless`), a slash-spelled verbatim head (`//?/C:/x` is a device path Win32 normalises, so its tail past `head_len` folds), and a `Device` or `Malformed` path, or an exact `\\?\` verbatim path (`Namespace::is_exact`, `head_len == raw.len()`: Win32 hands its rest to the kernel uninterpreted, `/` a name byte and `.`/`..` names), all head and kept whole, `..` included; a relative path's drive-shaped first name keeps a `.\` before it; `is_unparsed_head`, a Windows path absolute to the kernel in which `std::path` finds no prefix and so would descend or climb from the current drive, which the walk refuses. Every head predicate projects from `parse_bytes`: whether a path is absolute under either rules set (`absolute_under` / `Parsed::is_absolute`: POSIX, `HeadKind::Posix`; Windows, `Drive`, `Unc`, `Device` or `Malformed`) and on this host (`is_absolute`, `absolute_under` the host's rules, the one absoluteness ral asks, `anchor` included; `Path::is_absolute` is banned in clippy.toml in its favour); `is_working_directory` (`Posix`, or `Drive` / `Unc` under Windows); `is_bare_root` (one separator alone under the given `PathRules`); `has_drive` (non-namespace `Drive` or `DriveRelative`); `Parsed::is_mixed_head` and `Parsed::is_nt_object`, shared by `grant_eligibility` and `classify_subpath` (`SubpathRefusal`), which `grant::freeze` maps to its messages; `is_network` (`HeadKind::Unc`), `is_verbatim_drive` (`Namespace::Verbatim` + `HeadKind::Drive`) and `is_volume_guid` (`has_os_namespace_head` with a `Volume{` `namespace_tail`), which the Windows DACL backend asks rather than parsing prefixes itself; `Parsed::has_os_namespace_head` (`Namespace::is_exact`), what `show` leaves unfolded; `Parsed::without_verbatim_prefix`, what the Windows identity fold slices; and `Parsed::namespace_tail`, what the discard device check reads under `HeadKind::Device`; `utf8`, a path's native spelling as text or `NotUtf8` naming it, the one strict conversion for a path that must travel as a string (`--capabilities` profiles, `SourceFile`), as `shown` is the one lossy conversion, for display only; `spelled_parent`, a path with its last name dropped by spelling alone, for a program's own layout (the folder a file it writes sits in, its install tree, a run folder it made) and never for a name a grant judges, which asks `RealPath::parent`; a final component's `extension`, and whether a component is a Windows drive or stream (`is_stream_or_drive`); `relative_suffix` strips a sub-path's leading separators under the rules, and `join_under` joins a sub-path onto a base string one component at a time
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
  component, for keying per-project state. It is a readable part (`/Users/x/proj`
  → `-Users-x-proj`, lossy, for a person scanning a listing), a `-`, and 16 hex
  digits of the SHA-256 of the cwd's exact OS bytes (UTF-16 units on Windows), so
  two cwds that differ at all key apart: `-Users-x-proj-25cf7293a3713b73`;
- canon — `canon.rs`, the names one object answers to for the kernel
  sandboxes: firmlink twins (`match_variants`); what an object is, and where a
  spelling reaches it, is the walk's to say;
- locate — `walk.rs` exports one resolver from `walk/`: `names.rs` validates
  spellings, `trace.rs` holds evidence, and `exclusion.rs` keeps deny anchors.
  `held.rs` owns private handles and authority conversions; operations in
  `held/`, traversal in `resolve.rs`, and host adapters are its child modules.
  `walk` descends from the root through directory handles with no symlink
  followed by the kernel, splicing each link
  into the name itself (a link's target read through the handle held on it), and
  hands back a `Located` — the directory handle, the leaf, and the `Trace` whose
  `RealPath` is symlink-free and whose `passed` names each link whose target it
  read and each directory a `..` climbed out of — whose every operation is
  handle-relative. Each
  step is one host primitive, `step`, reading the child's stored name and identity
  from the handle just opened (Linux the name `openat` resolved, macOS the
  parent's listing for that identity, Windows the normalized name checked against
  the parent's path). A `..` is physical, the kernel's rule, even from a link
  target (the user's own path keeps its logical fold); the splice count is
  bounded by `MAX_HOPS`. `walk/error.rs::loop_trace` reads the actual failed
  link's transcript from a spent-budget error; `Shell::walk_failure` judges
  disclosure before rendering it, without rewalking the name. Every restart,
  the given name and each splice alike, is
  put to one `rooted` check, so a link target relative to a drive's current
  directory or with an unparsed head is refused as the name itself would be.
  `walk_dir`, `walk_lenient` (a grant spelling's transcript, as far as the disk
  goes), `walk_excluded` (an exclusion's `Excluded`, the link and its target
  held) and `open_discard` are the other entries; `Dir`, `Located` and `Hold` are
  the three holders and `Unjudged`, `Judged<O>` and `Installing` who vouched
  ([[internals/path-judgment|path-judgment]]);
- match — `identity.rs`: `RealPath::contains`, an allowance by position, names
  as stored (on Windows `windows_stored_components`, preserving stored case and
  unifying `/` vs `\`, and `\\?\`-verbatim spellings), and `Excluded::matches`,
  an exclusion by position under `collision_key` (compatibility caseless
  matching of the uppercase image, ICU4X, the coarsest identity any filesystem
  gives a name) or by the object its walk held, answering a `MatchedBy`
  ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]);
  the form-blind `path_within` is private to the file;
- name a device — `device::is_discard_device`, behind
  `LexicalPath::is_discard`: `/dev/null`, or on Windows exactly the
  device-namespace spelling `\\.\NUL` (either slash, any case), and no other
  path. Every other Windows path whose last component is a DOS reserved device
  name — `NUL`, `CON`, `PRN`, `AUX`, `COM1`–`COM9`, `LPT1`–`LPT9`, `COM¹`–`COM³`,
  `LPT¹`–`LPT³`, any case,
  any extension, trailing dots and blanks trimmed — is *refused*, not
  excused: `device::reserved_device_refusal` phrases the error (for `NUL`, as the
  question "did you mean `\\.\NUL`?"), and `guard::check_device_name`
  asks it at the fs entries, `Shell::locate` and its siblings, ahead of any
  grant. A resolved path keeps its last
  component, so the check needs no raw spelling. Like the identity rules above
  both take `windows` as a parameter rather than reading `cfg!`, so neither
  table is dark on the other host.

`resolver.rs` composes the stages — `Resolver::resolve` and
`Resolver::anchor` are the *sole* constructors of a `LexicalPath`
(`forms.rs`, with its grant-side twin `FrozenPath`), so a walk
cannot run before lexing: the ordering is in the types, not convention.
`resolve` takes surface text and expands sigils first; `anchor` takes a path
the host already holds, a walked entry say, and expands nothing, so a file
named `~` stays that file and a non-UTF-8 name is never renamed by a string
round trip.
A `Resolver` is a `host::EnvVars` and a `CwdPath`: `~` reads `env.home()`, `xdg:`
the `XDG_*_HOME` of `env` through `get_or_host`, so a `within [env: …]` overlay
reaches both. `Resolver::at` builds one for a caller with no shell to mint one; `resolve_in_script` is
`resolve` for a path written in a ral script, which reads it beside itself:
it is `resolve` itself with the script's directory for the cwd, so the two
agree: sigils expand first, only a still-relative result joins the
script's directory, and an absolute one, a malformed head included, is kept
as `resolve` keeps it, never lent the directory's drive. The directory is
the native parent of the file's `SourceOrigin`, never a re-parsed rendering
of it, anchored as `anchor` takes it, so a script under a directory named
`~` stays there. A virtual source has no directory,
and its paths resolve against the cwd.

(`which::ral_path_entries` lists `RAL_PATH`; `use` searches it through the
shell's resolver, and the plugin loader folds it into its own candidates.  The
name beside the script is `use`'s first candidate; each candidate is judged
before its kind is asked, a denied one passed over.)

`tree/walk.rs` is the one directory-tree walk, `Walk`: ral enumerates, with
`Dir::read_dir` and `Dir::open_dir`, depth first in name order, and `ignore` only
matches (`tree/ignore.rs` is the rule engine over `ignore::gitignore`, opening
nothing); `tree/glob.rs` is glob's pattern language, enumerated by it. A `Walk`
is told what it may enter (`respecting_ignores`, `all`; a depth; a `keeping`
predicate; link following) and which region it may read (`readable_by`), judges
every directory it enters, every file it reads for its own guidance and every
entry it yields against it, polls the cancel scope at each entry, and yields
`WalkEntry { shown, depth, found }`, which tells a pruning from a refusal from an
entry unseen. The root comes first and is filtered by nothing. `Entries`
yields `Result<WalkEntry, process::Cancellation>`; `WalkEntry::name` takes an
optional cwd base and returns `Result<String, String>`, with no runtime types.
A name reads back as itself: one a sigil would claim is spelled `./~/x`
(`sigil::literal_relative`), and one no ral string can hold is refused when asked
for. `Shell::admitted_walk` (`shell/runtime/walk.rs`) wraps this as `AdmittedWalk`:
selects the live region, scope and name base (`Resolver::is_relative`: a
cwd-relative root gives cwd-relative names where possible, any other absolute
ones), skips unseen entries, and converts cancellation to a non-catchable stop
and naming failures to contextual errors. `glob` and exarch's
`grep-files`, `grep-hidden`, `find-files` and `explore-dir` use that adapter.

`which.rs` is the one `PATH` walk. **Its anchor is an `Option<&CwdPath>`: the
shell's cwd cell, or `None` for a shell-less caller — and `CwdPath` has one
constructor, so no call site can choose "here" for itself**
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
  caller already holds (`path_dirs(…, None)` for bwrap's pin, absolute entries alone), so no list is
  re-joined into a string to be split again.
- `search` returns `PathSearch::{Executable, FoundNotExecutable, Missing}` from
  one traversal — the executable half memoised, the presence half not — so
  `shell::exec::command::vet` reads a verdict rather than taking a second walk.
- On Windows, `%PATHEXT%` suffixes **append**: `build.ps1` yields
  `build.ps1`, `build.ps1.EXE`, …, never `build.exe`.
- Grant construction walks the `PATH` of the decoding environment with
  no anchor, so neither a scoped `PATH` nor a relative entry can
  redirect a bare key.

A `FrozenPath` (`forms.rs`) is a spelling and nothing else: folded, shown,
ordered; a guest spelling is minted by `from_guest` from a `GuestPath`
(`namespace.rs`) under the POSIX fold. What it names is the walk's to say, once,
when the layer is decoded: `GrantPath { written, real }`.
[[invariants/grants-judge-objects|Every authority is judged on `real`]]:
`RealPath::contains` for an allowance, `Excluded::matches` for an exclusion —
and no other way: `path_within` and its string twin are `pub(super)` to
`identity.rs`, so the form-blind kernel does not leave `core/src/path/`, and
outside the module a spelling leaves the type only as a *string* (`as_str`,
`into_string`) for rendering. That is enforced rather than documented because the
`xdg-*` freeze guard once chose the form for itself — asking on the surface while
the check it guarded matched the real form — and read a symlink out of `HOME` as
contained.

The set-level algebra over prefixes is not in `path/`: it is
`authority::policy::table`, which names polarities and never identities, so every
spelling question stays here. `RealPath` (`walk/trace.rs`) is the position a walk
ends at and the identity of a run place, ordered as the host identifies files.

XDG base directories resolve through one resolver, `path/basedir.rs`
(`XdgKind`, the pure `resolve_xdg_in`; `host::xdg` reads the host env, the `xdg:` sigil and the freeze an `EnvVars`; `app_dir` spells an application's own `$XDG_<kind>_HOME/<app>/` for ral and exarch alike): an absolute `XDG_*_HOME` override else the
home-joined Linux default on every platform, and `None` where there is neither
— a host fact answers with an `Option`, so each caller picks its own fallback
rather than inheriting a fabricated home. For the program's own layout alone
(`host::xdg` and `host::home_dot`) the home falls back, where no `HOME` is bound, to
the account's own from the user database (the `AccountHome` site), so `app_dir`,
`OwnDir`, `UserFile` and the credential denies all see the same base; a `~` in a
script (`host::home`) is unchanged. Where even that fails, `own` refuses with an
error naming what to set, and no temp dir stands in. Both the `xdg-*` grant places
(`sigil.rs`, `grant::freeze`) and the host's own config/data loaders (`host::xdg`) defer to
it, so a grant and the rc/history/plugin paths can never name different
directories — [[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]].

The files a program touches on its own account, outside any shell, are
`core/src/own.rs`'s, under one authority, the crate-private write side of
`authority::policy::Ungoverned`:

- `OwnDir` — what the program keeps for itself (state, tokens, logs, caches,
  records, scratches): a held directory reached from an XDG root, one entry name
  per operation, no link followed beneath it;
- `UserFile` — a file the user writes at a place the program chooses (the rc,
  `config.ral`, house rules): links followed, a replace staged beside the link's
  target so the link survives, `seed` writing only where nothing stands;
- `read_named`, `open_named`, `kind_named`, `create_named` — a file the user
  names, the one write being a new file. They take any path, so `clippy.toml`
  bans them, and each caller is a typed site that says why the name is the
  user's: a CLI argument, a TUI command, the install tree.

`own/site.rs` holds three authority-mint sites, `Silent::{Read, Write,
WriteDir}`, for clippy-banned `Ungoverned` methods, but no syscall sites:
each syscall it needs is a walk primitive
(`walk_dir_made`, `Located::{remove, rename, open_data}`, and `Visibility`'s
`Public`, `Private` and `Secret`). The config home holds only what the user
writes ([[decisions/261022_own-files|own-files]]).

## OS sandbox — `core/src/authority/sandbox/`

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
(`docs/SPEC.md` §12.11). Unchanged means head-only: `build_launch`
(`shell/exec/command/process.rs`) builds no projection under a jail, so a grant's
fs and exec tables bind the command head and nothing an admitted program then
spawns (`rg --pre`, `env`, `find -exec`). The jail, the uid, the cgroup and the
VM boundary are the only confinement of grandchildren.

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
  `reexec::launch(role)` is the one launch entry for the unconfined
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
  the trampoline by name; Linux execs it from ral's pin, lent like every
  descriptor, so there is no name to verify.
- `projection.rs` — the sandbox's own view of authority:
  `SandboxProjection::of(&GrantStack, &Baseline, Option<&Admitted>)`
  meet-folds the stack once, at spawn, into `FsProjection` (`read:
  Vec<FsAllow>`, the live positives of the read region with run places and live
  support among them, beside the write prefixes, the system reads and the exclusions;
  allowances keep their resolved positions up to rendering, exclusions render
  from the resolved real paths their walks held; `FsRules`, whose `rendered()`
  derives `pinned_dirs`; no write
  reach classification) and `ExecProjection::Restricted(ExecRules)`,
  `ExecRules::for_kernel` of the `Admitted`'s own table joined with the live
  support (`sandbox::carriers`), so the profile renders the table that judged
  the launch rather than a second compile. It takes no `Shell`;
  `Shell::sandbox_projection` (`shell/runtime/state/checks.rs`) is the entry.
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
  (`shell/exec/command/process.rs`) routes an external or bundled child through here
  whenever a projection is active and the process is not already confined,
  confining that *one* child, the `Admitted`'s program: a `Program::File`
  by the real path the guard judged, its spelling as `argv[0]`, or a
  `Program::Tool`. macOS
  and Linux share one trampoline: the pinned ral itself, whose whole argv is
  `ral --warrant <fd>` (`Role::Warrant`), the warrant's descriptor. The confinement to enter and the program to
  run cross in a `Warrant` (`sandbox/warrant.rs`, its fields private) compiled in
  the parent. The confinement is the `Confinement` trait — `spell`, `parse`,
  `enter` — implemented on macOS by `Seatbelt(String)`, the profile
  `macos::build_profile` compiled, and on Linux by `Option<Landlocked>`, spelled
  `unconfined`, `landlock@<fd>` or `landlock+refer@<fd>`, the ruleset's descriptor;
  the program is a `Run`, `File(path, args)` or `Tool(tool, args)`.
  `Warrant::confine` is the only way to the program: receive and verify the
  warrant, enter the confinement (`macos::apply_profile`, `Landlocked::enter`),
  close the handoff range, and yield a `Confined`, whose `run` `execve`s the
  host program or runs the bundled tool in-process (`uutils::run`, shared with
  the `--ral-bundled-tool` multicall). `serve_warrant` serves the child: a
  failure before the program starts exits 126, and an `execve` refusal takes its
  code from `SpawnFailure` (`From<&io::Error>`) — 126, or 127 for a missing
  program.
  The encoding is NUL-terminated fields behind a `ral-warrant/3` magic — a
  `file` is its real path, then its `argv[0]` spelling, then the exclusion covering its script's interpreter (empty if none), then its argv; a `tool`
  its name, then its argv — canonical (decode re-encodes and compares) and at
  most 8 MiB. It rides a descriptor, only its number in argv: a sealed
  memfd on Linux, checked again after sealing, and on macOS a prefilled
  socketpair whose writer closes before the child exists; the child
  refuses an unsealed memfd or a non-socket. Every descriptor a launch
  hands down crosses at the number a `Handoff` lifted it to, from 98 up
  (`Slot`): bwrap's `--args`, the warrant, `--info-fd` (a socketpair,
  which unlike a pipe cannot be reopened through `/proc`) and the
  payload's copy of its writer, whose sweep ends the parent's read once
  the payload's group exists, the Landlock ruleset, ral's pin, the
  trampoline bwrap execs, the seccomp programs (sealed memfds) and one
  mount handle per bind. The child's one `pre_exec` only clears
  close-on-exec: nothing is `dup2`'d, so no descriptor it still uses
  (`std`'s exec-error pipe, `detach`'s handshake) can be displaced. Each
  number is named as data: to bwrap in its options, the ruleset's in the
  warrant's confinement, the warrant's in the trampoline's argv. The
  child sweeps everything from 98 up with `close_range` once confined,
  and `Landlocked::enter` consumes the ruleset.
  macOS spawns the trampoline directly; Linux spawns it under `bwrap`
  (`bwrap_command`), giving the full shape **bwrap → ral trampoline
  → Landlock → `execve`**. The real argv is `bwrap --args <a> --
  /proc/self/fd/<t> --warrant <w>`: `bwrap_argv` is pure in descriptor numbers and returns bwrap's options alone, which
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
  a directory it is listing), each opened in the host by `open_nofollow` / `open_dir_nofollow`
  (`RealPath`'s walk with every link refused, held as `Installing`); an absent admit is dropped, a file admit whose path
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
  too unless within another bind) opened once by `open_nofollow` and lent once per
  bind. A read-only bind within another bind is dropped, a
  writable one only within a writable one; an absent object drops its binds,
  and so does a system default this user may not reach, while a name now
  reaching outside what was rendered refuses the launch. Every allowance is
  bound as its resolved real position, a redirected one refusing the launch, and each
  deny mask goes on wherever a `Shown` bind shows the denied path
  (`Binds::names`): a mount laid inside one bind of an object does not show
  at another, so a prefix written through a symlink, two binds of one
  handle, is masked at both. A deny hiding `/proc` is refused,
  the envelope starting its trampoline through it, and so is one holding the
  working directory, which no `--chdir` can enter. The two pins join the `Over` layer by copies of their own
  descriptors, read-only at the names their inodes hold now (`pinned`, over
  `Pinned::names`), an unlinked pin binding nothing. `bwrap_argv` emits
  `--ro-bind-fd`/`--bind-fd` from the same vector, `Shown`,
  before `/proc` and `/dev`, `Over` after. `--chdir` carries
  the working directory's own bytes, as bwrap's argv does, while every rendered
  name refuses a position that is not Unicode rather than go lossy into
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
  profile recovery, and `acl.rs` queried descriptors and borrowed ACEs. Both
  files keep their JSON in one held `OwnDir`, `$XDG_STATE_HOME/ral/sandbox-dacl/`
  (`ledger_dir`), each replaced whole by `OwnDir::store`.
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
asked *before* anything about the ACE, even where the well-known `AppContainer`
SIDs make the ACE itself redundant, and witnessed under its own stamp key and
its own SACL probe. For each mutation two witnesses gate skipping it —
the completion store (`stamps.json` in that directory, replaced whole and atomically by `OwnDir::store`, per-path
named-mutex merge) recording completed propagations, and a probe of the root's
own DACL confirming its ACE remains. Before an ACE is reapplied, its old
completion witness is removed under the path mutex; recording follows
the apply, so a crash mid-propagation leaves no witness and the next grant
re-stamps idempotently while a child in the interim fails closed. A spawn's
kernel-checked reach is then exactly the capability SIDs `session::confine`
mints into its token, those of its own launch's grants and no other's, so
attenuation shrinks-only: a narrowed grant or subagent gets a token lacking the
wider paths' capabilities. An ACE lives on the NTFS
object and Windows does not re-inherit on a same-volume rename, so stamped
authority is object-sticky where a grant rule is path-based — `dacl/mod.rs`'s module
header records the resulting drift in both directions
([[decisions/260730_path-derived-capability-sids|path-derived-capability-sids]]).
Every frozen allowance is checked before the session's memo and stamped through a held handle on the very object checked, no redirecting reparse point on its path (a redirected one refuses the launch; a name surrogate redirects, a cloud placeholder does not; [[internals/confined-launch|confined-launch]]), and a position that is not Unicode refused as `DaclError::NotUnicode`, since a Windows grant names its object by text; and a launch whose fs projection holds the universal prefix is refused before anything is stamped.
Within that shape: an AppContainer profile is still minted per *distinct fs
projection* (`SessionSandbox` maps `ProjectionKey` to profile) for the
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
is granted read-only, to its own launch, so a user-installed binary or the bundled-tool self image
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
`mach-*`/`ipc-*` operand to nothing at all, because a service is the base
profile's to decide and no grant widens one. `Denied` is that taxonomy as a
type, so a service name cannot arrive where a path is expected; withheld services
carrying a reason (`macos.rs::withheld_services`, the `.sbpl`'s prose as data) are
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

Every `fs`/process constructor in this layer is a *typed site*: the workspace
bans the raw constructors via clippy `disallowed_methods`, so each call is
wrapped in `site!` with a variant that names it, a `Surface` one for the exec
image that raises a card (`Launch::new`, `Surface::ProcessLaunch`) and a
`Silent` one for infrastructure (the self re-exec, the
pinned binaries' exec (`Pinned::command`), the `ps` denial sampler, the
boot-time binary pin); test code
carries one blanket per module. The enums are the list: `core/src/site.rs`
for cross-domain sites, and domain-local `site.rs` declarations in `path`,
`host`, `own`, `process` and `sync`, each variant documenting why its place is
exempt. `Ungoverned::{read, read_dir, region, write, write_dir}`
are also clippy-banned, so each authority mint is a documented typed site.
`core/src/own/site.rs` declares the three own-file authority mints
(`Silent::{Read, Write, WriteDir}`), used in `own.rs`,
but no syscall sites, since every syscall it needs is a walk primitive
([[decisions/261022_own-files|own-files]]). The site
shapes and their rail rendering live in [[map/exarch/io-surface|io-surface]]; here
the sites are only declared and accounted, with clippy failing CI on a
constructor under no site and on a site that no longer wraps one, and
`core/tests/sites.rs` on any other exemption, on a lint group that silences the
lint, and on a member outside the workspace's lints
([[decisions/261022_typed-sites|typed-sites]]).
