# Grants judge objects

**A grant path carries `written`, the author's spelling, and `real`, what its
walk reached; every authority is judged on `real`.** `fs` authorises the
objects the kernel touches; `exec` authorises the file a head will execute
([[decisions/261004_exec-rules|exec-rules]]), which is the object the kernel
judges too. The spelling is kept for rendering and display, never a route to
authority.

A `GrantPath<R>` (`core/src/capability/lattice.rs`) is made once, where a layer
is decoded, by the walk ([[internals/path-judgment|path-judgment]]):

- **An allowance** is `GrantPath<Extent>`: the `RealPath` its walk reached, or
  `Everywhere`. It holds by position, `RealPath::contains`: the access's own
  walk, symlink-free, lands at or within it, so a link planted inside a granted
  region is matched where it lands
  ([[decisions/260906_object-not-name|object-not-name]]).
- **An exclusion** is `GrantPath<Excluded>`: the objects its walk held (the
  name's and, for a link, its target's). It holds by position under
  `collision_key` or by the object, `Excluded::matches`.
- **exec** keys enter `ExecRules` the same way: a path or dir by its resolved
  position, a bare name as the file the decoding `PATH` found.
- **identity — native.** A position is a native path: a name that is not Unicode
  is its own name under every identity, never the U+FFFD sibling it decodes to.
  A spelling whose real path is not Unicode is refused when it is frozen.
- **composition — `Table::meet`**, which joins exclusions and keeps an allowance
  where both layers support its own subject, judges the same positions and
  objects the in-process guard does. No sweep evicts an allowance: an exclusion
  beats it as evidence, never by deleting it.

An object can answer to several names — case and Unicode normalisation on APFS,
NTFS, ZFS and Linux casefold directories — and an absent one has no stored
spelling yet. So a **deny is judged by collision class and an allow by stored
name** ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]):
an exclusion speaks under `identity::collision_key`, an allowance as stored. A
deny then holds the create that would make its object under another spelling,
where an allow never reaches a distinct name on a case-sensitive volume.
Composition keeps the split: exclusions join, allowances meet by stored name,
and the meet writes an exclusion only where a layer wrote one, since a default
so written would hold every other spelling of its name
([[design/authority-tables|authority-tables]]).

A resolved allowance means what the disk said at decode, and keeps that meaning
for good: a granted `~/bin` that is a link to `/opt/x/bin` grants `/opt/x/bin`,
and a link planted at `~/bin` later redirects nothing. An exclusion holds the
object its spelling reached, under whatever name it has or gets
([[decisions/261021_one-path|one-path]]). That is no re-aiming of the author's
intent — a head spelled under `~/bin` executes there, and so the kernel has
always judged it.

## How it is held

The rule is which form, so the only way to get it wrong is to be handed a
choice. Nothing outside `core/src/path/` is:

- **`path_within`, the form-blind kernel, and its string twin are
  `pub(super)`** to `path/identity.rs`: they do not leave `core/src/path/`, and
  outside it every caller asks `RealPath::contains` or `Excluded::matches`, so
  no caller re-derives a matcher over whichever form it holds, as exarch's skill
  check once did.
- **The reading is chosen by the polarity of the type**: an allowance is a
  `GrantPath<Extent>` and has no identity to ask; an exclusion is a
  `GrantPath<Excluded>`.
- **`Path::{starts_with, strip_prefix, ancestors, components, parent}` are
  banned outside `path/`** (`clippy.toml`), so the renderers and synod take
  `RealPath`'s operations, and a program's own layout takes
  `path::spelled_parent`; so are the inherent `Path` methods that forward to a
  syscall (`exists`, `is_file`, `metadata`, `read_dir`, `canonicalize`, …).
  `clippy.toml` bans the raw `rustix`/`libc`/Win32 wrappers the walk is made
  of too, resolving aliases and imports, in `synod` as in the rest.
- **The one exemption is a typed site.** `site!` (`lint/site.rs`) lifts a ban
  for exactly one call, and its first argument is a `Surface` or `Silent`
  variant that names the place and documents why it is exempt; `#[expect]`
  makes a site that wraps no banned call an error. One lint serves every ban, so
  a hand-written allow over a syscall would silence it as readily as the
  spelling work it was written for: `core/tests/sites.rs` fails on any
  occurrence of `clippy::disallowed_methods` that is neither a site nor one of
  the blankets for test, example and build code (nor the one Tauri exception),
  on any `allow`, `expect` or `warn` of the `style` or `all` group, and on a
  workspace member that does not inherit the workspace's lints, and pins the
  callers of the string containment helpers outside `path/` at today's set ([[decisions/261022_typed-sites|typed-sites]]).

A guard must ask on the form its check matches. The one time it did not, the
`xdg-*` freeze guard asked containment of the surface while the check it guarded
matched the real form, and read `XDG_DATA_HOME=~/link`, `link → /etc`, as
contained inside `HOME` (fixed in `a5b0a525`); it now asks `RealPath::contains`
of the two walks' traces.

See also [[design/capability-carriers|capability-carriers]],
[[design/two-enforcers|two-enforcers]],
[[map/core/capabilities|map: capabilities]].
