/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// Bare-word stems: anything that is not a delimiter, sigil, or whitespace.
// Mirrors Lexer::is_bare_char in core/src/lexer.rs.
//
// `:` is context-sensitive in the ral lexer: it splits the word only when
// followed by space/tab/newline/`]`, so `host: val` becomes three tokens but
// `localhost:5432` stays one.  Tree-sitter's regex flavour has no lookahead,
// so we approximate by modelling a bare word as `stem (':' stem)*`, where a
// `stem` is one or more bare chars NOT including `:`.  This keeps
// `localhost:5432` as one token while letting `host: val` split (the trailing
// `:` cannot start a stem before whitespace).
//
// IDENT-shaped tokens (letter/underscore start, then alphanumerics or '-'/'_')
// classify as `identifier` via tree-sitter's `word: $ => $.identifier`; this
// `word` rule covers everything else: slashes, dots, equals, leading digits,
// colon-joined stems.  The `word` rule's regex is constructed so that pure
// IDENT shapes never match — every branch contains at least one non-IDENT
// character, so the lexer can pick `identifier` unambiguously.
const BARE_STEM         = /[^ \t\n\r|{}\[\]$!~<>"'`():;&,#?\\][^ \t\n\r|{}\[\]$!~<>"'`():;&,#?\\]*/;
const BARE_STEM_NODIGIT = /[^ \t\n\r|{}\[\]$!~<>"'`():;&,#?\\0-9][^ \t\n\r|{}\[\]$!~<>"'`():;&,#?\\]*/;

// Identifier-style chars: letter/underscore, then alphanumerics, '-' or '_'.
const IDENT = /[a-zA-Z_][a-zA-Z0-9_-]*/;

module.exports = grammar({
  name: 'ral',

  // The identifier rule is the keyword class.  String literals in the grammar
  // that match IDENT (e.g. "if", "let", "case") become reserved keywords and
  // won't match `identifier` in dynamic positions.
  word: $ => $.identifier,

  // Spaces/tabs/carriage returns and line continuations are skipped between
  // tokens.  Newlines are NOT in extras — they are significant statement
  // separators.
  extras: $ => [
    $.comment,
    /[ \t\r]/,
    /\\\r?\n/,
  ],

  supertypes: $ => [
    $._value,
    $._pattern,
    $._arith,
  ],

  // GLR conflicts:
  //  - $deref vs $deref_index: both start with '$' IDENT.
  //  - list_pattern vs map_pattern: both start with '['.
  //  - _list_item vs _map_entry: a spread can begin either; the inside-`[…]`
  //    parser decides which by looking at the first non-spread shape.
  conflicts: $ => [
    [$.deref, $.deref_index],
    [$._list_item, $._map_entry],
  ],

  rules: {

    // ── Top-level ─────────────────────────────────────────────────────────────

    source_file: $ => repeat(choice(
      seq($.statement, /[\n;]+/),
      $.statement,
      /[\n;]+/,
    )),

    statement: $ => $.chain,

    chain: $ => prec.left(seq(
      $.pipeline,
      repeat(seq('?', optional(/\n+/), $.pipeline)),
    )),

    pipeline: $ => prec.left(seq(
      $.cmd,
      repeat(seq('|', optional(/\n+/), $.cmd)),
      optional('&'),
    )),

    cmd: $ => choice(
      $.let_stmt,
      $.return_stmt,
      $.if_stmt,
      $.case_stmt,
      $.application,
    ),

    let_stmt: $ => seq(
      'let',
      $._pattern,
      '=',
      optional(/\n+/),
      $.chain,
    ),

    return_stmt: $ => prec.right(seq(
      'return',
      optional($._value),
    )),

    // if cond then [elsif cond then]* [else atom]?
    // Both `then` and `else` branches are atoms — typically blocks or !{…}.
    if_stmt: $ => prec.right(seq(
      'if',
      field('cond', $._value),
      field('then', $._value),
      repeat($.elsif_clause),
      optional($.else_clause),
    )),

    elsif_clause: $ => seq(
      optional(/\n+/),
      'elsif',
      field('cond', $._value),
      field('then', $._value),
    ),

    else_clause: $ => seq(
      optional(/\n+/),
      'else',
      field('body', $._value),
    ),

    // case scrutinee [`tag: handler, ...]
    case_stmt: $ => seq(
      'case',
      field('scrutinee', $._value),
      field('table', $._value),
    ),

    // Head and arguments are the same syntactic class.  Args additionally
    // admit `...$x` spreads and redirects.
    application: $ => prec.left(seq(
      $._value,
      repeat(choice($._value, $.spread, $.redirect)),
    )),

    // ── Values ───────────────────────────────────────────────────────────────

    _value: $ => choice(
      $.identifier,
      $.word,
      $.integer,
      $.float,
      $.boolean,
      $.unit_literal,
      $.string_single,
      $.string_double,
      $.tag,
      $.block,
      $.list_literal,
      $.map_literal,
      $.arith_expr,
      $.deref_paren,
      $.deref_index,
      $.deref,
      $.force_brace,
      $.force_bang,
      $.tilde,
      $.bypass,
      $.indexed,
    ),

    // Postfix indexing on a force or parenthesised dereference value:
    // !{f $x}[k], $(name)[k].  `$name[k]` is captured by `deref_index`.
    indexed: $ => prec.left(seq(
      choice($.force_brace, $.deref_paren),
      repeat1(seq(token.immediate('['), /[^\]\n]+/, ']')),
    )),

    // ── Patterns ─────────────────────────────────────────────────────────────

    _pattern: $ => choice(
      $.identifier,
      $.wildcard,
      $.list_pattern,
      $.map_pattern,
    ),

    wildcard: $ => '_',

    list_pattern: $ => seq(
      '[',
      optional(seq(
        $._pattern_item,
        repeat(seq(',', $._pattern_item)),
        optional(','),
      )),
      ']',
    ),

    _pattern_item: $ => choice(
      $.rest_pattern,
      $._pattern,
    ),

    rest_pattern: $ => seq('...', $.identifier),

    map_pattern: $ => seq(
      '[',
      $.map_pattern_entry,
      repeat(seq(',', $.map_pattern_entry)),
      optional(','),
      ']',
    ),

    // key: binding optional_default — e.g. [host: h, port: p = 5432].
    // Keys may be plain identifiers, single-quoted strings, or backtick tags.
    map_pattern_entry: $ => seq(
      field('key', choice($.identifier, $.string_single, $.tag)),
      ':',
      optional($._pattern),
      optional(seq('=', $._value)),
    ),

    // ── Redirects ────────────────────────────────────────────────────────────

    // fd-aware redirect: the source fd's digits are baked into the operator
    // token to avoid colliding with `integer` (`2>` vs the value `2`).
    redirect: $ => choice(
      // fd-to-fd: `2>&1`, `1>&2`
      seq($.redir_fd, $.fd_target),
      // file targets
      seq($.redir_append, $._value),
      seq($.redir_stream, $._value),
      seq($.redir_write, $._value),
      seq($.redir_read, $._value),
    ),

    fd_target:    $ => token(/[0-9]+/),
    redir_append: $ => token(seq(optional(/[0-9]+/), '>>')),
    redir_stream: $ => token(seq(optional(/[0-9]+/), '>~')),
    redir_write:  $ => token(seq(optional(/[0-9]+/), '>')),
    redir_read:   $ => token(seq(optional(/[0-9]+/), '<')),
    redir_fd:     $ => token(seq(optional(/[0-9]+/), '>&')),

    // ── Blocks ───────────────────────────────────────────────────────────────

    block: $ => seq(
      '{',
      optional($.lambda_params),
      optional($._block_body),
      '}',
    ),

    lambda_params: $ => seq(
      '|',
      repeat1($._pattern),
      '|',
    ),

    _block_body: $ => seq(
      repeat(/[\n;]/),
      $.statement,
      repeat(seq(/[\n;]+/, optional($.statement))),
      repeat(/[\n;]/),
    ),

    // ── Collections ──────────────────────────────────────────────────────────

    // A list literal is '[' items ']' where items are values (not map entries).
    list_literal: $ => seq(
      '[',
      optional(/\n+/),
      optional(seq(
        $._list_item,
        repeat(seq(',', optional(/\n+/), $._list_item)),
        optional(','),
        optional(/\n+/),
      )),
      ']',
    ),

    _list_item: $ => choice($._value, $.spread),

    // A map literal is either '[:] ' (empty) or '[entries...]' where each
    // entry is `key: value` or `...$expr` (spread).
    map_literal: $ => choice(
      seq('[', ':', ']'),
      seq(
        '[',
        optional(/\n+/),
        $._map_entry,
        repeat(seq(',', optional(/\n+/), $._map_entry)),
        optional(','),
        optional(/\n+/),
        ']',
      ),
    ),

    _map_entry: $ => choice($.map_entry, $.spread),

    // Keys: identifier, single-quoted string, backtick tag, or $deref.
    map_entry: $ => seq(
      field('key', choice($.identifier, $.string_single, $.tag, $.deref)),
      ':',
      field('value', $._value),
    ),

    // ── Tag literals ─────────────────────────────────────────────────────────

    // `name — optionally followed by a payload value in atom contexts.
    // The grammar accepts the optional payload greedily; the typechecker
    // decides whether it is well-typed for the variant.  The payload arm
    // sits at prec(-1) so `[`ok: payload]` keeps parsing the colon as a
    // map-entry separator rather than greedily consuming `: payload` as the
    // tag's payload.
    tag: $ => prec.right(seq(
      $.tag_label,
      optional($._tag_payload),
    )),

    tag_label: $ => token(seq('`', /[a-zA-Z_][a-zA-Z0-9_-]*/)),

    _tag_payload: $ => prec(-1, choice(
      $.integer,
      $.float,
      $.boolean,
      $.unit_literal,
      $.string_single,
      $.string_double,
      $.block,
      $.list_literal,
      $.map_literal,
      $.arith_expr,
      $.deref_paren,
      $.deref_index,
      $.deref,
      $.force_brace,
      $.force_bang,
      $.tilde,
    )),

    // ── Arithmetic expressions ────────────────────────────────────────────────

    // $[ expr ] — arithmetic/logic expression block.
    // '$[' is a compound token so the lexer doesn't confuse it with '$' + '['.
    arith_expr: $ => seq(
      token(seq('$', '[')),
      $._arith,
      ']',
    ),

    _arith: $ => choice(
      $.arith_binary,
      $.arith_negate,
      $.arith_not,
      $.arith_group,
      $.arith_force,
      $.deref_paren,
      $.deref_index,
      $.deref,
      $.integer,
      $.float,
      $.boolean,
    ),

    arith_binary: $ => choice(
      prec.left(1, seq($._arith, '||', $._arith)),
      prec.left(2, seq($._arith, '&&', $._arith)),
      prec.left(3, seq($._arith, choice('==', '!=', '<', '>', '<=', '>='), $._arith)),
      prec.left(4, seq($._arith, choice('+', '-'), $._arith)),
      prec.left(5, seq($._arith, choice('*', '/', '%'), $._arith)),
    ),

    arith_negate: $ => prec(6, seq('-', $._arith)),
    arith_not:    $ => prec(6, seq('not', $._arith)),
    arith_group:  $ => seq('(', $._arith, ')'),

    // Force inside arithmetic: !{ cmd }
    arith_force: $ => seq(
      token(seq('!', '{')),
      optional($._block_body),
      '}',
    ),

    // ── Dereferences ─────────────────────────────────────────────────────────

    // $(name) — parenthesised dereference
    deref_paren: $ => seq(
      token(seq('$', '(')),
      $.identifier,
      ')',
    ),

    // $name[k1][k2] — indexed dereference; at least one index.
    // Uses a compound `$IDENT` token so no space is permitted between sigil
    // and name.
    deref_index: $ => prec.left(seq(
      token(seq('$', IDENT)),
      repeat1(seq('[', optional(/[^\]\n]*/), ']')),
    )),

    // $name — plain dereference
    deref: $ => token(seq('$', IDENT)),

    // ── Force ────────────────────────────────────────────────────────────────

    // !{ stmts } — execute inline block
    force_brace: $ => seq(
      token(seq('!', '{')),
      optional($._block_body),
      '}',
    ),

    // !$name or !name — force a stored thunk
    force_bang: $ => choice(
      token(seq('!', '$', IDENT)),
      token(seq('!', IDENT)),
    ),

    // ── Tilde ────────────────────────────────────────────────────────────────

    // ~ or ~user or ~/path
    tilde: $ => token(seq('~', optional(/[a-zA-Z0-9_./-]*/))),

    // ── Spread ───────────────────────────────────────────────────────────────

    // Spread of a value: ...$x, ...[a,b], ...!{f}
    spread: $ => seq(
      '...',
      choice(
        $.deref_paren,
        $.deref_index,
        $.deref,
        $.list_literal,
        $.map_literal,
        $.force_brace,
      ),
    ),

    // ── Bypass ───────────────────────────────────────────────────────────────

    // ^cmd — bypass ral dispatch, run as raw external command
    bypass: $ => seq(
      '^',
      token.immediate(BARE_STEM_NODIGIT),
    ),

    // ── Strings ──────────────────────────────────────────────────────────────

    string_single: $ => choice(
      seq(
        "'",
        repeat(choice(
          alias(token.immediate("''"), $.escape_single),
          token.immediate(/[^']+/),
        )),
        token.immediate("'"),
      ),
      // Hash-bumped single-quoted literals (verbatim, embed any '): a
      // run of N `#`s before the open `'` is matched by the same run of
      // `#`s after the close `'`.  Tree-sitter has no balanced-bracket
      // primitive, so we enumerate the first few common levels.  Beyond
      // level 3, callers can resort to escaping or shorter quotes.
      $.bumped_string_1,
      $.bumped_string_2,
      $.bumped_string_3,
    ),

    bumped_string_1: $ => token(prec(2, seq(
      "#'",
      repeat(choice(/[^']/, /'[^#]/)),
      "'#",
    ))),
    bumped_string_2: $ => token(prec(2, seq(
      "##'",
      repeat(choice(/[^']/, /'[^#]/, /'#[^#]/)),
      "'##",
    ))),
    bumped_string_3: $ => token(prec(2, seq(
      "###'",
      repeat(choice(/[^']/, /'[^#]/, /'#[^#]/, /'##[^#]/)),
      "'###",
    ))),

    string_double: $ => seq(
      '"',
      repeat(choice(
        $.escape_sequence,
        $.interp_arith,
        $.interp_force,
        $.interp_deref_paren,
        $.interp_deref_index,
        $.interp_deref,
        $.interp_force_plain,
        token.immediate(/[^"\\$!]+/),
      )),
      token.immediate('"'),
    ),

    escape_sequence: $ => token.immediate(seq(
      '\\',
      choice(/[nrte\\0"$!]/, /x[0-9a-fA-F]{2}/, /u\{[0-9a-fA-F]{1,6}\}/, /\r?\n/),
    )),

    // $[ expr ] inside a string
    interp_arith: $ => seq(
      token.immediate(seq('$', '[')),
      $._arith,
      ']',
    ),

    // !{ ... } inside a string
    interp_force: $ => seq(
      token.immediate(seq('!', '{')),
      optional($._block_body),
      '}',
    ),

    // $(name) inside a string
    interp_deref_paren: $ => seq(
      token.immediate(seq('$', '(')),
      $.identifier,
      ')',
    ),

    // $name[k] inside a string
    interp_deref_index: $ => seq(
      token.immediate(seq('$', IDENT)),
      repeat1(seq('[', optional(/[^\]\n]*/), ']')),
    ),

    // $name inside a string
    interp_deref: $ => token.immediate(seq('$', IDENT)),

    // !$name or !name inside a string
    interp_force_plain: $ => token.immediate(
      seq('!', choice(seq('$', IDENT), IDENT)),
    ),

    // ── Primitives ───────────────────────────────────────────────────────────

    // Bare words: paths, flags, globs, host:port pairs, anything that isn't
    // a special token AND isn't already an identifier.  Identifier-shaped
    // tokens (letter-start, alphanumerics/-/_) hit the `identifier` rule via
    // tree-sitter's `word: $ => $.identifier` declaration; this rule covers
    // the remainder: slashes, dots, equals, leading digits, colons joining
    // stems (host:5432), etc.
    //
    // Every branch of this `choice` is anchored on a disqualifying-non-IDENT
    // character so the lexer never picks `word` over `identifier` for a pure
    // IDENT-shape.
    word: $ => token(choice(
      // starts with digit
      seq(/[0-9][^ \t\n\r|{}\[\]$!~<>"'`():;&,#?\\]*/, repeat(seq(':', BARE_STEM))),
      // first stem has a non-IDENT continuation char (slash, dot, equals, …)
      seq(
        /[a-zA-Z_][a-zA-Z0-9_-]*[^a-zA-Z0-9_\- \t\n\r|{}\[\]$!~<>"'`():;&,#?\\][^ \t\n\r|{}\[\]$!~<>"'`():;&,#?\\]*/,
        repeat(seq(':', BARE_STEM)),
      ),
      // IDENT-shaped first stem with `:stem` continuations (host:5432)
      seq(IDENT, ':', BARE_STEM, repeat(seq(':', BARE_STEM))),
      // starts with a non-IDENT, non-digit char (e.g. '.', '/', '+', '-')
      seq(
        /[^a-zA-Z_0-9 \t\n\r|{}\[\]$!~<>"'`():;&,#?\\][^ \t\n\r|{}\[\]$!~<>"'`():;&,#?\\]*/,
        repeat(seq(':', BARE_STEM)),
      ),
    )),

    identifier: $ => IDENT,

    integer: $ => /[0-9]+/,
    float:   $ => /[0-9]+\.[0-9]+/,

    boolean: $ => choice('true', 'false'),

    unit_literal: $ => 'unit',

    comment: $ => token(seq('#', /[^\n]*/)),
  },
})
