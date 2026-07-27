; ── Keywords ─────────────────────────────────────────────────────────────────

"let"    @keyword
"return" @keyword.return

; if / elsif / else / case are syntactic forms
"if"    @keyword.control.conditional
"elsif" @keyword.control.conditional
"else"  @keyword.control.conditional
"case"  @keyword.control

; ── Operators ────────────────────────────────────────────────────────────────

"|"   @operator   ; pipe
"?"   @operator   ; failure chain
"&"   @operator   ; background

"="   @operator   ; let binding

(redir_write)      @operator
(redir_stream)     @operator
(redir_append)     @operator
(redir_read)       @operator
(redir_fd)         @operator
(redir_herestring) @operator

"..."  @operator  ; spread

"^"    @operator  ; bypass sigil

; force sigils: !{...} opening, !name, !$name
(force_brace) @operator
(force_bang)  @operator

; Arithmetic operators
(arith_binary op: _ @operator)
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

(string_single)   @string
(string_double)   @string
(bumped_string_1) @string
(bumped_string_2) @string
(bumped_string_3) @string

(escape_sequence) @string.escape
(escape_single)   @string.escape

; Interpolated segments inside double-quoted strings
(interp_arith)       @embedded
(interp_force)       @embedded
(interp_force_plain) @variable

; ── Numbers ──────────────────────────────────────────────────────────────────

(integer)   @number
(float)     @number.float
(fd_target) @number

; ── Booleans & unit ──────────────────────────────────────────────────────────

(boolean)      @boolean
(unit_literal) @constant.builtin

; ── Tags (variant constructors) ──────────────────────────────────────────────

(tag (tag_label) @constructor)

; ── Tilde ────────────────────────────────────────────────────────────────────

(tilde) @constant.builtin

; ── Bypass ───────────────────────────────────────────────────────────────────

(bypass "^" @operator)

; ── Patterns ─────────────────────────────────────────────────────────────────

(wildcard) @variable.builtin           ; _
(rest_pattern "..." @operator (identifier) @variable.parameter)

(let_stmt
  (_pattern (identifier) @variable.declaration))

(lambda_params
  (_pattern (identifier) @variable.parameter))

(map_pattern_entry
  key: (identifier) @property)

; ── Map entries ──────────────────────────────────────────────────────────────

(map_entry
  key: (identifier) @property)

(map_entry
  key: (string_single) @property)

(map_entry
  key: (tag) @property)

; ── Function calls ───────────────────────────────────────────────────────────

; The first identifier in an application is the head — a builtin or function.
(application
  . (identifier) @function.call)

; ── Control flow (control-operator keywords, plus library functions treated
;    as keywords for the eye) ─────────────────────────────────────────────────
;
; Overrides @function.call above for these specific heads: try/guard/within/
; grant/audit are real keywords (core::syntax::is_keyword) with no dedicated
; grammar node — they parse as an ordinary `application` whose head happens
; to be one of these names — and the rest are library functions styled as
; keywords for the eye.

(application
  . (identifier) @keyword.control
  (#match? @keyword.control
    "^(try|guard|within|grant|audit|for|spawn|await|race|watch|service|detach|par|map|filter|fold|reduce|each)$"))

; Sandbox / scoping builtins read as keywords for the eye
(application
  . (identifier) @keyword.import
  (#match? @keyword.import "^(use|source)$"))

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
