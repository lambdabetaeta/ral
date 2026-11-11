---
verified_at_commit: becc34179cf764c11ef623c65e88f8f3d1006b57
verified_at_date: 2026-11-11
anchors: [HandlerStack, lookup, HandlerLookup, HandlerStack::over, natives_and_base, strip_matched, restore_matched, step_exec, BuiltinEntry::framed, render_handler_args, Frame::Unmask, WithinUndo, WithinScope::parse, HandlerEntry::vet, arm_stands_in]
---

# Handler dispatch: deep, self-masking, by frame

[[design/effects-handlers|The effects design]] states the discipline — handlers
are deep, self-masking, and there is no `resume`. This is the runtime mechanism
that realises it, in `HandlerStack` (`shell/runtime/handler.rs`) and the dispatch in
`shell/exec/resolve.rs`.

**The stack is what a head reaches when the env does not hold the name.**
Resolution is env → handlers → external, so a lexical binding — the prelude and
the [[design/builtins|natives]] among them — wins at a bare head, and the
handler stack is consulted for every other name. Shadowing, not admission, is
the interception discipline: a handler installs under any name and intercepts
it at the bare head only; `^name`, like a path head, skips the env and the
handler stack alike and reaches the external command directly
([[design/builtins|builtins]]).

**The stack has two layers.** Above are scoped frames pushed by `within` and
persistent frames installed by the host between units from rc or manifest
`handlers:` map; below is a permanent *base layer* holding the
*base-frame manifest*'s rows — `echo` and any host-supplied rows, each variadic
over a list of strings (`HandlerStack::over`, over the rows `natives_and_base`
selects). `detach` is a native value, reached before this stack; handlers
cannot intercept it. Its program lookup also bypasses the whole stack, as
`^program` does. Permanence is representational:
the mutators index the run frames alone, so no stack mutation removes a base
frame. No statement or builtin value mutates persistent frames.
The base layer never crosses the wire — the wire form is a `Vec<HandlerFrame>` — so a receiving shell's own boot installs
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
Rust body never self-forwards (`step_exec`'s `Base` arm). A user frame therefore stacks
*above* a base frame and forwards into it by the same wrap-and-forward idiom
that reaches an outer handler.

**The calling convention is fixed by surface position, not value shape.** A
per-name entry takes one parameter, the command's argument
list, as `{ |args| … }` does; a catch-all takes two, the name and the argument
list, as `{ |name args| … }` does. That list is a `List String` — `machine::render_handler_args` renders
every element through the total `to-string` as it packs the argv, so an arm
consumes what an exec call would
([[design/exec|exec]]). Every base frame has `[String] → Command`; stored
named arms have `U([String] → Command)`, and catch-alls
`U(String → [String] → Command)`. No handler result depends on its name or Σ.
Dispatch invokes the matched entry under the convention its install site demands
rather than inspecting the stored value; an arm whose type takes other
parameters, or a value that is no block, is refused statically in `within`
(`docs/SPEC.md` §9.4). `WithinScope::parse` consumes the checked option
record, builds per-name entries from its `handlers:` map and takes `handler:`
as checked; neither is re-vetted. Naming either option installs a frame, even
for `handlers: [:]`; naming neither leaves the stack alone. Persistent rc and
manifest handlers retain install-time `arm_stands_in`, which returns only a
verdict: no handler scheme is persisted or shipped. The checker imports
handled names for diagnostics alone; Σ and lexical bindings determine the
head's type and argument gate, with the external gate for every non-base
command name even when handled.

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
`within [handlers: [:, git: { |args| my-git ...$args }]] { … }` terminate rather than
diverge.

**Deep** means the frame persists across the body's whole extent, re-reached by
each successive operation — a consequence of the stack living on the dynamic
[[design/scoping|frame]] `Context` and not being consumed at first use. Frames
are pushed by `WithinScope::enter` (`shell/evaluator/scope.rs`), whose `WithinUndo`
the machine carries in a `Frame::Within`; at an IPC boundary a
deserialised stack is rebuilt with fresh handles, since the wire format does not
carry them.

See also [[design/effects-handlers|effects-handlers]],
[[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]],
[[design/builtins|builtins]];
map [[map/core/shell-state|shell-state]], [[map/core/runtime|runtime]].
`docs/SPEC.md` §9.4, §9.6.
