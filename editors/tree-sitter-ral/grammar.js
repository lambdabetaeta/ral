/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const IDENT = /[a-zA-Z_][a-zA-Z0-9_-]*/;
const DEREF_IDENT = /[a-zA-Z_]([a-zA-Z0-9_-]*[a-zA-Z0-9_])?/;
const INTEGER = /[+-]?[0-9]+/;
const FLOAT = /[+-]?([0-9]+\.[0-9]*|\.[0-9]+)([eE][+-]?[0-9]+)?/;
const GAP = /[ \t\r]+/;
const SPACE = repeat1(choice(GAP, '\n'));
const NL = repeat1('\n');
const SEP = repeat1(choice('\n', ';'));

// Colon splitting is approximated by nonempty stems joined by colon runs.
// Commas and arithmetic operators belong to the innermost delimiter only.
function words(mode) {
  const excluded = mode === 'arith' ? '+*/%=\\-&' : '';
  const comma = mode === 'top' ? '' : ',';
  const delimiters = ` \\t\\n\\r|{}\\[\\]$^!<>"'\u0060():;${comma}${excluded}`;
  const cont = new RegExp(`[^${delimiters}]`);
  const disq = new RegExp(`[^a-zA-Z0-9_\\-${delimiters}]`);
  const lead = new RegExp(`[^a-zA-Z_0-9&?#.${delimiters}]`);
  const nondot = new RegExp(`[^.${delimiters}]`);
  const ident = mode === 'arith' ? /[a-zA-Z_][a-zA-Z0-9_]*/ : IDENT;
  const stem = seq(cont, repeat(cont));
  const colons = seq(/:+/, stem);
  return choice(
    seq(/[0-9]/, repeat(cont), repeat(colons)),
    seq(ident, disq, repeat(cont), repeat(colons)),
    seq(ident, repeat1(colons)),
    seq(lead, repeat(cont), repeat(colons)),
    seq(choice('.', '..'), optional(seq(nondot, repeat(cont))), repeat(colons)),
    ...(mode === 'arith' ? [seq(
      /([0-9]+\.[0-9]*|\.[0-9]+)([eE][+-]?[0-9]+)?/,
      repeat1(cont), repeat(colons),
    )] : []),
  );
}

const MODES = ['top', 'bracket', 'arith'];
const KEYWORDS = ['if', 'elsif', 'else', 'let', 'return', 'case'];
const OPERATORS = [
  ['||'], ['&&'], ['==', '!=', '<', '>', '<=', '>='], ['+', '-'], ['*', '/', '%'],
];
const LEAVES = ['identifier', 'integer', 'float', 'boolean', 'unit_literal',
  'string_single', 'string_double', 'block', 'list_literal', 'map_literal',
  'arith_expr', 'deref', 'deref_paren', 'force_brace', 'tilde'];
const node = ($, name, attached) => attached
  ? alias($[`_attached_${name}`], $[name]) : $[name];
const contextual = ($, name, mode, attached = false) =>
  alias($[`_${attached ? 'attached_' : ''}${name}_${mode}`], $[name]);
const value = ($, mode, attached = false) => $[`_${attached ? 'attached_' : ''}value_${mode}`];
const leaf = ($, name, mode, attached = false) => mode !== 'top' && name === 'unit_literal'
  ? alias($[attached ? '_attached_bracket_unit' : '_bracket_unit'], $.unit_literal)
  : mode === 'arith' && name === 'identifier'
    ? alias($[attached ? '_attached_arith_identifier' : '_arith_identifier'], $.identifier)
    : name === 'identifier' ? $[attached ? '_attached_value_label' : '_value_label'] : node($, name, attached);
const ws = rule => seq(optional(NL), rule, optional(NL));
const separated = rule => seq(ws(rule), repeat(seq(',', ws(rule))), optional(seq(',', optional(NL))));
const continued = op => seq(optional(NL), op, optional(NL));

