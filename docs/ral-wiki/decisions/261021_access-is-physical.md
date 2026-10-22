---
status: active
verified_at_commit: f56fff48
anchors: [walk, walk_lenient, LexicalPath::folded, FrozenPath::from_surface]
---

# An access is physical; a grant prefix is a logical spelling

**An access is resolved as the kernel resolves it; a grant prefix is a logical
spelling, folded before it is resolved.** The two meet on their real forms.
This sharpens [[decisions/260906_object-not-name|object-not-name]]: the policy
side and the access side now resolve a name by one code path.

## Context

- The lenient canonicaliser kept a dangling link's own name while the walk
  resolved its target, so a deny on such a link did not hold for a write
  through it.
- Anchoring folded `..` lexically and dropped a trailing separator before the
  disk was asked, so `resolve-path 'link/../f'` and `> 'file/'` disagreed
  with the kernel; and the root, having no leaf, was not an object at all.

## Decision

- **Anchoring and folding are separate.** A `LexicalPath` is absolute and
  anchored, `.`, `..` and a trailing separator kept. `LexicalPath::folded` is
  the explicit fold, asked by the logical consumers: `cd`,
  `within [dir: …]`, `absolute-path`, a display base, the device predicates.
- **One physical resolution.** `walk` is the resolver: from the root, by
  directory handles, every link spliced, every `..` taken after the links
  before it, a trailing separator demanding a directory. Under
  `Missing::Refuse` it is an open; under `Missing::Tolerate`
  (`resolve_lenient`) it stops at the first name it cannot enter and appends
  the rest lexically. The lenient canonicaliser is the second mode.
- **Grant prefixes are logical.** `FrozenPath::mint` folds the surface, then
  resolves it: `` `cwd link/../x `` names `<cwd>/x`.
- **A directory is an object.** The root, a name ending in a separator and a
  trailing `..` locate the directory through its own handle (leaf `.`), so
  every query answers for `/`, and a write to one fails with `EISDIR`.

## Rejected

- *Folding on the access side*: it answers for a name the kernel never reads.
- *The ancestor walk over `realpath(3)`*: a second resolver, which disagreed
  with the first on exactly the names an attacker chooses.

## Where

- **`core/src/path/walk.rs`** — `walk`, `walk_lenient`, the directory-itself
  `Located`.
- **`core/src/path/forms.rs`** — `LexicalPath::folded`, `FrozenPath::from_surface`.

**Amended 2026-10-22 by [[decisions/261021_one-path|one-path]].** The walk is
the only resolver: the lenient canonicaliser is gone, and its mode is
`walk_lenient`, which returns the transcript of a walk that stops where the disk
does. A `FrozenPath` is a spelling and nothing else; it is folded and shown, and
what it names is the walk's to say, once, when the layer is decoded. A grant
prefix is therefore resolved by the same code as an access, as before, but it
is resolved at decode and kept (`GrantPath { written, real }`), never again.
