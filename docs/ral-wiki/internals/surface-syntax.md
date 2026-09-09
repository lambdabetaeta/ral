---
verified_at_commit: dfe6e55c
verified_at_date: 2026-09-09
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
`scan_splice` lexes `$name`, `$(name)`, `$[…]`, `!{…}`, `!$name` and any
adjacent `[key]` groups in place and stores the stream in
`StringPart::Splice`; the parser reads it with the ordinary `parse_atom`. So
`"!$d"` is the same `Force(Variable)` as `!$d`, and `"$(h)[file]"` indexes as
`$(h)[file]` does. Outside a string nothing is fused: `$xs[0]` is a variable
followed by a bracket group, and `parse_atom` reads the adjacency, as it does
for `!{f}[k]`.

**A word's *literal* shape is lexical too.** `WordLiteral::classify` (`ast.rs`)
reads a bare word and nothing else — no expected type, no scope, no head — so a
numeral denotes its number wherever it stands and a token meaning bytes is
quoted ([[invariants/numerals-denote-numbers|numerals-denote-numbers]]). Both the
parser (skipping the `Call` wrapper for a value head) and elaboration (through
`Val::from_word`) read that one answer. Its dual is `quote.rs`'s
`is_bare_word`: whatever the numeral grammar claims cannot be emitted bare, or
printed text would come back as a value. The remaining literals are
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