// Immediate-start variants enforce the attachment after ! and ... without
// changing whitespace inside the attached atom or its nested delimiters.
function primary($, name, attached) {
  const start = text => attached ? token.immediate(text) : text;
  const lex = pattern => attached ? token.immediate(pattern) : token(pattern);
  switch (name) {
    case 'identifier': return lex(IDENT);
    case 'integer': return lex(INTEGER);
    case 'float': return lex(FLOAT);
    case 'boolean': return choice(start('true'), start('false'));
    case 'unit_literal': return seq(start('('), ')');
    case 'string_single': return choice(
      seq(start("'"), repeat(token.immediate(prec(1, /[^']+/))), token.immediate("'")),
      // Prefer a statement's bumped string over a continuation's comment.
      ...[1, 2, 3].map(n => prec.dynamic(1, node($, `bumped_string_${n}`, attached))),
    );
    case 'string_double': return seq(
      start('"'),
      optional(choice(
        alias(token.immediate(prec(2, '~')), $.tilde),
        token.immediate(prec(2, /([^"\\$!~]|~[^"\\$!\/])[^"\\$!]*/)),
      )),
      repeat(choice($.escape_sequence, $.interp_arith, $.interp_force,
        $.interp_deref_paren, $.interp_deref_index, $.interp_deref,
        $.interp_force_plain, token.immediate(prec(1, /[^"\\$!]+/)),
        token.immediate(prec(-1, choice('$', '!'))))),
      token.immediate('"'),
    );
    case 'block': return seq(start('{'), optional($.lambda_params), optional($._block_body), '}');
    // Spreads alone make a list; a later keyed entry selects the map parse.
    case 'list_literal': return prec.dynamic(1, seq(start('['), choice(optional(NL),
      separated(choice(value($, 'bracket'), contextual($, 'spread', 'bracket')))), ']'));
    case 'map_literal': return seq(start('['), choice(ws(':'), separated(choice($.map_entry,
      contextual($, 'spread', 'bracket')))), ']');
    case 'arith_expr': return seq(lex('$['), optional(NL), $._arith, ']');
    case 'deref': return lex(seq('$', DEREF_IDENT));
    case 'deref_paren': return seq(lex('$('), token.immediate(IDENT), token.immediate(')'));
    case 'force_brace': return seq(lex('!{'), optional($._block_body), '}');
    case 'tilde': return lex(choice(prec(1, seq('~/', /[a-zA-Z0-9_./-]*/)), '~'));
  }
  throw new Error(`unknown primary ${name}`);
}

function contextRules(mode) {
  const rules = {};
  for (const attached of [false, true]) {
    const prefix = `_${attached ? 'attached_' : ''}`;
    rules[`${prefix}word_${mode}`] = $ => attached
      ? token.immediate(words(mode)) : token(words(mode));
    rules[`${prefix}value_${mode}`] = $ => choice(
      contextual($, 'indexed', mode, attached),
      $[`${prefix}indexable_${mode}`],
      node($, 'deref_paren', attached),
      contextual($, 'force_bang', mode, attached),
      contextual($, 'tag', mode, attached),
    );
    rules[`${prefix}indexable_${mode}`] = $ => choice(
      ...LEAVES.filter(name => name !== 'deref_paren').map(name => leaf($, name, mode, attached)),
      contextual($, 'word', mode, attached),
      alias($[`${prefix}force_primary_${mode}`], $.force_bang),
    );
    rules[`${prefix}indexed_${mode}`] = $ => prec.left(2, seq(
      $[`${prefix}indexable_${mode}`], repeat1($.index),
    ));
    rules[`${prefix}tag_${mode}`] = $ => prec.right(seq(
      node($, 'tag_label', attached),
      optional(prec.right(3, seq(optional(mode === 'top' ? GAP : SPACE), value($, mode)))),
      optional(mode === 'top' ? GAP : SPACE),
    ));
    // Variable keys belong inside the force operand, other primary keys outside.
    rules[`${prefix}force_bang_${mode}`] = $ => seq(
      attached ? token.immediate('!') : '!', field('operand', choice(
        node($, 'deref_paren', true),
        alias($._attached_variable_index, $.indexed),
        node($, 'deref', true),
      )));
    rules[`${prefix}force_primary_${mode}`] = $ => seq(
      attached ? token.immediate('!') : '!',
      field('operand', choice(
        ...LEAVES.filter(name => !['deref', 'deref_paren'].includes(name))
          .map(name => leaf($, name, mode, true)),
        contextual($, 'word', mode, true), contextual($, 'tag', mode, true),
        contextual($, 'force_bang', mode, true),
        alias($[`_attached_force_primary_${mode}`], $.force_bang),
      )),
    );
  }
  rules[`_spread_${mode}`] = $ => seq(token(prec(2, '...')), value($, mode, true));
  return rules;
}

module.exports = grammar({
  name: 'ral',
  word: $ => $.identifier,
  extras: $ => [$.comment, /[ \t\r]/],
  // GLR retains separator and continuation readings until the next operator.
  // Keeping newlines as tokens also preserves intervening comment scopes.
  conflicts: $ => [
    [$.pipeline],
    [$.chain],

    [$._indexable_bracket, $.map_entry],
    [$._value_bracket, $.map_entry],
    [$._indexable_bracket, $._label],
    [$._indexable_bracket, $._static_key],
    [$._pattern, $._value_label],
    [$.boolean, $._label],
    [$.list_literal, $.map_literal],
    [$._attached_list_literal, $._attached_map_literal],
  ],
  rules: {
    source_file: $ => seq(optional(SEP), optional(seq(
      $.statement, repeat(seq(SEP, $.statement)), optional(SEP),
    ))),
    statement: $ => $.chain,
    chain: $ => seq($.pipeline, repeat(seq(continued('?'), $.pipeline))),
    pipeline: $ => seq($.cmd, repeat(seq(continued('|'), $.cmd))),
    cmd: $ => choice($.let_stmt, $.return_stmt, $.if_stmt, $.case_stmt, $.application),
    let_stmt: $ => seq('let', GAP, $._pattern, '=', optional(NL), $.chain),
    return_stmt: $ => prec.right(seq('return', optional(seq(GAP, value($, 'top'))), optional(GAP))),
    if_stmt: $ => prec.right(seq('if', SPACE, field('cond', value($, 'top')),
      SPACE, field('then', value($, 'top')), repeat($.elsif_clause), optional($.else_clause))),
    elsif_clause: $ => seq(
      choice('elsif', alias(token(seq(NL, /[ \t\r]*/, 'elsif')), 'elsif')),
      SPACE, field('cond', value($, 'top')), SPACE, field('then', value($, 'top'))),
    else_clause: $ => seq(
      choice('else', alias(token(seq(NL, /[ \t\r]*/, 'else')), 'else')),
      SPACE, field('body', value($, 'top'))),
    case_stmt: $ => seq('case', SPACE, field('scrutinee', value($, 'top')),
      optional(NL), '[', separated(field('arm', $.case_arm)), ']'),
    case_arm: $ => seq(field('tag', $.tag_label), optional(NL), ':', optional(NL),
      field('body', choice($.case_arm_lambda, $._case_arm_value))),
    _case_arm_value: $ => choice(
      ...LEAVES.filter(name => name !== 'block').map(name => leaf($, name, 'bracket')),
      contextual($, 'word', 'bracket'), contextual($, 'tag', 'bracket'),
      alias($._case_arm_index, $.indexed), contextual($, 'force_bang', 'bracket'),
      alias($._force_primary_bracket, $.force_bang)),
    _case_arm_index: $ => prec.left(2, seq(choice(
      ...LEAVES.filter(name => !['block', 'deref_paren'].includes(name))
        .map(name => leaf($, name, 'bracket')),
      contextual($, 'word', 'bracket'),
      alias($._force_primary_bracket, $.force_bang)), repeat1($.index))),
    case_arm_lambda: $ => seq('{', field('binder', alias($._case_arm_binder, $.lambda_params)),
      optional($._block_body), '}'),
    _case_arm_binder: $ => seq('|', $._pattern, '|'),
    // Keywords dispatch forms only at the head; elsewhere they are value words.
    _head_indexable: $ => choice(
      ...LEAVES.filter(name => name !== 'deref_paren').map(name => $[name]),
      contextual($, 'word', 'top'), alias($._force_primary_top, $.force_bang)),
    _head_indexed: $ => prec.left(2, seq($._head_indexable, repeat1($.index))),
    _head_value: $ => choice($._head_indexable, alias($._head_indexed, $.indexed),
      $.deref_paren, contextual($, 'force_bang', 'top'), contextual($, 'tag', 'top')),
    application: $ => prec.left(seq(choice($.bypass, $._head_value),
      repeat(choice($.redirect,
        seq(GAP, choice(value($, 'top'), contextual($, 'spread', 'top'), $.redirect)))),
      optional(GAP))),

    ...Object.fromEntries(LEAVES.flatMap(name => [
      [name, $ => primary($, name, false)],
      [`_attached_${name}`, $ => primary($, name, true)],
    ])),
    ...Object.assign({}, ...MODES.map(contextRules)),
    _arith_identifier: $ => /[a-zA-Z_][a-zA-Z0-9_]*/,
    _attached_arith_identifier: $ => token.immediate(/[a-zA-Z_][a-zA-Z0-9_]*/),
    _bracket_unit: $ => seq('(', optional(NL), ')'),
    _attached_bracket_unit: $ => seq(token.immediate('('), optional(NL), ')'),

    // Keys are words, not arbitrary atoms: no collection, block or application.
    index: $ => seq(token.immediate(prec(3, '[')), ws($._index_word), ']'),
    _index_word: $ => choice($._label, contextual($, 'word', 'bracket'),
      $.integer, $.float, $.boolean, $.string_single, $.string_double,
      $.deref, $.deref_paren, $.arith_expr, contextual($, 'force_bang', 'bracket'),
      contextual($, 'tag', 'bracket'), $.force_brace,
      alias($._force_primary_bracket, $.force_bang)),
    _attached_variable_index: $ => prec.left(3, seq(node($, 'deref', true), repeat1($.index))),

    _pattern: $ => choice($.identifier, $.wildcard, $.list_pattern, $.map_pattern),
    wildcard: $ => '_',
    list_pattern: $ => seq('[', choice(optional(NL), seq(choice(
      ws($.rest_pattern),
      seq(ws($._pattern), repeat(seq(',', ws($._pattern))),
        optional(seq(',', ws($.rest_pattern)))),
    ), optional(seq(',', optional(NL))))), ']'),
    rest_pattern: $ => seq('...', alias(token.immediate(IDENT), $.identifier)),
    map_pattern: $ => seq('[', separated($.map_pattern_entry), ']'),
    map_pattern_entry: $ => seq(field('key', $._static_key),
      optional(NL), ':', optional(NL), field('pattern', $._pattern)),

    redirect: $ => choice(seq($.redir_fd, $.fd_target),
      ...['append', 'stream', 'write', 'read', 'herestring'].map(op =>
        seq($[`redir_${op}`], value($, 'top')))),
    fd_target: $ => token(/[0-9]+/),
    redir_append: $ => token(seq(optional(/[0-9]+/), '>>')),
    redir_stream: $ => token(seq(optional(/[0-9]+/), '>~')),
    redir_write: $ => token(seq(optional(/[0-9]+/), '>')),
    redir_read: $ => token(seq(optional(/[0-9]+/), '<')),
    redir_fd: $ => token(seq(optional(/[0-9]+/), '>&')),
    redir_herestring: $ => token(seq(optional(/[0-9]+/), '<<')),

    lambda_params: $ => seq('|', repeat1($._pattern), '|'),
    _block_body: $ => choice(SEP, seq(optional(SEP), $.statement,
      repeat(seq(SEP, $.statement)), optional(SEP))),
    map_entry: $ => seq(field('key', choice($._static_key, $.string_double,
      $.deref, $.deref_paren)), optional(NL), ':', optional(NL), field('value', value($, 'bracket'))),
    _static_key: $ => choice($._label, $.string_single),
    _label: $ => choice($._value_label, alias('true', $.identifier), alias('false', $.identifier)),
    _value_label: $ => choice($.identifier, ...KEYWORDS.map(word => alias(word, $.identifier))),
    _attached_value_label: $ => choice(node($, 'identifier', true),
      ...KEYWORDS.map(word => alias(token.immediate(word), $.identifier))),

    tag_label: $ => token(seq('`', IDENT)),
    _attached_tag_label: $ => token.immediate(seq('`', IDENT)),

    _arith: $ => choice($.arith_binary, $.arith_negate, $.arith_not, $._arith_atom),
    // Consume whitespace at the atom, so operator precedence sees the operator.
    _arith_atom: $ => seq(choice($.arith_group, value($, 'arith')), optional(NL)),
    arith_binary: $ => choice(...OPERATORS.map((ops, i) => prec.left(i + 1,
      seq($._arith, field('op', ops.length === 1 ? ops[0] : choice(...ops)), optional(NL), $._arith)))),
    arith_negate: $ => prec(6, seq('-', optional(NL), $._arith)),
    arith_not: $ => prec(6, seq('not', SPACE, $._arith)),
    arith_group: $ => seq('(', optional(NL), $._arith, ')'),

    bypass: $ => seq('^', alias(token.immediate(choice(IDENT, words('top'))), $.word)),
    escape_sequence: $ => token.immediate(seq('\\',
      choice(/[nrte\\0"$!~]/, /x[0-9a-fA-F]{2}/, /u\{[0-9a-fA-F]{1,6}\}/, /\r\n?|\n/))),
    interp_arith: $ => seq(token.immediate('$['), optional(NL), $._arith, ']', repeat($.index)),
    interp_force: $ => seq(token.immediate('!{'), optional($._block_body), '}', repeat($.index)),
    interp_deref_paren: $ => node($, 'deref_paren', true),
    interp_deref_index: $ => seq(node($, 'deref', true), repeat1($.index)),
    interp_deref: $ => token.immediate(seq('$', DEREF_IDENT)),
    interp_force_plain: $ => choice(
      seq(token.immediate('!$'), field('operand', choice(
        alias($._interp_forced_index, $.indexed), alias(token.immediate(DEREF_IDENT), $.deref)))),
      seq(token.immediate('!$('), field('operand', alias($._interp_forced_name, $.deref_paren))),
      seq(token.immediate('!$['), field('operand', alias($._interp_forced_expr, $.arith_expr)), repeat($.index))),
    _interp_forced_index: $ => prec.left(3, seq(
      alias(token.immediate(DEREF_IDENT), $.deref), repeat1($.index))),
    _interp_forced_name: $ => seq(token.immediate(IDENT), token.immediate(')')),
    _interp_forced_expr: $ => seq(optional(NL), $._arith, ']'),

    // Bounded fences; shorter quote/hash runs cannot consume a closing fence.
    ...Object.fromEntries([1, 2, 3].flatMap(n => {
      const fence = '#'.repeat(n);
      const short = new RegExp(`#{0,${n - 1}}`);
      const quotes = seq("'", repeat(seq(short, "'")));
      const pattern = seq(fence + "'",
        repeat(choice(/[^']/, seq(quotes, short, /[^#']/))), quotes, fence);
      return [
        [`bumped_string_${n}`, $ => token(prec(2, pattern))],
        [`_attached_bumped_string_${n}`, $ => token.immediate(prec(2, pattern))],
      ];
    })),
    comment: $ => token(seq('#', /[^\n]*/)),
  },
});
