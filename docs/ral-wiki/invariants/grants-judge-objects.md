# Grants judge objects

**A grant prefix carries two forms, `surface` as written and `resolved` with
symlinks followed, and every authority is judged on `resolved`.** `fs`
authorises the objects the kernel touches; `exec` authorises the file a head
will execute ([[decisions/261004_exec-rules|exec-rules]]),
which is the object the kernel judges too. The surface is what the author
wrote, kept for rendering and display, never a door to authority.

`NormalizedPrefix` (`core/src/path/resolved.rs`) freezes both at one disk
consultation and offers containment on the resolved form alone:

- **fs — `path::covers` / `PrefixSet::covering`.** The access side is
  canonicalised (`canon.rs`) or walked symlink-free (`walk.rs`) before the
  question is asked, so a link planted inside a granted region is matched
  where it lands ([[decisions/260906_object-not-name|object-not-name]]).
- **exec — `RealPath::frozen`**, the one door from a prefix to the exec
  table: path and dir keys enter `ExecRules` as their resolved forms, judged
  against a head's `realpath(3)` by equality and by `RealPath::within`.
- **composition — `evicts`**, which drops an allow dir a deny dir decides,
  judges the same resolved forms the in-process guard does.

A resolved prefix means what the disk said at freeze: a granted `~/bin` that
is a link to `/opt/x/bin` grants `/opt/x/bin`. That is no re-aiming of the
author's intent — a head spelled under `~/bin` executes there, and so the
kernel has always judged it.

## How it is held

The rule is which form, so the only way to get it wrong is to be handed a
choice. Nothing outside `core/src/path/` is:

- `lex::path_within` and `lex::path_within_str`, the form-blind kernel, are
  `pub(super)`, so no caller elsewhere re-derives a matcher over whichever
  form it holds, as exarch's skill check once did.
- `surface_path` is private to `resolved.rs`; outside `path`, the surface
  leaves the type only as a `String`, for rendering.

A guard must ask on the form its check matches. The one time it did not, the
`xdg:` freeze guard asked containment of the surface while the check it guarded
matched the resolved form, and read `XDG_DATA_HOME=$HOME/link`, `link → /etc`, as
contained inside `$HOME` (fixed in `a5b0a525`).

See also [[design/capability-carriers|capability-carriers]],
[[design/two-enforcers|two-enforcers]],
[[map/core/capabilities|map: capabilities]].
