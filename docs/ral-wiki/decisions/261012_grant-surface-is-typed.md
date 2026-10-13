---
status: active
generated_at_commit: 6bfbb097
verified_at_commit: 6bfbb097
anchors: [Place, Selector, FreezeCtx, Net, EditorVerb, ShellVerb, Holds]
---

# The grant surface is typed data

**A grant is a record of tagged values, not a record of strings. The authority
model underneath was already typed (places, executables, a chain of network
modes, verbs); the surface now says so, so a misspelling is a type error
before the program runs, and the decoder parses nothing back out of a string.**

## The surface

```ral
grant [
    exec: [allow: [`name git, `cwd tools/build, `dir `system], deny: [`name bash]],
    fs:   [read: [`cwd ., `xdg-data .], write: [`cwd build], deny: [`cwd .git]],
    net:  `offline,
    editor: [`read],
    shell:  [`chdir],
] { … }
```

- **Places are tagged paths.** `absolute` takes an absolute path; `cwd`,
  `home`, `tempdir`, `gitdir` and one `xdg-*` tag per `XdgKind` are anchors,
  each taking a relative subpath, `.` for the anchor. A bare string, `~`,
  `xdg:NAME`, `path:`, `system:` and the trailing slash are gone. A prefix that is only expanded at run time is refused in
  every payload, naming the place it means.
- **Selectors live under `dir`.** `path` and `system` name directories the
  environment chooses, and their expansion has no containment guard, so a
  `$PATH` entry can be `$HOME`. They are valid only as the payload of `dir` in
  an `exec` list, never in `fs`: a write grant on one would let a child plant
  programs where the user's own shell looks. Splitting the types makes
  ``allow: [`path]`` a static error.
- **An exec target is `name`, a place, or `dir`.** `name` is the host-`PATH`
  name grant; a place is one executable; `dir D` is everything beneath `D`.
  Directory scope is spelled, never inferred from a stat.
- **Dead grants are symmetric.** An `absolute` payload is classified on the raw
  string, before folding, host first: live if absolute under the host's rules,
  dead if absolute only under the other platform's (it names no path here, so
  one profile loads on both), refused if under neither. A well-formed
  forward-slash UNC `//server/share/x` is live on both platforms; `\x`, `C:x`,
  relative paths and malformed UNC roots (`\\` or `//` with no server and
  share) name no path anywhere and are refused.
- **Lists are sets, repeats are idempotent.** An entry written twice, or
  arriving through spreads of two computed lists, adds nothing. A rule refusing
  repeats would have to tell a written list from a computed one, which the
  decoder cannot, and would refuse an overlap that changes nothing. A target in
  both `allow` and `deny` is denied.
- **Nested tables are records,** as a profile is. A map, `[:]` included, is
  refused statically with the record spelling; the empty table is
  `exec: [allow: []]`. The checker ascribes nested tables from a queue drained
  once in `typecheck`, so the check is independent of statement order and never
  constrains a variable ([[map/core/typecheck|typecheck]]).
- **`net` has one value, `` `offline ``.** A grant only narrows, so `net: true`
  restricted nothing and is not a spelling.
- **`editor` and `shell` are verb lists,** and `detach` is a `shell` verb. A
  verb is permitted only where every supplying layer lists it, so
  ``shell: [`chdir]`` withholds `detach`, the safe direction. Under extension
  the lists **join**: a list has no spelling for a veto, so an extension adds
  authority and never removes it, and a base that omits the axis keeps every
  verb.

## Neutral layers launch ambient

`net: true`, `detach: true` and fully enabled verb records restricted nothing
yet counted as restrictive, switching on resource limits, process braking and
loader-variable scrubbing. Activation is now stated per axis: `exec` and `fs`
whenever supplied, `net` when `offline`, a verb list unless it names every
verb. A neutral layer launches with ambient behaviour, and inside a restrictive
stack it cannot turn the protections off. ([[design/two-enforcers|two-enforcers]])

## No argument filtering

A first-argument filter ignored the remaining arguments and understood neither
options nor command semantics. `exec` is allow or deny of a target, regardless
of arguments; `only` and subcommand lists are refused rather than read as an
unrestricted allow. A future argument policy needs its own design over the
whole argument vector.

## Relations

- It supersedes the surface half of
  [[decisions/260727_exec-surface-fails-loudly|exec-surface-fails-loudly]]: the
  `dir` constructor replaces the trailing slash it kept, and the empty
  subcommand list has no home once subcommand lists are gone.
- The surface now mirrors the partition of
  [[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]:
  `name`, a place and `dir` are its `Name`, `Path` and `Dir` keys.
- The exec and fs algebra is left alone here; it is the subject of the
  decision that follows.

## Where

`guard::freeze` (`Place`, `Selector`, `FreezeCtx::place`, `FreezeCtx::selector`),
`capability::lattice` (`Net`, `EditorVerb`, `ShellVerb`), `typecheck::contract`
(`Holds`). See [[design/grant|grant]],
[[design/capability-freeze|capability-freeze]] and `docs/SPEC.md` §12.
