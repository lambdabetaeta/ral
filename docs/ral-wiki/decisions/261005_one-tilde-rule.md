---
status: accepted
generated_at_commit: 05420f58
verified_at_commit: 05420f58
anchors: [TildePath, scan_bare_word, scan_double_quoted, parse_head, reserved_register, apply_chdir]
---

# One tilde rule

**`~` abbreviates `$HOME`, the current user's home directory, at the start of
a word — a bare word or a double-quoted string — when it stands alone or is
followed by `/`. Anywhere else `~` is an ordinary character.** `'…'` and
`#'…'#` are verbatim, and `\~` joins the escape set so a double-quoted string
can begin with a literal `~`.

## Context

Each of these failed for a reason only the implementation knew:

- `ls "~/To process at work"` handed `ls` a literal `~`: the quotes a path
  with spaces needs were the quotes that switched `~` off.
- `let h = ~` tried to execute the home directory: a bare `~` was a path head
  wherever it stood.
- `let x = $[foo]` ran `foo`, and `let h = $[~]` executed the home directory:
  `$[…]` leaves no node behind, so the parser re-classified the word inside it
  as a head.
- `ls "$CWD/x"` failed under home: `$CWD` was `~`-abbreviated, so the string
  carried a `~` nothing expanded, and `cd` re-expanded `~` in string data to
  keep `cd $CWD` working.

## What was decided

- **A word and a string read `~` alike.** A leading `~` in a double-quoted
  string is a splice of the same store read a bare `~` is (`Register::Tilde`),
  followed by ordinary content, so `"~/x"` and `"$HOME/x"` are one and the
  same.
- **`$HOME` is a register.** A reserved pseudo-variable like `$USER`, it reads
  `HOME`, or `USERPROFILE` where appropriate, from the effective environment,
  so `within [env: [HOME: …]]` is honoured; `~` is its abbreviation.
  `let HOME = …` is refused like `let CWD = …`.
- **`~user` is gone.** `~bob/x` is an ordinary word, a relative path spelled
  `~bob/x`, and `foo~bar` is one word. Another user's home, via `getpwnam(3)`,
  was a timesharing convenience, unresolvable on Windows, and the reason
  `"~5 minutes"` could not be left literal under a uniform rule.
- **A value is not a command.** `~/bin/tool` is still a path head that runs the
  program. A bare `~`, a literal word (`42`, `true`) and a `$[…]` block are
  value heads wherever they stand: `let h = ~` binds the home directory, and
  `~ foo`, `42 foo`, `true foo` are the checker's T0011 (*"value of type …
  cannot be used as a command head"*), not "command not found".
- **`$CWD` is absolute.** It is the logical working directory, unabbreviated.
  Presentation is a builtin: `abbreviate-home <path>` folds a leading home
  directory to `~`, and a prompt wanting the short form writes
  `"!{abbreviate-home $CWD} ❯ "`.
- **`cd` resolves its argument as every path builtin does**, through the
  shell's Resolver: path-prefix sigils decoded, anchored against the effective
  cwd. Its private tilde rewrite is gone, as is `detach`'s.

## Two layers

Two readings of `~` remain, deliberately:

- *syntax* — `~` at a word's or a double-quoted string's start, a store read
  fixed by the parser;
- *data* — the path-prefix sigil language of policy entries, `glob` patterns
  and the Resolver, decoded where ral itself consumes a path string.

Externals always receive strings verbatim: `ls '~/x'` hands `ls` the bytes
`~/x`, while `cd '~/x'` goes home, because `cd` consumes its argument as a
path.

## Alternatives considered

- **Leave `"~"` literal, on the bash precedent.** It does not transfer. In bash
  double quotes mean *suppress expansion*, so refusing `"~"` is coherent there;
  ral's double quotes mean only that spaces do not separate and splices are
  allowed. With no splitting to suppress, a leading `~` losing its meaning
  inside them had no justification once the literal case has `'…'`.
- **Make `~` a value in head position too.** It loses `~/bin/tool` for nothing
  `$HOME` does not give.
- **Widen `$(…)` to `$(~)`.** `$(name)` is "a name, delimited"; a disjunction
  nobody would guess.
- **Leave `$ENV[HOME]` as the answer.** The most common environment datum,
  reached through a map index, Unix-only (Windows binds `USERPROFILE`), and
  learned by failing first.
- **Seed `$HOME` lexically.** [[decisions/260531_env-is-dynamic-only|env-is-dynamic-only]]
  removed that seed because it was a second copy that drifted. A register is a
  read of the one authority, so that decision's rationale stands while its
  surface sentence, *"a bare `$HOME` is now an undefined variable"*, is
  amended.

## What is deleted

- `~user`, and `get_user_home` with its fabricated `/home/<name>`;
- `Unexpandable`: an unset `HOME` is the one way `~` has no answer, so
  `TildePath::expand` answers `Option`;
- `cwd_string`, the abbreviating reader behind `$CWD`;
- `cd`'s and `detach`'s private tilde rewrites;
- the runtime hint for a `~` that "reached the command unexpanded", drafted and
  not landed: the only `~` an external now sees is one its author wrote as
  text.

See [[internals/surface-syntax|surface-syntax]] for the lexing and head
classification, and `docs/SPEC.md` §3.5, §4.2, §6.2 and §10.2 for the language
statement.

## Where

`core/src/path/tilde.rs` (`TildePath::parse`, `TildePath::expand`,
`abbreviate_home`), `core/src/syntax/lexer.rs` (`scan_bare_word`,
`scan_double_quoted`), `core/src/syntax/parser.rs` (`parse_head`),
`core/src/elaborator.rs` (`reserved_register`),
`core/src/evaluator/observe.rs`, `core/src/types/shell/cwd.rs`
(`apply_chdir`).
