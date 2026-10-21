# Grants judge objects

**A grant prefix carries two forms, `surface` as written and `real` with
symlinks followed, and every authority is judged on `real`.** `fs`
authorises the objects the kernel touches; `exec` authorises the file a head
will execute ([[decisions/261004_exec-rules|exec-rules]]),
which is the object the kernel judges too. The surface is what the author
wrote, kept for rendering and display, never a door to authority.

`FrozenPath` (`core/src/path/forms.rs`) freezes both at one disk
consultation and offers containment on the real form alone:

- **fs — `FrozenPath::contains::<P>`**, to which a `Region`'s `Scope::holds` delegates.
  The access side is canonicalised (`canon.rs`) or walked symlink-free
  (`walk.rs`) before the question is asked, so a link planted inside a granted
  region is matched where it lands
  ([[decisions/260906_object-not-name|object-not-name]]).
- **exec — `FrozenPath::real`**, the one door from a prefix to its real
  form: path and dir keys enter `ExecRules` as their real forms, judged
  against a head's `realpath(3)` by `RealPath::within::<P>`, the rule's
  polarity choosing the identity.
- **identity — native.** Both forms are native paths: a name that is not
  Unicode is its own name under every identity, never the U+FFFD sibling it
  decodes to. An allowance whose real form is not Unicode is refused at
  freeze, since it is serialized and kept for good; an exclusion's live real
  form is kept byte-exact, and holds.
- **composition — `Table::meet`**, which joins exclusions and keeps an
  allowance where both layers support its own subject, judges the same real
  forms the in-process guard does. No sweep evicts an allowance: an exclusion
  beats it as evidence, never by deleting it.

An object can answer to several names — case and Unicode normalisation on
APFS, NTFS, ZFS and Linux casefold directories — and an absent one has no stored
spelling yet. So a **deny is judged by collision class and an allow by stored
name** ([[decisions/261006_denies-hold-under-every-spelling|denies-hold-under-every-spelling]]):
a rule of a `Table` speaks through `Scope::holds::<P>`, its evidence bit choosing
the `Polarity`, and `path` maps `Deny` to `Identity::Collision`, keyed by
`identity::collision_key`, and `Allow` to `Identity::Stored`. A deny then holds the
create that would make its object under another spelling, where an allow never
reaches a distinct name on a case-sensitive volume. Composition keeps the
split: exclusions join, allowances meet by stored name, and the meet writes a
negative bit only where a layer wrote one, since a default so written would hold every other
spelling of its name ([[design/authority-tables|authority-tables]]).

A resolved prefix means what the disk said at freeze, and an allowance keeps
that meaning for good: a granted `~/bin` that is a link to `/opt/x/bin` grants
`/opt/x/bin`, and a link planted at `~/bin` later redirects nothing. Only an
exclusion re-resolves, so it holds under whatever its name reaches now
([[decisions/261012_authority-is-belnap-evidence|authority-is-belnap-evidence]]). That is no re-aiming of the
author's intent — a head spelled under `~/bin` executes there, and so the
kernel has always judged it.

## How it is held

The rule is which form, so the only way to get it wrong is to be handed a
choice. Nothing outside `core/src/path/` is:

- `identity::path_within` and `identity::path_within_str`, the form-blind kernel, are
  `pub(super)`, so no caller elsewhere re-derives a matcher over whichever
  form it holds, as exarch's skill check once did.
- The identity is chosen by a rule's polarity, a type parameter: a table
  names `Allow` or `Deny`, never an `Identity`, so a deny has no exact
  question to ask.
- `surface_path` is private to `forms.rs`; outside `path`, the surface
  leaves the type only as a `String`, for rendering.

A guard must ask on the form its check matches. The one time it did not, the
`xdg-*` freeze guard asked containment of the surface while the check it guarded
matched the real form, and read `XDG_DATA_HOME=~/link`, `link → /etc`, as
contained inside `HOME` (fixed in `a5b0a525`).

See also [[design/capability-carriers|capability-carriers]],
[[design/two-enforcers|two-enforcers]],
[[map/core/capabilities|map: capabilities]].
