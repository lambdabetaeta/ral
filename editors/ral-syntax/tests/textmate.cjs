const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const textmate = require('vscode-textmate');
const oniguruma = require('vscode-oniguruma');

async function main() {
  await oniguruma.loadWASM(fs.readFileSync(require.resolve('vscode-oniguruma/release/onig.wasm')));
  const registry = new textmate.Registry({
    onigLib: Promise.resolve({
      createOnigScanner: patterns => new oniguruma.OnigScanner(patterns),
      createOnigString: text => new oniguruma.OnigString(text),
    }),
    loadGrammar: async () => textmate.parseRawGrammar(
      fs.readFileSync(path.join(__dirname, '../ral.tmLanguage.json'), 'utf8'),
      'ral.tmLanguage.json',
    ),
  });
  const grammar = await registry.loadGrammar('source.ral');
  const fixtures = JSON.parse(fs.readFileSync(path.join(__dirname, 'scopes.json'), 'utf8'));
  for (const { name, source, scopes } of fixtures) {
    const ranges = new Map();
    let state = textmate.INITIAL;
    let offset = 0;
    for (const line of source.match(/[^\n]*\n|[^\n]+$/g) ?? []) {
      const result = grammar.tokenizeLine(line.replace(/\n$/, ''), state);
      state = result.ruleStack;
      for (const token of result.tokens) {
        for (const scope of new Set(token.scopes)) {
          const spans = ranges.get(scope) ?? [];
          const start = offset + token.startIndex;
          const end = offset + Math.min(token.endIndex, line.length);
          if (start === end) continue;
          if (spans.at(-1)?.[1] === start) spans.at(-1)[1] = end;
          else spans.push([start, end]);
          ranges.set(scope, spans);
        }
      }
      offset += line.length;
    }
    for (const [scope, expected] of Object.entries(scopes)) {
      const actual = (ranges.get(scope) ?? []).map(([start, end]) => source.slice(start, end));
      assert.deepEqual(actual, expected, `${name}: ${scope}`);
    }
  }
  registry.dispose();
  console.log(`TextMate: ${fixtures.length} scope fixtures passed`);
}

main().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
