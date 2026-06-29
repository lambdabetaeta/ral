---
status: active
date: 2026-06-29
---

# 260629 TUI modularisation

Split `exarch/src/tui.rs` from 3,889 mixed-frontend lines into a thin façade
over 13 focused modules, each owning one invariant with a narrow type interface.

## Motivation

The monolith mixed terminal lifecycle, session management, event buffering,
prompt editing, gesture/mouse state, model switching, render orchestration, and
the REPL loop into one file. Cross-cutting concerns were invisible; borrow
hygiene required whole-struct `&mut self`. Adding a feature meant touching
unrelated code.

## Decision

Follow the target shape in `dev/docs/260629_tui_modular.md`:

- **Diamond state pattern**: `App` is composition, not a warehouse. Four
  component structs (`Tabs`, `SurfaceBuffer`, `PromptState`, `GestureState`)
  own their state and expose narrow methods. App methods orchestrate across
  components.
- **Pure render projections**: `status.rs`, `matrix.rs`, `banner.rs` are
  pure functions of their input data — no App borrow.
- **One invariant per module**: `tabs.rs` owns focus invariants; `surface.rs`
  owns flush ordering; `prompt.rs` owns draft preservation; `gesture.rs` owns
  selection confinement.
- **Façade**: `tui.rs` declares modules, re-exports `run`, `Tui`, `SessionInfo`,
  and holds the `App` struct with its orchestration methods.

## Consequences

- 3,889 → 840 lines in `tui.rs` (78% reduction)
- 13 new modules, 17 total under `exarch/src/tui/`
- All 10 stated invariants verified satisfied by audit
- 300 existing tests pass unchanged
- Compilation is zero-error
- Visibility: `pub(super)` by default; `SessionInfo`, `run`, `Tui`, `CommandCtx`,
  `ReplControl`, `KeyMode`, `KeyAction` re-exported `pub`

## Outstanding

- `app.rs` extraction: `App` struct still lives in `tui.rs` (~550 lines);
  moving it to `tui/app.rs` would complete the file-level breakdown
- Accessor methods on `Tabs` (`born`, `died`, `focus_next`, `viewport`,
  `viewport_mut`, `ordered_viewports`) remain inline in `App` methods
- `PromptState` lacks `height_hint`/`render` delegation methods; callers
  reach through to `editor` field directly
- `SurfaceSink` trait from the spec not yet implemented
- Unit tests for new components not yet written
