---
verified_at_commit: 870865ed
verified_at_date: 2026-10-08
anchors: [lex, parse, Head, DelimKind, Operator, scan_token_group, scan_splice, end_unit, touches_after, eat_continuation, parse_control_op, collect_trailing_redirects, Ast::Redirected, LexErrorKind::Mismatched, WordLiteral::classify, is_bare_word]
---

# Surface syntax: lexing and parsing

The front of the [[internals/compilation-ladder|ladder]]. `core/src/syntax/` is
the only code that sees raw bytes and bare words.

**The lexer's only context is the innermost open delimiter.** `lex(source)`
produces `Vec<(Token, Span)>` in one pass; the delimiter stack (`DelimKind`)
picks among three modes and nothing else does. In a `{…}` block newlines
separate statements. In a `[…]` collection newlines are whitespace and `,`
punctuates. `$[…]` is a bracket in which, additionally, the comparison and
Boolean spellings `<` `>` `<=` `>=` `!=` `&&` `||` and the arithmetic
characters `+ - * / % =` are *operators*, a token of their own
(`Token::Op(Operator)`, a closed enum over `ir::BinaryOp` plus `And`, `Or` and
the lone `=`, which is an error wherever it stands), and end a word (a numeral
is read whole, sign of exponent included); an operator is never a word, which is
what lets the [[design/words|words rule]] tell `1+1` from touching atoms. So `$[2>3]` is a comparison, `$[1+1]` a sum, and
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

**A mismatched closer is a definite lexer error, not an incomplete input.**
`close_delim` meeting a closer that is not the innermost opener's returns
`LexErrorKind::Mismatched` (L0006, labelled at the closer with the opener as
its `also`), so the REPL never waits for a line that cannot repair it; a closer
at depth 0 still lexes and the parser reports it as unmatched. A leading BOM is
stripped when a source is loaded (`normalize_source_text`), before the line
endings fold. The lexer also refuses bash's reflexes by name: `${…}` at the
`$`, and `$(cmd args)`, which holds one name, pointing at `!{…}`.

**A splice in `"…"` is the tokens it would be outside the string.**
`scan_splice` lexes `$name`, `$(name)`, `$[…]`, `!{…}`, `!$name` in place and
stores the stream in `StringPart::Splice`; the parser reads it with the
ordinary `parse_atom`. So `"!$d"` is the same `Force(Variable)` as `!$d`.
A leading `~` before `/` or the closing quote is a splice too:
`scan_double_quoted` reads it as the bare `~` it would be outside, one read of
the home directory followed by ordinary text, so `"~/x"` is one string; `\~`
escapes it, and a literal string never splices.
Outside a string nothing is fused: `$xs[0]` is a variable followed by a
bracket group, and `parse_atom` reads the adjacency, as it does for `!{f}[k]`.

**`$(name)` ends its splice; every other splice takes keys.** Inside a
string `[` is otherwise text, so a splice must say whether the `[` after
it is its own: `$(name)` exists to mark the end of a name and takes
nothing, so `"$(red)[$host]"` is a colour and a bracketed host, while
`$name`, `!$name`, `!{…}` and `$[…]` continue into adjacent `[key]`
groups, and `scan_splice` lexes those keys into the splice. The same
rule holds outside a string: `parse_atom` declines a postfix for a
delimited name, so `$(red)[x]` is two words that touch
([[design/words|words]]), and `$red[x]` is the index.

**The words rule is one check at the end of a unit.** `Parser::end_unit(span)`
refuses the unit when `touches_after` finds an atom starting where it ended;
`parse_atom`, a keyword head (`try{a}{b}`, `return'x'`), a redirect's target,
a parenthesised group and `not` in `$[…]` each end their unit there, and
nowhere else, so the rule holds in every position, brackets and `$[…]`
included. The grammar's attachments (`^`, `...`, `!`, an index) are the
exceptions that must *touch*: `^ ls` is an error naming `^ls`, not a spacing.

**A word's *literal* shape is lexical too.** `WordLiteral::classify` (`ast.rs`)
reads a bare word and nothing else — no expected type, no scope, no head — so a
numeral denotes its number wherever it stands and a token meaning bytes is
quoted ([[invariants/numerals-denote-numbers|numerals-denote-numbers]]). Both the
parser (skipping the `Call` wrapper for a value head) and elaboration (through
`elaborator::word_val`) read that one answer. Its dual is `quote.rs`'s
`is_bare_word`: whatever the numeral grammar claims cannot be emitted bare, or
printed text would come back as a value. `is_bare_word` *lexes* rather than
scanning characters, so it inherits `continues_bare_word` and the positional splits
together — which is why a metacharacter added to `continues_bare_word` also starts
being quoted on the way out, and why `&` (added when `echo hi&` was found to
lex as one word ending in `&`) makes `http://h/?a=1&b=2` an emitted `'…'`.
The ASCII control characters are not bare-word characters and are refused
outside strings; the nine Unicode bidirectional controls are refused anywhere in
the source, before lexing begins. In a double-quoted string `\u{…}` is the
spelling for all of them.
The remaining literals are
*punctuation*, not words — `()` for unit beside `[]` and `[:]` — so no
spelling of a name can collide with them.

