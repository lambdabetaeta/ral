# Local edtui patch

Upstream: **edtui 0.11.7**, https://github.com/preiter93/edtui,
from the crates.io source distribution. Author: preiter (Philipp Reiter).
The upstream MIT license and copyright remain in `LICENSE.md`.

This directory contains production Rust source, the crate manifest, license,
and README only. Inline unit tests remain with their source. Examples,
resources, binary theme assets, release tooling and the upstream lockfile are
not vendored. `../rustfmt.toml` intentionally disables bulk formatting.
Modified upstream files contain `LOCAL PATCH` markers; new source files carry
the same marker. Upstream README image links refer to omitted resources.

## History and editing

- `state/undo.rs`: one native history owns undo and redo, with combined step
  and allocated-snapshot byte limits (100 steps / 8 MiB by default). Public
  `capture`, `transaction`, `insert_transaction`, `set_history_limits` and
  `clear_history` let hosts group edits without reconstructing the editor. Transactions
  suppress nested captures, ignore unchanged text, and invalidate redo only on
  divergent text edits. Empty insert sessions defer their undo capture. History
  traversal during an unfinished insert abandons that repeat recording; normal
  undo after leaving insert preserves the completed repeat command.
- `state.rs`, `actions.rs`, `actions/{insert,cpaste}.rs`, `events/paste.rs`:
  insert repeat records actions, including relative custom replacements and
  cursor movements. Public `replace_range` (byte offsets) and `move_to` (scalar
  coordinates) preserve exact edit intent for `ciw`/dot replay, even when the
  replacement matches adjacent text. No edit location is inferred by diffing
  snapshots. `insert_transaction` only groups undo; recording uses these explicit
  primitives or native actions. Atomic undo grouping and repeat are independent.
  Native insert paste records immutable text, not the future clipboard contents.
  Native action execution snaps cursor boundaries for live edits and replay alike.
- `state/selection.rs`: characterwise extraction removes exactly the projected
  byte range, preserving unselected newlines even when a whole row's text is
  removed. Linewise extraction retains its separate row-removal semantics.
- `events/key.rs`, `events/mod.rs`: key and paste edits use native transactions.
  Pending key resolution stays in the native parser. Hosts can inspect pending
  state or clear it for an external replacement. Undo/redo dispatch is not itself
  captured as an edit.
- `actions/delete.rs`: native `r` consumes a pending character argument, like
  find/change operators; `ru` is a replacement, not an undo command.

Transaction closures are text/cursor edits, not undo/redo traversal. Direct
mutation of public `lines` must use `capture` or a transaction to be undoable.
Direct low-level `Execute` retains upstream capture behavior; key dispatch and
host transactions provide grouped, no-op-aware history semantics.

## Geometry

`view/geometry.rs` supplies one scalar-indexed, extended-grapheme layout for
rendering, height measurement, cursor positions and mouse hit-testing. It
replaces `view/line_wrapper.rs`; `view.rs`, `view/{render_line,internal}.rs`,
`state/view.rs`, `events/mouse.rs`, `helper.rs` and the export in `lib.rs` use it.
Clusters wrap indivisibly; overwide clusters use a replacement glyph in narrow
viewports. Tabs retain upstream fixed-width expansion. Viewport cropping and
scroll offsets respect cluster boundaries. Native and host regressions cover
combining text, emoji, flags, narrow viewports, cursor rows and mouse hits.

This is not a conversion of all vi operators to grapheme semantics: internal
storage and vi operation counts still use Unicode scalar columns. The host
provides grapheme-aware insert-mode movement, selection and deletion using
native edit transactions.

## Production-only manifest

`Cargo.toml` removes targets for omitted examples, retains the ratatui test
dependency, adds `unicode-segmentation`, and uses the workspace's Rust 2024 edition.
`view/syntax_higlighting.rs` uses syntect's built-in theme set rather than an
omitted binary theme dump; optional syntax-highlighting callers must use those
names or supply a custom set. The README and crate example name a built-in theme.

## License boundary

Changes in this directory are provided under the upstream MIT license. No
OpenAI Codex implementation is included here. The host's separately attributed
kill/yank and insert-boundary policies remain outside this directory in exarch;
they are not relicensed by this vendoring patch.

## Integration

At the workspace root (not changed by this patch):

```toml
[patch.crates-io]
edtui = { path = "vendor/edtui" }
```

The existing edtui version requirement accepts 0.11.7. No direct path dependency
on edtui-jagged or other crate is required.
