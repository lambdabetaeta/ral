; ── Keywords ─────────────────────────────────────────────────────────────────

"let"    @keyword
"return" @keyword.return

; ── Control flow (library functions treated as keywords) ─────────────────────

(application
  . (identifier) @keyword.control
  (#match? @keyword.control
    "^(if|for|while|try|case|spawn|await|race|use|source|grant|withenv|withdir|map|filter|fold|reduce)$"))

; ── Operators ────────────────────────────────────────────────────────────────

"|"   @operator   ; pipe
"?"   @operator   ; failure chain
"&"   @operator   ; background

"="   @operator   ; let binding

">"   @operator
">>"  @operator
"<"   @operator
">&"  @operator

"..."  @operator  ; spread

"^"    @operator  ; bypass sigil

; force sigils: !{...} opening, !name, !$name
(force_brace) @operator
(force_bang)  @operator

; Arithmetic operators
(arith_binary _ @operator _)
(arith_negate "-" @operator)
"not" @keyword.operator

; ── Variables / dereferences ─────────────────────────────────────────────────

; $name — highlight the whole token
(deref)       @variable
(deref_paren) @variable
(deref_index) @variable

; Interpolation inside strings
(interp_deref)       @variable
(interp_deref_paren) @variable
(interp_deref_index) @variable

; ── Strings ──────────────────────────────────────────────────────────────────

(string_single) @string
(string_double) @string

(escape_sequence) @string.escape
(escape_single)   @string.escape

; Interpolated segments inside double-quoted strings
(interp_arith)       @embedded
(interp_force)       @embedded
(interp_force_plain) @variable

; ── Numbers ──────────────────────────────────────────────────────────────────

(integer) @number
(float)   @number.float

; ── Booleans & unit ──────────────────────────────────────────────────────────

(boolean)      @boolean
(unit_literal) @constant.builtin

; ── Tilde ────────────────────────────────────────────────────────────────────

(tilde) @constant.builtin

; ── Bypass ───────────────────────────────────────────────────────────────────

(bypass "^" @operator)
(bypass (word) @function)

; ── Patterns ─────────────────────────────────────────────────────────────────

(wildcard) @variable.builtin           ; _
(rest_pattern "..." @operator (identifier) @variable.parameter)

(let_stmt
  (_pattern (identifier) @variable.declaration))

(lambda_params
  (_pattern (identifier) @variable.parameter))

(map_pattern_entry
  (identifier) @property .)

; ── Map entries ──────────────────────────────────────────────────────────────

(map_entry
  (identifier) @property)

; ── Comments ─────────────────────────────────────────────────────────────────

(comment) @comment

; ── Punctuation ──────────────────────────────────────────────────────────────

"{"  @punctuation.bracket
"}"  @punctuation.bracket
"["  @punctuation.bracket
"]"  @punctuation.bracket
"("  @punctuation.bracket
")"  @punctuation.bracket
","  @punctuation.delimiter
":"  @punctuation.delimiter
