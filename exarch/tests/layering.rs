#![allow(clippy::disallowed_methods)]

use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

exarch::pre_main_ctor!();

type Graph = BTreeMap<String, BTreeSet<String>>;

const SKIPPED: [&str; 4] = ["tests.rs", "wire_tests.rs", "lost.rs", "testkit.rs"];

const LAYERED: [(&str, &str); 6] = [
    ("provider", "record"),
    ("record", "bus"),
    ("bus", "agent"),
    ("shell_eval", "agent"),
    ("agent", "tui"),
    ("agent", "headless"),
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for path in std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()) {
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() {
            if name != "tests" {
                sources(&path, out);
            }
        } else if name.ends_with(".rs") && !SKIPPED.contains(&&*name) {
            out.push(path);
        }
    }
}

fn module_of(src: &Path, file: &Path) -> String {
    let first = file.strip_prefix(src).unwrap().iter().next().unwrap();
    match first.to_string_lossy().trim_end_matches(".rs") {
        "main" => "lib".into(),
        m => m.into(),
    }
}

fn production_code(text: &str) -> String {
    let inline_tests =
        Regex::new(r"#\[cfg\(test\)\]\s*(pub(\([a-z]+\))?\s+)?mod\s+\w+\s*\{").unwrap();
    let live = inline_tests.find(text).map_or(text, |m| &text[..m.start()]);
    live.lines()
        .map(|line| line.split("//").next().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

fn group_heads(rest: &str) -> impl Iterator<Item = String> {
    let (mut depth, mut top) = (0, String::new());
    for c in rest.chars() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => break,
            '}' => depth -= 1,
            c if depth == 0 => top.push(c),
            _ => {}
        }
    }
    top.split(',')
        .map(|item| {
            item.trim()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        })
        .collect::<Vec<String>>()
        .into_iter()
}

fn crate_refs(code: &str) -> Vec<String> {
    let reference = Regex::new(r"crate::(?:(\w+)|\{)").unwrap();
    reference
        .captures_iter(code)
        .flat_map(|c| match c.get(1) {
            Some(name) => vec![name.as_str().to_owned()],
            None => group_heads(&code[c.get(0).unwrap().end()..]).collect(),
        })
        .collect()
}

fn path<'a>(g: &'a Graph, from: &'a str, to: &str) -> Option<Vec<&'a str>> {
    fn go<'a>(
        g: &'a Graph,
        at: &'a str,
        to: &str,
        seen: &mut BTreeSet<&'a str>,
    ) -> Option<Vec<&'a str>> {
        if at == to {
            return Some(vec![at]);
        }
        seen.insert(at).then_some(())?;
        g[at].iter().find_map(|next| {
            let mut tail = go(g, next, to, seen)?;
            tail.insert(0, at);
            Some(tail)
        })
    }
    go(g, from, to, &mut BTreeSet::new())
}

/// Exarch's top-level modules form a DAG, layered as the pairs in `LAYERED` say.
/// `docs/ral-wiki/invariants/exarch-is-a-dag.md` is the prose of this invariant.
#[test]
fn exarch_modules_form_a_dag() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    let modules: BTreeSet<String> = files.iter().map(|f| module_of(&src, f)).collect();
    let mut graph: Graph = modules
        .iter()
        .map(|m| (m.clone(), BTreeSet::new()))
        .collect();
    for file in &files {
        let module = module_of(&src, file);
        let code = production_code(&std::fs::read_to_string(file).unwrap());
        let deps = crate_refs(&code)
            .into_iter()
            .filter(|d| *d != module && modules.contains(d));
        graph.get_mut(&module).unwrap().extend(deps);
    }

    for (module, deps) in &graph {
        for dep in deps {
            if let Some(back) = path(&graph, dep, module) {
                panic!(
                    "cycle {module} -> {}: cut one of these edges",
                    back.join(" -> ")
                );
            }
        }
    }
    for (below, above) in LAYERED {
        assert!(
            graph.contains_key(below) && graph.contains_key(above),
            "no module `{below}` or `{above}`"
        );
        if let Some(p) = path(&graph, below, above) {
            panic!("`{below}` must sit below `{above}`, yet {}", p.join(" -> "));
        }
    }
}