**Parsing is recursive descent with a Pratt core for `$[…]`.**
`parse(source)` returns `Vec<Stmt>` or a `ParseError`. Statement and pipeline
productions descend recursively (`parse_stmt`, `parse_pipeline`, `parse_primary`)
(a `case` arm's lambda included) under a depth counter that fails cleanly on pathological nesting rather than
overflowing the host stack. The body of `$[…]` is one Pratt parser by binding
power (`parse_expr_prec`), not bash's partitioned `(( ))` / `[[ ]]`, and its
operands are the ordinary atoms of the value grammar: `$[$s == 'quit']` is
admitted by the parser and judged by the checker. The one judgement the parser
keeps is the one that needs no types: a bare non-numeral word is a string, so
under `-`, arithmetic or ordering it can never typecheck, and `numeric_operand`
refuses `$[x + 1]` asking whether `$x` was meant — the old sublanguage's best
diagnostic, kept without its leaf grammar. The five operator forms —
`Binary`, `Negate`, `Not`, `And`, `Or` — are `Ast` variants like any other;
there is no `Expr` type, and `$[…]` leaves no node behind. `""` is a
literal (the empty string), not an interpolation.

**Newlines bend around a continuation, on both sides of it.** A trailing `|`
or `?` promises a stage or a branch, so the parser skips the newlines after it
(`?` continues exactly as `|` does, both through `eat_continuation`) and the REPL's continuation prompt is telling the truth when it asks for the
next line (`needs_continuation` runs the real parser and reads the `ParseError`'s
own `incomplete` verdict; `join_continuation` folds lines in with `'\n'`). A
newline *before* `?` is allowed too, so `cmd\n? fallback` and `cmd ?\nfallback`
both parse. A `;` never continues anything.

**Redirects are the three standard streams.** The lexer refuses every fd-prefixed
spelling but `2>`, `2>>`, `2>~` and `2>&1`, so its `Redirect { stderr, op }` and
`StderrToStdout` tokens carry no fd number; `parse_redirect_into` eliminates them
into the sum `Redirect<Ast>` through the total `redirect_word` — so no fd number
exists anywhere downstream
([[invariants/redirects-are-the-three-standard-streams|redirects-are-the-three-standard-streams]]). Each is then bound into `Redirects<Ast>` by `Redirects::bind`, which refuses a second binding of a stream with a caret on the second redirect
([[decisions/260930_redirects-are-bindings|redirects-are-bindings]]).
Redirects belong to the stage: every pipeline stage takes trailing redirects
(`collect_trailing_redirects`), and one node, `Ast::Redirected`, holds them,
minted only when the list is non-empty; `Call` and `Scope` carry none. An
external head's redirects fuse into its `Exec` at elaboration; any other stage
takes a `Redirect` frame.

**The AST is flat by decision.** `Ast` (expressions) and `Stmt` are wide flat
enums ([[decisions/260530_ast-stays-flat|ast-stays-flat]]); no desugaring happens
here. The surface forms are preserved verbatim for the
[[map/core/elaboration|elaborator]], the one phase that lowers them.

**Head classification refines in three stages, each deciding a different
question.** The parser fixes the *syntactic shape* of each command head
(`ast.rs`: `ExternalName` for `^name`, `Path` / `TildePath` for `./x` / `~/x`,
`Bare` for a lexical-lookup-or-`Exec` word, `Value` for every other atom). A
literal word, a bare `~` and a `$[…]` block are value heads wherever they
stand, so `let h = ~` binds the home directory and `42 foo` is the checker's
T0011 rather than a missing command. The
[[map/core/elaboration|elaborator]] resolves a `Bare` head against the lexical
scope — a bound name lowers to CBPV application, an unbound one to `Exec`
(forward declarations cover only the thunk-form bindings `group.rs` knots, so
the shadow agrees with what evaluation will see). A name-dispatched `Exec`
then resolves binding → handler → PATH at *evaluation* time
(`ir.rs` `CommandWord`), so handler installation and PATH state are read when
the command runs, not when it was parsed. The five reserved
[[design/control-operators|control operators]] are recognised at the parser
layer, by a `match` in `parse_control_op` over `syntax::CONTROL_OPERATORS`;
`else` and `elsif` are refused as heads (they continue an `if`), and
everything else is a library binding. `group.rs` is a pre-pass
marking mutually recursive binding groups for the elaborator.

See also [[internals/compilation-ladder|compilation-ladder]]; map
[[map/core/syntax|syntax]]. Grammar: `docs/SPEC.md` §3, §17.1; RATIONALE
§"One form, one meaning".
