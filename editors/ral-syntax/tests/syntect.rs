use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
};

use serde_json::Value;
use syntect::{
    parsing::{ParseState, ScopeStack, SyntaxDefinition, SyntaxSetBuilder},
    util::LinesWithEndings,
};

fn main() {
    let root = env::args()
        .nth(1)
        .expect("usage: syntect-scopes <grammar-directory>");
    let yaml = fs::read_to_string(format!("{root}/ral.sublime-syntax")).unwrap();
    let mut builder = SyntaxSetBuilder::new();
    builder.add(SyntaxDefinition::load_from_str(&yaml, true, None).unwrap());
    let syntaxes = builder.build();
    let syntax = syntaxes.find_syntax_by_extension("ral").unwrap();
    let fixtures: Vec<Value> =
        serde_json::from_str(&fs::read_to_string(format!("{root}/tests/scopes.json")).unwrap())
            .unwrap();

    for fixture in &fixtures {
        let name = fixture["name"].as_str().unwrap();
        let source = fixture["source"].as_str().unwrap();
        let mut parser = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        let mut ranges: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
        let mut offset = 0;
        for line in LinesWithEndings::from(source) {
            let operations = parser.parse_line(line, &syntaxes).unwrap();
            let mut cursor = 0;
            for (at, operation) in operations {
                append(&mut ranges, &stack, offset + cursor, offset + at);
                stack.apply(&operation).unwrap();
                cursor = at;
            }
            append(&mut ranges, &stack, offset + cursor, offset + line.len());
            offset += line.len();
        }
        for (scope, expected) in fixture["scopes"].as_object().unwrap() {
            let actual: Vec<&str> = ranges
                .get(scope)
                .into_iter()
                .flatten()
                .map(|&(start, end)| &source[start..end])
                .collect();
            let expected: Vec<&str> = expected
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap())
                .collect();
            assert_eq!(actual, expected, "{name}: {scope}");
        }
    }
    println!("syntect: {} scope fixtures passed", fixtures.len());
}

fn append(
    ranges: &mut BTreeMap<String, Vec<(usize, usize)>>,
    stack: &ScopeStack,
    start: usize,
    end: usize,
) {
    if start == end {
        return;
    }
    let scopes: BTreeSet<_> = stack.scopes.iter().map(ToString::to_string).collect();
    for scope in scopes {
        let spans = ranges.entry(scope).or_default();
        match spans.last_mut() {
            Some((_, previous_end)) if *previous_end == start => *previous_end = end,
            _ => spans.push((start, end)),
        }
    }
}
