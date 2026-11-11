//! The module graph of exarch: a layered DAG, whose frontends reach agents through the port.

use ral_lint::{module_path, production_code, root_refs, rust_files, workspace_root};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

type Graph = BTreeMap<String, BTreeSet<String>>;

/// Modules that are test scaffolding, out of line.
const SKIPPED: [&str; 2] = ["tests", "testkit"];

const LAYERED: [(&str, &str); 16] = [
    ("provider", "record"),
    ("record", "bus"),
    ("bus", "agent"),
    ("bus", "schedule"),
    ("schedule", "agent"),
    ("shell_eval", "agent"),
    ("agent", "tui"),
    ("agent", "headless"),
    ("agent", "host"),
    ("host", "tui"),
    ("host", "headless"),
    ("agent", "port"),
    ("bus", "port"),
    ("port", "host"),
    ("port", "tui"),
    ("port", "headless"),
];

fn sources(src: &Path) -> Vec<PathBuf> {
    rust_files(src)
        .into_iter()
        .filter(|file| {
            module_path(src, file)
                .iter()
                .all(|part| !SKIPPED.contains(&part.as_str()))
        })
        .collect()
}

fn module_of(src: &Path, file: &Path) -> String {
    let first = module_path(src, file).swap_remove(0);
    if first == "main" { "lib".into() } else { first }
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

fn graph() -> Graph {
    let src = workspace_root().join("exarch/src");
    let files = sources(&src);
    let modules: BTreeSet<String> = files.iter().map(|f| module_of(&src, f)).collect();
    let mut graph: Graph = modules
        .iter()
        .map(|m| (m.clone(), BTreeSet::new()))
        .collect();
    for file in &files {
        let module = module_of(&src, file);
        let code = production_code(&std::fs::read_to_string(file).unwrap());
        let deps = root_refs(&code, 0)
            .into_iter()
            .filter(|d| *d != module && modules.contains(d));
        graph.get_mut(&module).unwrap().extend(deps);
    }

    graph
}

/// Exarch's top-level modules form a DAG, layered as the pairs in `LAYERED` say.
/// `docs/ral-wiki/invariants/exarch-is-a-dag.md` is the prose of this invariant.
#[test]
fn exarch_modules_form_a_dag() {
    let graph = graph();
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

#[test]
fn frontends_reach_agents_through_the_port() {
    let graph = graph();
    for (m, forbidden) in [
        ("tui", &["agent", "bus", "host"][..]),
        ("headless", &["agent", "bus"][..]),
    ] {
        for d in forbidden {
            assert!(
                !graph[m].contains(*d),
                "`{m}` names `{d}` directly: reach agents through `port`"
            );
        }
    }
}

#[test]
fn control_commits_belong_to_record_and_bus() {
    let src = workspace_root().join("exarch/src");
    let capability = Regex::new(r"\bcommit_to\s*\(|\bFolds\s*\{").unwrap();
    for file in sources(&src) {
        if file.starts_with(src.join("record")) || file.starts_with(src.join("bus")) {
            continue;
        }
        let code = production_code(&std::fs::read_to_string(&file).unwrap());
        assert!(
            !capability.is_match(&code),
            "{} constructs a control commit outside record/ and bus/",
            file.display()
        );
    }
}
