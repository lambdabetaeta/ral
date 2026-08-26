---
verified_at_commit: 6d48e9af
verified_at_date: 2026-08-26
anchors: [HandlerStack, lookup, HandlerLookup, install_base, strip_matched, restore_matched, apply_handler, render_handler_args, Frame::Unmask, WithinUndo]
---

# Handler dispatch: deep, self-masking, by frame

[[design/effects-handlers|The effects design]] states the discipline — handlers
are deep, self-masking, and there is no `resume`. This is the runtime mechanism
that realises it, in `HandlerStack` (`types/handler.rs`) and the dispatch in
`runtime/command_call.rs`.

**The stack is what a head reaches when the env does not hold the name.**
Resolution is env → handlers → external, so a lexical binding — the prelude and
the [[design/builtins|natives]] among them — wins at a bare head, and the
handler stack is consulted for every other name. Shadowing, not admission, is
the interception discipline: a handler installs under any name, and `^name`,
which skips the env by definition, reaches it
([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]).

**The stack has two layers.** Above are the *run frames* pushed by `within` and
`handle`; below is a permanent *base layer* holding the *base-frame manifest*'s
rows — `echo` and `detach`, each variadic over a list of strings
(`install_base`). Permanence is representational:
the mutators (`strip_matched`, `remove_alias`) index the run frames alone, so no
operation can name a base frame. The base layer never crosses the wire — the
wire form is a `Vec<HandlerFrame>` — so a receiving shell's own boot installs
its own.

**Lookup is per-name first, catch-all last.** `HandlerStack::lookup(name)` scans
run frames innermost-first for an explicit per-name entry, then the base layer,
and only then makes a second innermost-first pass for the first catch-all
(`handler:`). So any per-name handler beats any catch-all whatever their relative
depth, and a catch-all never sees a base frame's name. The `HandlerLookup` it
returns names the two arm shapes: a run `Frame` with its `depth` — the count of
frames from the top down to and including the match — or a `Base` entry.

**Each arm runs honestly.** A run-frame hit applies the user thunk under the
argv-list convention below, masked by depth; a base hit calls the row's Rust
body directly with the argv slice, with no adapter and no masking, because a
Rust body never self-forwards (`run_base_frame`). A user frame therefore stacks
*above* a base frame and forwards into it by the same wrap-and-forward idiom
that reaches an outer handler.

**The calling convention is fixed by surface form, not value shape.** A per-name
entry (and every alias) is a unary lambda `{ |args| … }`, applied to the command's
argument list; a catch-all is a binary lambda `{ |name args| … }`, applied to the
name and the argument list. That list is a `List String` — `machine::render_handler_args` renders
every element through the total `to-string` as it packs the argv, so an arm
consumes what an exec call would
([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).
Dispatch invokes the matched entry under the convention its install site demands
rather than inspecting the stored value; a bare block, a non-lambda, or a
wrong-arity lambda is rejected at install time (`docs/SPEC.md` §9.4), so no
malformed entry ever reaches lookup.

**Self-masking is a strip-and-restore of one frame.** For the dynamic extent of
the matched handler's body, `strip_matched(depth)` lifts *only that frame* off
the stack and the machine holds it in a `Frame::Unmask` — so the restore is a
continuation frame, run on the returning path, the halting path, and the
abandon path alike. Frames newer or older than it stay in place, so outer
handlers for *other* names remain visible, and a same-name call inside the body
reaches the next outer handler — or the OS — never itself. `restore_matched`
re-inserts the frame afterwards, using the monotonically allocated `FrameHandle` to find the position
that preserves the original ordering even if the body pushed frames of its own.
This is what makes the wrap-and-forward idiom
`within [handlers: [git: { |args| my-git ...$args }]] { … }` terminate rather than
diverge.

**Deep** means the frame persists across the body's whole extent, re-reached by
each successive operation — a consequence of the stack living on the dynamic
[[design/scoping|frame]] `Context` and not being consumed at first use. Frames
are pushed by `WithinScope::enter` (`evaluator/scope.rs`), whose `WithinUndo`
the machine carries in a `Frame::Within`; at an IPC boundary a
deserialised stack is rebuilt with fresh handles, since the wire format does not
carry them.

See also [[design/effects-handlers|effects-handlers]],
[[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]],
[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]];
map [[map/core/shell-state|shell-state]], [[map/core/runtime|runtime]].
`docs/SPEC.md` §9.4, §9.6.
