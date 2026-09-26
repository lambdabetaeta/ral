---
verified_at_commit: 703628d7
verified_at_date: 2026-09-10
anchors: [lex, parse, Head, DelimKind, scan_token_group, scan_splice, WordLiteral::classify, is_bare_word]
---

# Surface syntax: lexing and parsing

The front of the [[internals/compilation-ladder|ladder]]. `core/src/syntax/` is
the only code that sees raw bytes and bare words.

**The lexer's only context is the innermost open delimiter.** `lex(source)`
produces `Vec<(Token, Span)>` in one pass; the delimiter stack (`DelimKind`)
picks among three modes and nothing else does. In a `{…}` block newlines
separate statements. In a `[…]` collection newlines are whitespace and `,`
punctuates. `$[…]` is a bracket in which, additionally, the comparison and
Boolean spellings `<` `>` `<=` `>=` `!=` `&&` `||` are operator *words* and the
arithmetic characters `+ - * / % =` end a word (a numeral is read whole, sign
of exponent included) — so `$[2>3]` is a comparison, `$[1+1]` a sum, and
`$[!{wc -l < f} > 1]` reads a file inside its `!{…}` and compares outside it.
Everywhere else those characters keep their shell meaning: `1+1` is a word,
`>` begins a redirect, `&` and `&&` and `||` are refused by name (`spawn`, `;`,
`?`). This is one stack and one predicate, not the separate
arithmetic / test / glob lexers a POSIX shell inherits
([[decisions/260909_expression-block-is-a-lexical-mode|expression-block-is-a-lexical-mode]]).
The two sigils stay lexically distinct: `$name` is a value dereference, a bare
word in head position is a command token. A bare `$name` never absorbs a
*trailing* `-` even though `-` is a name character, so `"$os-$arch"` splits
into two derefs around a literal `-`; `$(name)` is the explicit interpolation
boundary and keeps such a dash.

**A splice in `"…"` is the tokens it would be outside the string.**
`scan_splice` lexes `$name`, `$(name)`, `$[…]`, `!{…}`, `!$name` in place and
stores the stream in `StringPart::Splice`; the parser reads it with the
ordinary `parse_atom`. So `"!$d"` is the same `Force(Variable)` as `!$d`.
Outside a string nothing is fused: `$xs[0]` is a variable followed by a
bracket group, and `parse_atom` reads the adjacency, as it does for `!{f}[k]`.

**A splice ends where its delimiter does.** Inside a string `[` is otherwise
text, so what a splice may swallow is decided by how it closes: `$(name)`,
`$[…]` and `!{…}` end at their own `)`, `]`, `}`, and the `[` after one is
text — `"$(red)[$host]"` is a colour and a bracketed host. Only the
undelimited `$name` and `!$name` have nothing to end them, so `scan_splice`
lets those two continue into adjacent `[key]` groups; `"$[!{f}[k]]"` indexes
a delimited form explicitly. This is why the string and the bare text
disagree for `"$(h)[file]"` alone: outside a string brackets are structure,
inside one they are prose, and only a form that closes itself can tell them
apart.

**A word's *literal* shape is lexical too.** `WordLiteral::classify` (`ast.rs`)
reads a bare word and nothing else — no expected type, no scope, no head — so a
numeral denotes its number wherever it stands and a token meaning bytes is
quoted ([[invariants/numerals-denote-numbers|numerals-denote-numbers]]). Both the
parser (skipping the `Call` wrapper for a value head) and elaboration (through
`Val::from_word`) read that one answer. Its dual is `quote.rs`'s
`is_bare_word`: whatever the numeral grammar claims cannot be emitted bare, or
printed text would come back as a value. `is_bare_word` *lexes* rather than
scanning characters, so it inherits `is_bare_char` and the positional splits
together — which is why a metacharacter added to `is_bare_char` also starts
being quoted on the way out, and why `&` (added when `echo hi&` was found to
lex as one word ending in `&`) makes `http://h/?a=1&b=2` an emitted `'…'`.
The remaining literals are
*punctuation*, not words — `()` for unit beside `[]` and `[:]` — so no
spelling of a name can collide with them.

**Parsing is recursive descent with a Pratt core for `$[…]`.**
`parse(source)` returns `Vec<Stmt>` or a `ParseError`. Statement and pipeline
productions descend recursively (`parse_stmt`, `parse_pipeline`, `parse_primary`)
under a depth counter that fails cleanly on pathological nesting rather than
overflowing the host stack. The body of `$[…]` is one Pratt parser by binding
power (`parse_expr_prec`), not bash's partitioned `(( ))` / `[[ ]]`, and its
operands are the ordinary atoms of the value grammar: `$[$s == 'quit']` is
admitted by the parser and judged by the checker. The one judgement the parser
keeps is the one that needs no types: a bare non-numeral word is a string, so
under `-`, arithmetic or ordering it can never typecheck, and `numeric_operand`
refuses `$[x + 1]` asking whether `$x` was meant — the old sublanguage's best
diagnostic, kept without its leaf grammar. The five operator forms —
`Binary`, `Negate`, `Not`, `And`, `Or` — are `Ast` variants like any other;
there is no `Expr` type, and `$[…]` leaves no node behind.

**Newlines bend around a continuation, on both sides of it.** A trailing `|`
or `?` promises a stage or a branch, so the parser skips the newlines after it
and the REPL's continuation prompt is telling the truth when it asks for the
next line (`needs_continuation` runs the real parser and reads the `ParseError`'s
own `incomplete` verdict; `join_continuation` folds lines in with `'\n'`). A
newline *before* `?` is allowed too, so `cmd\n? fallback` and `cmd ?\nfallback`
both parse. A `;` never continues anything.

**Redirects are the three standard streams.** `parse_redirect` eliminates the
lexer's `Redirect` and `Dup` tokens, fd numbers and all, into the sum
`Redirect<Ast>` through `Redirect::word` and `Redirect::dup`, which refuse any
fd that names no stream — so no fd number exists anywhere downstream
([[invariants/redirects-are-the-three-standard-streams|redirects-are-the-three-standard-streams]]).

**The AST is flat by decision.** `Ast` (expressions) and `Stmt` are wide flat
enums ([[decisions/260530_ast-stays-flat|ast-stays-flat]]); no desugaring happens
here. The surface forms are preserved verbatim for the
[[map/core/elaboration|elaborator]], the one phase that lowers them.

**Head classification refines in three stages, each deciding a different
question.** The parser fixes the *syntactic shape* of each command head
(`ast.rs`: `ExternalName` for `^name`, `Path` / `TildePath` for `./x` / `~/x`,
`Bare` for a lexical-lookup-or-`Exec` word). The
[[map/core/elaboration|elaborator]] resolves a `Bare` head against the lexical
scope — a bound name lowers to CBPV application, an unbound one to `Exec`
(forward declarations cover only the thunk-form bindings `group.rs` knots, so
the shadow agrees with what evaluation will see). A name-dispatched `Exec`
then resolves binding → handler → PATH at *evaluation* time
(`ir.rs` `CommandWord`), so handler installation and PATH state are read when
the command runs, not when it was parsed. The five reserved
[[design/control-operators|control operators]] are recognised at the parser
layer and everything else is a library binding. `group.rs` is a pre-pass
marking mutually recursive binding groups for the elaborator.

See also [[internals/compilation-ladder|compilation-ladder]]; map
[[map/core/syntax|syntax]]. Grammar: `docs/SPEC.md` §3, §17.1; RATIONALE
§"One form, one meaning".
