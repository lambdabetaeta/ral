# Redirects are the three standard streams

**ral models `<` and `<<` on stdin, `>`, `>>` and `>~` on stdout and on stderr,
and `2>&1`. The type says so: `Redirect<T>` (`core/src/syntax/ast.rs`) is the
sum `Stdin(StdinSource<T>) | Stdout(WriteMode, T) | Stderr(WriteMode, T) |
StderrToStdout`, one type for all three tiers, so a redirect outside the set is
not a value of any tier rather than a value checked again downstream.**

ral has no fd plumbing. There is no `exec 3< file`, no `{fd}>`, no `dup2` on a
user's behalf: stdout and stderr are routed through the shell's own `Sink`s,
never the process-global descriptors, because libtest, the REPL frontend and
sibling ral threads all share those ([[internals/evaluator-machine|evaluator-machine]]).
An fd number is therefore not a general mechanism with three supported cases;
three streams are the whole of it, and an fd prefix in the source only *names*
one.

**The fd number is spelling, eliminated at the parser.** The lexer's tokens
keep what was written — `Token::Redirect { fd, op }` for a word-taking
operator, `Token::Dup { fd, to }` for `fd>&to` — and `Redirect::word` and
`Redirect::dup` eliminate them into the sum, refusing with a message any fd
that names no stream (`1< f`, `0> f`, `3> f`, `1>&2`). The identity dups `1>&1`
and `2>&2` name the stream they already are, so they denote no redirect:
`dup` returns `None` and nothing is built.

**Every tier is the same sum over a different operand**: the parsed word
(`Redirect<Ast>`), the elaborated value (`Redirect<Val>`, inside the IR), the
evaluated string (`Redirect<String>`, at the runtime). `map` and `try_map` carry
one tier to the next, so no tier re-widens the type and every consumer — the
in-process redirect frame (`core/src/evaluator/redirect.rs`), the external
command's stdio plan (`core/src/runtime/command/stdio.rs`), the write
observation (`core/src/types/observation.rs`) — is an exhaustive case with no
unreachable arm. `WriteMode` is a write mode and nothing else, so a write
observation cannot carry a read mode.

**This closes the wire as well.** `Comp` crosses the wire (`core/src/serial.rs`)
and derives `Deserialize`; since the IR's redirect is the sum, a peer cannot
hand this process a redirect that names fd 7 — it fails to decode instead of
reaching a runtime check.

The lexer refuses two of the excluded forms earlier than the parser, and
should: `1>&2` earns advice about `warn` and `2>&1` that the fd rule has no
way to give, and fd ≥ 3 earns the sentence naming the three streams. Those are
better diagnostics for the same rule, not a second gate.

This is a hard rule. Do not reintroduce an fd number below the lexer's tokens,
and do not widen the sum without giving the new form plumbing that means
something.

See [[internals/surface-syntax|surface-syntax]] for where redirects are lexed
and parsed, [[design/capture|capture]] for why a redirect moves `ambient` with
`stdout` (and why an identity dup must *not*), and
[[decisions/260526_redirect-drop-on-handler-dispatch|redirect-drop-on-handler-dispatch]]
for what a redirect does when the head turns out to be handled.
