# Redirects are the three standard streams

**ral models `0<`, `<<`, `1>` (with `>>` and `>~`), `2>` and `2>&1`, and the
identity dups `1>&1` and `2>&2`, which mean nothing. Every other fd form is
refused at the surface. The set is declared in one place — `Redirect::new`
(`core/src/syntax/ast.rs`) — and it is a *constructor*, so a redirect outside
the set is unbuildable rather than checked again downstream.**

ral has no fd plumbing. There is no `exec 3< file`, no `{fd}>`, no `dup2` on a
user's behalf: fds 1 and 2 are routed through the shell's own `Sink`s, never
the process-global descriptors, because libtest, the REPL frontend and sibling
ral threads all share those ([[internals/evaluator-machine|evaluator-machine]]).
An fd prefix is therefore not a general mechanism with three supported cases;
three doors are the whole of it, and the prefix only *names* one.

**A spelling bash gives a meaning ral cannot honour is refused, not
reinterpreted.** This is the same divergence rule that refuses `&&`, `||` and a
trailing `&`, applied to descriptors, and it is what makes `1>&1` and `1<`
different answers to one question rather than an inconsistency:

- `1>&1` means in bash exactly what it would mean in ral — nothing — so it
  stays legal and does nothing.
- `1< f` means something in bash that ral cannot do at all, so it is a parse
  error rather than a silent write door.

**Refusal is at construction — at the surface.** `Redirect`'s fields are
private and `Redirect::new` is its only constructor, so every `RedirectV` a
ral program's own source can produce descends from an admitted form. The two
places downstream that trust the model at that layer — the in-process
redirect frame (`core/src/evaluator/redirect.rs`) and the write observation an
audit trail carries (`core/src/types/observation.rs`) — then *state* it
instead of re-deriving it. That is the difference this rule exists to keep: a
second derivation is how `1< f` came to be filed as a write door still
carrying the read mode, and to panic the interpreter the moment an audit trail
or a surface sink was listening.

The lexer refuses two of the excluded forms earlier than the constructor, and
should: `1>&2` earns advice about `warn` and `2>&1` that the fd rule has no
way to give, and fd ≥ 3 earns the sentence naming the three streams. Those are
better diagnostics for the same rule, not a second gate — the constructor
still states the model whole.

**The IR is not closed the same way, and that is why one runtime check
stays.** `ir::RedirectV` re-widens the surface's closed type back to a raw
`{ fd: u32, mode, target }` product and derives `Deserialize`; `Comp` — the
thunk value a `RedirectV` lives inside — crosses the wire (`core/src/serial.rs`),
so a peer can hand this process a `RedirectV` that never passed through
`Redirect::new` at all. `core/src/runtime/command/stdio.rs`'s
`unmodeled_redirect` (fd ≥ 3, or a `fd>&fd` dup other than `2>&1`) is the only
gate on that arrival — not a second check on the set `Redirect::new` already
closed, but the one check on the set it *didn't*, because it never saw that
value. Turning it into an `unreachable!` would turn a malformed peer redirect
into a peer-triggered panic instead of a refused command.

This is a hard rule, not a stylistic preference. Do not widen the admitted set
`Redirect::new` grants without giving the new form plumbing that means
something, and do not make `Redirect`'s fields public again — the point is
that the surface bug is unspellable. The runtime's wire-arrival check is not
part of that point and must stay: nothing upstream of it can vouch for a value
that crossed a `Deserialize` boundary instead of the constructor.

See [[internals/surface-syntax|surface-syntax]] for where redirects are lexed
and parsed, [[design/capture|capture]] for why a redirect moves `ambient` with
`stdout` (and why an identity dup must *not*), and
[[decisions/260526_redirect-drop-on-handler-dispatch|redirect-drop-on-handler-dispatch]]
for what a redirect does when the head turns out to be handled.
