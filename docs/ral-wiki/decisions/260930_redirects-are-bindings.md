---
status: accepted
generated_at_commit: 8d868e18
---

# Redirects are bindings

**A redirect list binds each of stdin, stdout and stderr at most once, and one
interpreter, `RedirectState`, serves every shape that carries one.** A stream
has one final destination, written once; nothing is opened only to be
overridden, and no position in the list means anything.

## Context

Two interpreters read one redirect list and disagreed: a compound block and a fused external gave `> a > b` and `2>&1 > f` different meanings. The language had no rule; it had two implementations.

## What was decided

- **A list is a set of bindings.** `Redirects<T>` holds `stdin`, `stdout` and
  `stderr` as options; `Redirects::bind` refuses a second binding of a stream
  at parse time, caret on the second redirect: *"standard output is redirected
  twice; which one do you mean?"*, and its input and error counterparts.
  `> a > b`, `2> e 2>&1`, `2>&1 2> e` and `< a << b` are not programs.
- **`2>&1` is position-free.** It binds stderr to stdout's destination, so
  `2>&1 > f` and `> f 2>&1` are one command.
- **One interpreter.** `RedirectState` (`runtime/redirect/scope.rs`) opens the
  targets and installs the sinks for a fused external and a `CompKind::Redirect`
  block alike; the IR keeps `Exec.redirects` and the redirect scope as two
  shapes, both read by it. Targets open in one fixed order — stdin, stdout,
  stderr — so the trail reads the same for both.
- **An external gets a `>` file as its own fd**, with no pump, and `2>&1`
  stays a `dup2` when stderr's sink is the same destination as stdout's
  (`Sink::same_destination`), so `let x = sh -c 'echo err >&2; echo out' 2>&1`
  is `"err\nout"`. `2>` streams and is never atomic.
- **A streaming write is `committed` at its open**, even if the body later
  fails; only an atomic `>` can be `aborted`.

  > **Amended 2026-10-06** by [[decisions/261006_a-file-change-is-one-fact|a-file-change-is-one-fact]]:
  > the stream is still `committed` whatever the body does, but it is reported
  > when the frame settles, so the report can carry what landed.
- **A pipeline stage with redirects is a thread stage** run through the same
  `RedirectState`; a direct stage has none, and `wire_stdio` wires its
  stage pipes without reading any redirect list.
- **Dup cycles cannot be written**: the lexer refuses `1>&2`, and an identity
  dup binds nothing.

## Alternatives considered

- **POSIX left-to-right fold.** Faithful, and the source of `2>&1 > f` versus
  `> f 2>&1` meaning different things for reasons a reader must simulate. It
  also opens `a` in `> a > b` to truncate it for nobody. The fold is shell
  machinery ral exists to be free of.
- **Last wins.** Total, so nothing is refused, but the dead redirect still
  costs an open with side effects (a truncation, an audit observation), and a
  typo that doubles a redirect is silently accepted.

## Consequences

- POSIX `cmd 2>&1 2>/dev/null` is refused; write `2>/dev/null`.
- A list's meaning is a function of its bindings, not of their order, and a
  block and an external read it the same way.
- The audit order of a write's observation is canonical, and a streaming write
  is observed at open whether the body succeeds or not.
- An external writing to a file is that file's fd: no pump thread stands
  between them.

See [[invariants/redirects-are-the-three-standard-streams|redirects-are-the-three-standard-streams]]
for the type, [[internals/surface-syntax|surface-syntax]] for where the refusal
is raised, and `docs/SPEC.md` §7.4 for the language statement.
