# fs judges objects, exec judges names

**A grant prefix carries two forms, and which one an authority is judged on is
fixed by what that authority is *about*: `fs` authorises objects, so it is
judged on the symlink-followed `resolved` form; `exec` authorises names, so it
is judged on the `surface` form the author wrote — which a deny, since a veto
may over-reach but never under-reach, widens to `resolved` as well.** Neither
form is the "real" one and the other a spelling of it.

`NormalizedPrefix` (`core/src/path/resolved.rs`) freezes both at one disk
consultation, and offers exactly two containment doors, one per authority:

- **fs — `path::covers` / `PrefixSet::covering`, on `resolved`.** An `fs` grant
  is a ceiling on what the kernel may touch, and the kernel touches an object.
  The access side is canonicalised (`canon.rs`) or walked symlink-free
  (`walk.rs`) before the question is asked, so a link planted inside a granted
  region is matched where it lands, not where it sits
  ([[decisions/260906_object-not-name|object-not-name]],
  [[design/capability-freeze|capability-freeze]] §"what freeze does *not*
  resolve").
- **exec — `NormalizedPrefix::grant_depth`, on `surface`.** The question
  `capability::exec::longest_dir_match` asks is *what command did the user
  name?* — the question `runtime::command::identity` is built to answer. An
  `allow_dirs` prefix is therefore compared as written.

Exec does not thereby launder a symlink, because a veto widens where a grant
does not. On the candidate side, an allow dir sees only the narrow spellings a
head earns and a deny dir additionally sees the canonical one, so a binary
symlinked out of a denied directory into an allowed one is still vetoed
([[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]).
On the prefix side, `NormalizedPrefix::veto_depth` matches a deny dir on its
resolved form as well as its surface, the deeper winning, so a denied directory
that is itself a symlink vetoes where it points — as a deny literal, which is
canonicalised, already did. Asking an *allow* prefix on its resolved form would
silently re-aim a written grant at whatever the disk happened to say at freeze;
a deny may over-reach, never under-reach, so the widening is safe on that side
alone.

## How it is held

The distinction is a rule about which form, so the only way to get it wrong is
to be handed a choice. Nothing outside `core/src/path/` is:

- `lex::path_within` and `lex::path_within_str` — the one form-blind kernel —
  are `pub(super)`. A caller elsewhere in the crate cannot reach them, so it
  cannot re-derive a matcher over whichever form it happens to hold, as exarch's
  skill gate once did.
- `surface_path` is private to `resolved.rs` and `resolved_path` is
  `pub(super)`; outside `path`, the surface leaves the type only as a `String`
  (`as_str`, `into_string`), for rendering the OS profile.
- `evicts`, the composition sweep that drops an allow dir a deny dir decides,
  is mutual containment of the allow's surface with either of the deny's forms
  — it exists to mirror the exec gate, so it judges the forms that gate judges.

The half of this that was merely documented cost a reproduced escape: the
`xdg:` freeze guard asked containment of the surface while the gate it guarded
matched the resolved form, and read `XDG_DATA_HOME=$HOME/link`, `link → /etc`,
as contained inside `$HOME` (fixed in `a5b0a525`). The lesson is not "always use
the resolved form" — the exec gate is right to use the surface — but that a
guard must ask on the form its gate matches, which is decided per authority and
in one place.

See also [[design/capability-carriers|capability-carriers]],
[[design/two-enforcers|two-enforcers]],
[[map/core/capabilities|map: capabilities]].
