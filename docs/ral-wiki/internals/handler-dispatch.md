---
verified_at_commit: 7ba500b
verified_at_date: 2026-06-17
anchors: [HandlerStack, lookup, strip_matched, restore_matched]
---

# Handler dispatch: deep, self-masking, by frame

[[design/effects-handlers|The effects design]] states the discipline — handlers
are deep, self-masking, and there is no `resume`. This is the runtime mechanism
that realises it, in `HandlerStack` (`types/value.rs`) and the dispatch in
`runtime/command_call.rs`.

**Handlers intercept external command names only.** When a head resolves (via
[[internals/surface-syntax|classification]]) to neither a lexical binding nor a
builtin, `command_call` consults the handler stack before falling through to
external lookup. Lexical bindings, builtins, and the prelude are not shell
aliases and cannot be intercepted.

**Lookup is two innermost-first passes.** `HandlerStack::lookup(name)` scans all
frames innermost-first for an explicit per-name entry; only if none exists
anywhere does a second innermost-first pass take the first catch-all (`handler:`).
It returns the matched entry with a `depth` — the count of frames from the top
down to and including the match.

**Self-masking is a strip-and-restore of one frame.** For the dynamic extent of
the matched handler's body, `strip_matched(depth)` lifts *only that frame* off the
stack. Frames newer or older than it stay in place, so outer handlers for *other*
names remain visible, and a same-name call inside the body reaches the next outer
handler — or the OS — never itself. `restore_matched` re-inserts the frame
afterwards, using the monotonically allocated `FrameHandle` to find the position
that preserves the original ordering even if the body pushed frames of its own.
This is what makes the wrap-and-forward idiom
`within [handlers: [git: { |args| my-git ...$args }]] { … }` terminate rather than
diverge.

**Deep** means the frame persists across the body's whole extent, re-reached by
each successive operation — a consequence of the stack living on the dynamic
[[design/scoping|frame]] `Context` and not being consumed at first use. Frames
are pushed by the `within` guard (`evaluator/scope.rs`); at an IPC boundary a
deserialised stack is rebuilt with fresh handles, since the wire format does not
carry them.

See also [[design/effects-handlers|effects-handlers]],
[[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]]; map
[[map/core/shell-state|shell-state]]. `docs/SPEC.md` §3.2.
