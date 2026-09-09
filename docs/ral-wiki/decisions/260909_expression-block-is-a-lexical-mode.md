---
status: active
---

# `$[…]` is a lexical mode, not a sublanguage

**Inside `$[…]` the spellings `<` `>` `<=` `>=` `!=` `&&` `||` lex as operator
words, exactly as `+` and `==` always did; the parser reads the body with one
Pratt loop whose operands are the ordinary atoms of the value grammar; the five
operator forms are plain `Ast` variants; and the block itself leaves no node.
There is no `Expr` type.** What `$[…]` contributes is the *mode* in which
operators can be written without colliding with the shell — nothing else.

## What was there

The old front end had a second expression language bolted beside the first.
`Ast::Expr(Box<Expr>)` wrapped an `Expr` enum whose leaves — integer, float,
Boolean, variable, indexed variable, force — duplicated `Ast`'s own, with a
separate elaborator arm for each. The Pratt parser admitted only those leaves,
so `$[$s == 'quit']` was a *parse* error although `==` is structural on every
value and `equal` already compared strings. Two of the operators could not
be lexed at all: `<` and `>` arrived as redirect tokens the Pratt parser
reinterpreted, `2>3` lexed `2>` as a file descriptor and earned a diagnostic
about spacing, and `&&`/`||` were fused from adjacent single-character tokens
by a post-pass. Meanwhile `!=`, `<=`, `>=` lexed as words *everywhere*, so
`echo a >= b` printed `a >= b` while §3.5 listed `>` among the word-enders.

## The decision

- **The lexer's whole context is the innermost open delimiter.** `DelimKind`
  gains `Expr`. A `[…]` makes newlines whitespace and `,` punctuation; a
  `$[…]` does the same and also turns the comparison and Boolean spellings
  into words, and makes the arithmetic characters `+ - * / % =` end a word,
  so `$[1+1]` is a sum with no spacing rule to remember — a numeral is read
  whole through its exponent sign, so `1.5e+3` survives. Everywhere else `>`
  begins a redirect, `1+1` is a word, and `&`, `&&`, `||` are refused by name
  (`spawn`, `;`, `?`). One stack, one predicate; the mode is restored by the
  next `{`, so `$[!{wc -l < f} > 1]` reads a file inside and compares outside.
- **Operands are atoms.** `parse_expr_operand` is `(…)`, prefix `-`, prefix
  `not`, or `parse_atom`. Which values `+` or `<` accept is the type system's
  question, as it already was for `$[$a + $b]` on two variables; the grammar
  merely stopped pretending otherwise. `$[$s == 'quit']`,
  `$[!{f}[k] + 1]`, `$[[1][0] * 2]` are now admitted. The price is that
  `$[x + 1]` — a dropped `$` — is `"x" + 1`; since a bare non-numeral word
  can never be a number, `numeric_operand` refuses it under `-`, arithmetic
  and ordering at parse time, asking whether `$x` was meant. `==` and `!=`
  are exempt: comparing to a bare-word string is legitimate.
- **No node for the block.** `Ast::Binary`, `Negate`, `Not`, `And`, `Or`
  carry `Spanned` `Ast` operands; `$[1.5]` *is* the numeral `1.5`. The
  elaborator lowers five arms where it lowered twelve, and `free_refs` walks
  one tree.

## Landing beside it

The same pass removed every token that existed only to be refused
(`Dollar`, `Ampersand`) — the lexer refuses the character with the message
the parser used to give — and made a splice inside `"…"` the token stream the
same text has outside the string, so `"!$d"` is `Force(Variable)` like `!$d`
rather than the synthesised `!{$d}` that interpolated a block. Un-fusing
`$name[k]` at the lexer exposed the one place fusion was load-bearing: `!`
must reach over a dereference's keys (`!$p[tail]` forces the field) while a
forced block is indexed after (`!{cmd}[k]`), and that rule now lives in
`parse_bang` as grammar rather than in the token shape.

**Amended.** "The token stream the same text has outside the string" was read
too far: the pass also let *every* splice swallow a following `[key]`, and
inside a string `[` is otherwise text, so `"$(red)[$host]"` indexed a colour
and no escape could stop it — `\[` is not an escape, and `"$(x)[oops"` lost
its closing quote to the bracket group. A splice now ends where its own
closing delimiter does; only the undelimited `$name` and `!$name` continue
into `[key]`, and `"$[!{f}[k]]"` says the rest explicitly. `$(name)` is again
what `docs/TUTORIAL.md` always called it: the mark for the end of a name.

## Rejected

- *Keep the redirect reinterpretation and add diagnostics.* It was already
  one special error for `2>`; every further glued spelling would want another.
- *Lex `&&`/`||` as words everywhere.* Outside `$[…]` `a && b` would then run
  `a` with two arguments — the bash reflex succeeding silently, which is the
  one outcome worse than a bad message.
- *Restrict operands to a "numeric" subset of atoms.* More grammar to say
  what the checker already says, and `==` on strings stays unwritable.
