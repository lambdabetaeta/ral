---
status: active
---

# REPL builtins live in the REPL layer

The line-editor builtins prefixed `_ed-*`, and the editor types they need, live
in the `ral` binary's REPL layer, not in `ral-core` (`f896dac`, 2026-05-14
moved them out of core; the `_ed-*` family has since grown there).

The REPL is a layer *above* the core engine. The core descriptor describes the
language and runtime; interactive editing concerns — clipboard, hyperlinks, line
editing — are a frontend matter and do not belong in the engine's builtin set.
Folding them into core would entangle the language definition with one
particular interactive frontend.

This keeps the boundary clean: `ral-core` knows nothing about the REPL, and the
REPL adds its builtins on top.

See also [[map/core|core]], the forthcoming [[map/repl|repl]], and the REPL design
direction in [[decisions/260522_repl-architecture|repl-architecture]].
