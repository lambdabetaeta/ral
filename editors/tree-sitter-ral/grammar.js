/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// Bare-word characters: anything that is not a delimiter, sigil, or whitespace.
// Mirrors Lexer::is_bare_char in core/src/lexer.rs.
// Note: ':', '=', '.', '#' (non-initial), digits are all bare chars.
const BARE = /[^ \t\n\r|{}\[\]$!~<>"',();&#?]+/;

// Identifier: starts with letter/underscore, continues with alphanumeric/-/_.
const IDENT = /[a-zA-Z_][a-zA-Z0-9_-]*/;

module.exports = grammar({
  name: 'ral',

  // The identifier rule is the "word" class: any string literal in the
  // grammar that matches IDENT is a keyword and won't match as identifier.
  word: $ => $.identifier,

  // Spaces/tabs and line continuations are skipped between tokens.
  // Newlines are NOT in extras — they are significant statement separators.
  extras: $ => [
    $.comment,
    /[ \t]/,
    /\\\r?\n/,
  ],

  supertypes: $ => [
    $._value,
    $._pattern,
    $._arith,
  ],

  // GLR conflict: deref and deref_index both start with '$' IDENT.
  conflicts: $ => [
    [$.deref, $.deref_index],
  ],

  rules: {

    // ── Top-level ─────────────────────────────────────────────────────────────

    source_file: $ => repeat(choice(
      seq($.statement, /[\n;]+/),
      /[\n;]+/,
    )),

    statement: $ => $.chain,

    chain: $ => prec.left(seq(
      $.pipeline,
      repeat(seq('?', $.pipeline)),
    )),

    pipeline: $ => prec.left(seq(
      $.cmd,
      repeat(seq('|', $.cmd)),
      optional('&'),
    )),

    cmd: $ => choice(
      $.let_stmt,
      $.return_stmt,
      $.application,
    ),

    let_stmt: $ => seq(
      'let',
      $._pattern,
      '=',
      $.chain,
    ),

    return_stmt: $ => seq(
      'return',
      optional($._value),
    ),

    // Head and arguments are the same syntactic class.
    application: $ => prec.left(seq(
      $._value,
      repeat($._value),
      repeat($.redirect),
    )),

    // ── Values ───────────────────────────────────────────────────────────────

    _value: $ => choice(
      $.word,
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
      $.spread,
      $.bypass,
    ),

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

    // key: binding optional_default — e.g. [host: h, port: p = 5432]
    map_pattern_entry: $ => seq(
      $.identifier,
      ':',
      optional($.identifier),
      optional(seq('=', $._value)),
    ),

    // ── Redirects ────────────────────────────────────────────────────────────

    redirect: $ => seq(
      optional($.fd_number),
      choice(
        $.redir_append,
        $.redir_fd,
        $.redir_write,
        $.redir_read,
      ),
      $._value,
    ),

    fd_number:   $ => token(/[0-9]/),
    redir_append: $ => '>>',
    redir_write:  $ => '>',
    redir_read:   $ => '<',
    redir_fd:     $ => '>&',

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
      optional(seq(
        $._value,
        repeat(seq(',', $._value)),
        optional(','),
      )),
      ']',
    ),

    // A map literal is either '[:] ' (empty) or '[key: val, ...]'.
    // Disambiguated from list by the leading 'identifier :' pattern.
    map_literal: $ => choice(
      seq('[', ':', ']'),
      seq(
        '[',
        $.map_entry,
        repeat(seq(',', choice($.map_entry, $.spread))),
        optional(','),
        ']',
      ),
    ),

    map_entry: $ => seq(
      $.identifier,
      ':',
      $._value,
    ),

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
    // Uses compound token '$' + identifier so no space is permitted.
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

    // ~ or ~user
    tilde: $ => token(seq('~', optional(IDENT))),

    // ── Spread ───────────────────────────────────────────────────────────────

    spread: $ => seq(
      '...',
      choice($.deref_paren, $.deref_index, $.deref),
    ),

    // ── Bypass ───────────────────────────────────────────────────────────────

    // ^cmd — bypass ral dispatch, run as raw external command
    bypass: $ => seq(
      '^',
      token.immediate(BARE),
    ),

    // ── Strings ──────────────────────────────────────────────────────────────

    string_single: $ => seq(
      "'",
      repeat(choice(
        alias(token.immediate("''"), $.escape_single),
        token.immediate(/[^']+/),
      )),
      token.immediate("'"),
    ),

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

    escape_sequence: $ => token.immediate(seq('\\', /[nte\\0"$!\r\n]/)),

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

    identifier: $ => IDENT,

    // Bare words: paths, flags, globs, anything that isn't a special token.
    // Must not start with digits (those are integers/floats).
    word: $ => token(BARE),

    integer: $ => /[0-9]+/,
    float:   $ => /[0-9]+\.[0-9]+/,

    boolean: $ => choice('true', 'false'),

    unit_literal: $ => 'unit',

    comment: $ => token(seq('#', /.*/)),
  },
})
