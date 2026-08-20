/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// Bare-word stems: anything that is not a delimiter, sigil, or whitespace.
// Mirrors Lexer::is_bare_char in core/src/syntax/lexer.rs, plus two positional
// rules the character predicate alone can't express:
//
// - `,` is punctuation only while the real lexer is inside `[...]` (list/map
//   literals); everywhere else — top level, inside `{...}`/`!{...}` blocks —
//   it is bare (`ps -Aco rss,comm`, `echo a,b,c`).  Tree-sitter has no
//   delimiter-stack, so bare words used directly inside a list/map literal
//   (`_bracket`-suffixed below) exclude `,`; every other bare-word site
//   includes it.
// - `#` opens a comment (or a quoted string) only as the first character of
//   a *new* token; mid-word it is ordinary (`curl host:8080/foo#anchor`).
//   Excluding it from continuation positions would be wrong, but the one
//   spot a `word` alternative can start on `#` (the leading symbol-start
//   branch) must still exclude it so `comment` wins there instead.
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
const CONT        = /[^ \t\n\r|{}\[\]$!~<>"'`():;&?\\]/;     // bare-word continuation: ',' and '#' both fine
const CONT_NC     = /[^ \t\n\r|{}\[\]$!~<>"'`():;&?,\\]/;    // …inside a list/map literal: no ','
// The char right after an identifier-shaped run that disqualifies it from
// being a pure `identifier` (e.g. the '.' in "foo.bar"): must exclude
// ident-continuation chars themselves, or "grant" would match by treating
// its own last letter as the disqualifier. Not the token's overall first
// character, so — unlike LEAD_SYM below — '#' is still fine here.
const DISQ        = /[^a-zA-Z0-9_\- \t\n\r|{}\[\]$!~<>"'`():;&?\\]/;
const DISQ_NC     = /[^a-zA-Z0-9_\- \t\n\r|{}\[\]$!~<>"'`():;&?,\\]/;
const LEAD_SYM    = /[^a-zA-Z_0-9 \t\n\r|{}\[\]$!~<>"'`():;&?#\\]/;   // leading symbol char: no '#'
const LEAD_SYM_NC = /[^a-zA-Z_0-9 \t\n\r|{}\[\]$!~<>"'`():;&?#,\\]/;  // …inside a list/map literal: no ',' either
const BARE_STEM_NODIGIT = seq(/[^ \t\n\r|{}\[\]$!~<>"'`():;&?\\0-9]/, repeat(CONT));

// The four shapes of `word` (see below), built over a continuation class,
// a disqualifying-char class, and a leading-symbol class so the
// comma-excluding bracket variant reuses the same structure instead of
// repeating it.
function wordAlternatives(cont, disq, lead) {
  const stem = seq(cont, repeat(cont));
  return [
    seq(/[0-9]/, repeat(cont), repeat(seq(':', stem))),
    seq(/[a-zA-Z_][a-zA-Z0-9_-]*/, disq, repeat(cont), repeat(seq(':', stem))),
    seq(IDENT, ':', stem, repeat(seq(':', stem))),
    seq(lead, repeat(cont), repeat(seq(':', stem))),
  ];
}

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
  //  - _list_item vs _map_entry: a spread can begin either; the inside-`[…]`
  //    parser decides which by looking at the first non-spread shape.
  conflicts: $ => [
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
    // Newlines may separate 'if'/'elsif' from their condition, and the
    // condition from its body, mirroring Parser::parse_if_branch (SPEC §3.3).
    if_stmt: $ => prec.right(seq(
      'if',
      optional(/\n+/),
      field('cond', $._value),
      optional(/\n+/),
      field('then', $._value),
      repeat($.elsif_clause),
      optional($.else_clause),
    )),

    // A newline before 'elsif'/'else' reads equally well as the separator
    // ending the previous statement, and tree-sitter silently always chose
    // that — so a clause starting on its own line parsed as a bare command
    // named `elsif`. Bundling the newline run into the keyword's own token
    // hands the choice to the lexer, which can look past the run and match
    // only when the keyword is really there. The alias keeps the visible
    // node type bare, so `"elsif" @keyword` still fires; its span reaches
    // back over the swallowed whitespace, which has no glyph to mis-color.
    elsif_clause: $ => seq(
      choice('elsif', alias(token(seq(/\n+/, /[ \t\r]*/, 'elsif')), 'elsif')),
      optional(/\n+/),
      field('cond', $._value),
      optional(/\n+/),
      field('then', $._value),
    ),

    else_clause: $ => seq(
      choice('else', alias(token(seq(/\n+/, /[ \t\r]*/, 'else')), 'else')),
      optional(/\n+/),
      field('body', $._value),
    ),

    // case scrutinee [`tag: body, ...]
    //
    // The arm list is a production of its own and no value stands in its
    // place: a computed table (`case $x $handlers`), a `...` spread arm, and
    // an empty list are all parse errors, because each hides the set of
    // alternatives that makes exhaustiveness decidable (SPEC §8.3).  A
    // repeated tag is refused by the real parser and not here — tree-sitter
    // compares no two arms.
    // Newlines may separate the scrutinee from the list, and the arms from
    // each other (SPEC §3.3).
    case_stmt: $ => seq(
      'case',
      optional(/\n+/),
      field('scrutinee', $._value),
      optional(/\n+/),
      '[',
      optional(/\n+/),
      field('arm', $.case_arm),
      repeat(seq(',', optional(/\n+/), field('arm', $.case_arm))),
      optional(','),
      optional(/\n+/),
      ']',
    ),

    // `tag: { |payload| … } — or any other atom, which the arm applies to the
    // payload.  The tag is a bare label rather than a `tag` value: a tag takes
    // the next adjacent atom as its payload, and would swallow the arm's body.
    case_arm: $ => seq(
      field('tag', $.tag_label),
      ':',
      field('body', choice($.case_arm_lambda, $.word_bracket, $._value_nonblock)),
    ),

    // A brace body is the arm's own binder form, not a block: it binds exactly
    // one payload, where a block binds any number or none.  Hence the arm's
    // other spellings come from `_value_nonblock`, and the binder is written
    // out here — aliased to `lambda_params` so one query highlights every
    // parameter list.
    case_arm_lambda: $ => seq(
      '{',
      field('binder', alias($._case_arm_binder, $.lambda_params)),
      optional($._block_body),
      '}',
    ),

    _case_arm_binder: $ => seq('|', $._pattern, '|'),

    // Head and arguments are the same syntactic class, except that only the
    // head may be a `^name` bypass (`head = '^' NAME | atom` — a bypass is
    // never a general value: `parse_word` rejects a bare '^' anywhere else).
    // Args additionally admit `...$x` spreads and redirects.
    application: $ => prec.left(seq(
      choice($.bypass, $._value),
      repeat(choice($._value, $.spread, $.redirect)),
    )),

    // ── Values ───────────────────────────────────────────────────────────────

    // Forked only on the bare-word leaf: `_value` is the default (`,` bare),
    // `_value_bracket` is for sites lexed directly inside an open `[...]`
    // (list/map literal elements, map-entry values, pattern defaults) where
    // the real lexer instead treats `,` as the element separator.
    _value: $ => choice($.word, $._value_common),
    _value_bracket: $ => choice($.word_bracket, $._value_common),

    // Split around `block`, because a `case` arm's body is the one site where
    // a brace opens the arm's binder rather than a block (`case_arm_lambda`).
    _value_common: $ => choice($.block, $._value_nonblock),

    _value_nonblock: $ => choice(
      $.identifier,
      $.integer,
      $.float,
      $.boolean,
      $.unit_literal,
      $.string_single,
      $.string_double,
      $.tag,
      $.list_literal,
      $.map_literal,
      $.arith_expr,
      $.deref_paren,
      $.deref_index,
      $.deref,
      $.force_brace,
      $.force_bang,
      $.tilde,
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
    // The binder is its own field (mirroring map_entry's 'value') so a
    // highlight query can target it without also catching an identifier-
    // shaped key.
    map_pattern_entry: $ => seq(
      field('key', choice($.identifier, $.string_single, $.tag)),
      ':',
      optional(field('pattern', $._pattern)),
      optional(seq('=', $._value_bracket)),
    ),

    // ── Redirects ────────────────────────────────────────────────────────────

    // fd-aware redirect: the source fd's digits are baked into the operator
    // token to avoid colliding with `integer` (`2>` vs the value `2`).
    redirect: $ => choice(
      // fd-to-fd: `2>&1`.  Deliberately wider than the surface, which admits
      // that direction only — highlighting a refused `1>&2` beats not
      // highlighting it.
      seq($.redir_fd, $.fd_target),
      // file targets
      seq($.redir_append, $._value),
      seq($.redir_stream, $._value),
      seq($.redir_write, $._value),
      seq($.redir_read, $._value),
      // `<<` feeds a string value to stdin (a here-string, not a heredoc).
      seq($.redir_herestring, $._value),
    ),

    fd_target:        $ => token(/[0-9]+/),
    redir_append:     $ => token(seq(optional(/[0-9]+/), '>>')),
    redir_stream:     $ => token(seq(optional(/[0-9]+/), '>~')),
    redir_write:      $ => token(seq(optional(/[0-9]+/), '>')),
    redir_read:       $ => token(seq(optional(/[0-9]+/), '<')),
    redir_fd:         $ => token(seq(optional(/[0-9]+/), '>&')),
    redir_herestring: $ => token(seq(optional(/[0-9]+/), '<<')),

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

    _list_item: $ => choice($._value_bracket, $.spread),

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
      field('value', $._value_bracket),
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
      $.identifier,
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
      $.indexed,
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
      prec.left(1, seq($._arith, field('op', '||'), $._arith)),
      prec.left(2, seq($._arith, field('op', '&&'), $._arith)),
      prec.left(3, seq($._arith, field('op', choice('==', '!=', '<', '>', '<=', '>=')), $._arith)),
      prec.left(4, seq($._arith, field('op', choice('+', '-')), $._arith)),
      prec.left(5, seq($._arith, field('op', choice('*', '/', '%')), $._arith)),
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
    // and name, and `token.immediate` on '[' so none is permitted before an
    // index either: `$x [0]` is a command argument followed by a list
    // literal, not an index (Parser::next_token_is_adjacent).
    deref_index: $ => prec.left(seq(
      token(seq('$', IDENT)),
      repeat1(seq(token.immediate('['), optional(/[^\]\n]*/), ']')),
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

    // The body tokens carry an explicit precedence above `comment`'s default
    // (0): otherwise an embedded '#' — legal in either string body, and
    // ordinary mid-word text per the real lexer — ties on match length
    // against `comment`, which as an unconditional `extra` would run to end
    // of line and swallow the string's own closing delimiter.
    string_single: $ => choice(
      seq(
        "'",
        repeat(choice(
          alias(token.immediate("''"), $.escape_single),
          token.immediate(prec(1, /[^']+/)),
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
        token.immediate(prec(1, /[^"\\$!]+/)),
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
    // IDENT-shape.  Parameterised on `cont`/`lead` so the one place `,` must
    // stay punctuation — directly inside a list/map literal — gets its own
    // variant (`word_bracket`, aliased back to `word` for tooling) without
    // duplicating the four branches.
    word: $ => token(choice(...wordAlternatives(CONT, DISQ, LEAD_SYM))),

    word_bracket: $ => alias(token(choice(...wordAlternatives(CONT_NC, DISQ_NC, LEAD_SYM_NC))), $.word),

    identifier: $ => IDENT,

    integer: $ => /[0-9]+/,
    float:   $ => /[0-9]+\.[0-9]+/,

    boolean: $ => choice('true', 'false'),

    unit_literal: $ => 'unit',

    comment: $ => token(seq('#', /[^\n]*/)),
  },
})
