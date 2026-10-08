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

**The fd number is spelling, eliminated at the lexer.** The vocabulary is
exactly nine spellings: `< f`, `<< str`, `> f`, `>> f`, `>~ f`, `2> f`,
`2>> f`, `2>~ f` and `2>&1`. A digit run glued to `>` or `<` is read whole and
judged; the token stream never carries an fd number. `Token::Redirect { stderr,
op }` is a word-taking operator, with `stderr` set only for the `2` writes (by
construction, so a stderr read cannot be built), and `Token::StderrToStdout` is
`2>&1`. The parser's `redirect_word` is then a total function of `(stderr, op)`
into the sum.

**A list of redirects is a set of bindings, one per stream.** `Redirects<T>`
(`core/src/syntax/ast.rs`) is the checked list: `stdin`, `stdout` and `stderr`
each hold at most one binding, so a stream has one final destination and
nothing is opened only to be overridden. `Redirects::bind` refuses the second
binding of a stream at parse time, with a caret on the second redirect and the
question *which one do you mean?* — `> a > b`, `2> e 2>&1`, `2>&1 2> e` and
`< a << b` are not programs. `StderrTarget` is `File(mode, t)` or `Stdout`, so
`2>&1` is a binding of stderr *to stdout's destination*, not a step that reads
a destination established so far: its position in the list means nothing. The
interpreter opens the targets in one fixed order, stdin, stdout, stderr
([[decisions/260930_redirects-are-bindings|redirects-are-bindings]]).

**Every tier is the same shape over a different operand**: the parsed word
(`Redirects<Ast>`), the elaborated value (`Redirects<Val>`, inside the IR), the
evaluated string (`Redirects<String>`, at the runtime). `map` and `try_map` carry
one tier to the next, so no tier re-widens the type and every consumer — the
in-process redirect frame (`core/src/runtime/redirect/scope.rs`), the external
command's stdio plan (`core/src/runtime/command/stdio.rs`), the write
observation (`core/src/types/audit/observation.rs`) — is an exhaustive case with no
unreachable arm. `WriteMode` is a write mode and nothing else, so a write
observation cannot carry a read mode.

**This closes the wire as well.** `Comp` crosses the wire (`core/src/seed/table.rs`)
and derives `Deserialize`; since the IR's redirect is the sum, a peer cannot
hand this process a redirect that names fd 7 — it fails to decode instead of
reaching a runtime check.

The lexer refuses every excluded form, each with its own advice: `0<` and
`0<<` (`<` already reads standard input), `1>`, `1>>` and `1>~` (`>` already
writes standard output; put a space before it to pass `1` as an argument, as
in `echo 1>file`), `1<`, `2<`, `1<<` and `2<<` (`<` always feeds stdin), `0>`
and friends (standard input cannot be written to), fd ≥ 3 (the sentence naming
the three streams), `1>&2` and `>&2` (use `warn`), the identity dups `1>&1` and
`2>&2` (they name the stream they already are), and any other `a>&b` (no fd
plumbing beyond `2>&1`).

This is a hard rule. Do not reintroduce an fd number below the lexer's tokens,
and do not widen the sum without giving the new form plumbing that means
something.

See [[internals/surface-syntax|surface-syntax]] for where redirects are lexed
and parsed, [[design/capture|capture]] for what a redirect does to a captured
command (`let saved = echo hi > f` binds `""`), and
[[decisions/260526_redirect-drop-on-handler-dispatch|redirect-drop-on-handler-dispatch]]
for what a redirect does when the head turns out to be handled.
