//! Temporary Step 0 oracle; session units carry accepted Defines only.

use super::{SessionSchemes, Unifier, typecheck};
use crate::compile::{CompileError, on_compile_stack};
use crate::elaborator::elaborate;
use crate::ir::{Comp, CompKind, FieldsNode, Name, Node, Phrase, Toplevel, Unchecked, Val};
use crate::shell::boot::{BakedPrelude, HostSurface};
use crate::shell::builtins::{SURFACE_BUILTIN, WATCH_BUILTIN};
use crate::source::{FileId, Spanned, normalize_source_text};
use crate::syntax::parser::parse_with;
use crate::ty::{Site, UnitId};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

type Checked = Result<Toplevel, CompileError>;

const FIXTURES: [(&str, &str); 12] = [
    (
        "01-split",
        "let maker = { return { |x| return $x } }; maker 1",
    ),
    (
        "02-let-fun",
        "let f = { |a b| return $a }; let g = f 1; return ()",
    ),
    ("03-captures", "let x = hostname; return !{hostname}"),
    (
        "04-under-applied-boundary",
        "let at = from-json-at; return $at",
    ),
    (
        "05-boundary-value",
        "let decoder = $from-json; return $decoder",
    ),
    ("06-imported-exec", "golden-decoder '0'"),
    ("07-imported-values", IMPORTS),
    (
        "08-shared-group",
        "let f = { |x| g $x }; let g = { |y| f $y }",
    ),
    ("09-protect", "return 1"),
    ("10-protected-recheck", IMPORTS),
    (
        "11-upcast-in-hand",
        "let t = { echo a }; let u = { return () }; if true $t else $u",
    ),
    ("12-map-in-hand", "let g = { |f| echo $f }; map $g [1, 2]"),
];
const IMPORTS: &str = "let d = from-json; let decoder = { |s| to-string $s | from-json }; let k = length $d; let box = [f: $decoder]";
const EXEC_SEED: &str = "let golden-decoder = { |s| to-string $s | from-json }";

#[test]
#[ignore = "writes the Step 0 oracle to RAL_GOLDEN_DIR"]
fn capture_at_let_golden() {
    on_compile_stack(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let output = PathBuf::from(std::env::var_os("RAL_GOLDEN_DIR").expect("set RAL_GOLDEN_DIR"));
        fs::create_dir_all(&output).unwrap();
        let git = Command::new("git")
            .current_dir(root)
            .args(["ls-files", "-co", "--exclude-standard", "*.ral"])
            .output()
            .unwrap();
        assert!(git.status.success(), "git ls-files: {:?}", git.stderr);
        let mut paths: Vec<_> = String::from_utf8(git.stdout)
            .unwrap()
            .lines()
            .filter(|p| !p.starts_with("exarch/") && !p.starts_with("plugins/"))
            .map(str::to_owned)
            .collect();
        paths.sort();
        paths.dedup();
        fs::write(
            output.join("corpus-paths.txt"),
            format!("{}\n", paths.join("\n")),
        )
        .unwrap();

        let prelude = BakedPrelude::runtime().comp();
        let surface = HostSurface {
            statics: vec![WATCH_BUILTIN, SURFACE_BUILTIN],
            ..HostSurface::default()
        };
        let seed = SessionSchemes::from_prelude(prelude, surface.manifest());
        let mut baked = Dump::default();
        baked.unit("baked", &Ok(prelude.as_ref().clone()));
        baked.save(&output, "baked-prelude", "whole");
        assert_source(include_str!("../shell/prelude.ral"));
        for path in paths {
            let source = normalize_source_text(fs::read_to_string(root.join(&path)).unwrap());
            record(
                &output,
                &path,
                &source,
                Some(Path::new(&path)),
                &seed,
                false,
                false,
            );
        }
        assert_source(EXEC_SEED);
        let setup = front_end(EXEC_SEED, None, &seed, false).unwrap();
        let setup = typecheck(&setup, seed.clone(), None).expect("imported Exec seed");
        let exec_seed = extend_seed(&seed, &setup);
        for (name, source) in FIXTURES {
            let imported_exec = name == "06-imported-exec";
            record(
                &output,
                &format!("fixtures/{name}"),
                source,
                None,
                if imported_exec { &exec_seed } else { &seed },
                true,
                imported_exec,
            );
        }
    });
}

fn assert_source(source: &str) {
    let candidate = Regex::new(r"%[A-Za-z][A-Za-z_-]*[0-9]+").unwrap();
    assert!(
        !candidate.is_match(source),
        "source contains a synthetic-name candidate"
    );
}

fn front_end(
    source: &str,
    script: Option<&Path>,
    seed: &SessionSchemes,
    imported_exec: bool,
) -> Result<Unchecked, CompileError> {
    let ast = parse_with(source, FileId::DUMMY).map_err(CompileError::Parse)?;
    if imported_exec {
        elaborate(&ast, [], script)
    } else {
        elaborate(
            &ast,
            seed.bindings.iter().map(|(n, _)| Name::from(n.as_str())),
            script,
        )
    }
    .map_err(CompileError::Parse)
}

fn extend_seed(seed: &SessionSchemes, top: &Toplevel) -> SessionSchemes {
    let mut bindings: BTreeMap<_, _> = seed.bindings.iter().cloned().collect();
    bindings.extend(
        top.exported_schemes()
            .into_iter()
            .map(|(n, s)| (n, Some(s))),
    );
    SessionSchemes {
        bindings: bindings.into_iter().collect(),
        ..seed.clone()
    }
}

fn strip_schemes(top: &Toplevel) -> Unchecked {
    top.phrases
        .iter()
        .map(|p| {
            Spanned::with_span(
                p.span,
                match &p.item {
                    Phrase::Define { pattern, comp, .. } => Phrase::Define {
                        pattern: Arc::clone(pattern),
                        comp: Arc::clone(comp),
                        schemes: (),
                    },
                    Phrase::Run(comp) => Phrase::Run(Arc::clone(comp)),
                },
            )
        })
        .collect()
}

fn protected(phrases: &Unchecked) -> Unchecked {
    phrases
        .iter()
        .map(|p| {
            Spanned::with_span(
                p.span,
                match &p.item {
                    Phrase::Define { .. } => p.item.clone(),
                    Phrase::Run(comp) => {
                        let mut u = Unifier::new();
                        let ty = u.fresh_ty();
                        let site = Arc::new(Site::snapshot(&u, &ty, UnitId::fresh()));
                        Phrase::Run(Arc::new(Spanned::with_span(
                            comp.span,
                            CompKind::Protect {
                                body: Arc::clone(comp),
                                site,
                            },
                        )))
                    }
                },
            )
        })
        .collect()
}

fn record(
    output: &Path,
    path: &str,
    source: &str,
    script: Option<&Path>,
    seed: &SessionSchemes,
    fixture: bool,
    imported_exec: bool,
) {
    assert_source(source);
    let input = front_end(source, script, seed, imported_exec);
    let mut whole = Dump::default();
    let mut session = Dump::default();
    let mut recheck = Dump::default();
    let mut protect = Dump::default();
    match input {
        Err(error) => {
            whole.unit("whole", &Err(error.clone()));
            session.unit("front-end", &Err(error));
        }
        Ok(phrases) => {
            let result = typecheck(&phrases, seed.clone(), None).map_err(CompileError::Types);
            whole.unit("whole", &result);
            recheck_unit(&mut recheck, "whole", &result, seed);
            if fixture {
                assert!(result.is_ok(), "{path} whole: {result:#?}");
                let result = typecheck(&protected(&phrases), seed.clone(), None)
                    .map_err(CompileError::Types);
                assert!(result.is_ok(), "{path} protect: {result:#?}");
                protect.unit("protect", &result);
            }
            let mut current = seed.clone();
            // Refusal exports nothing; continue with earlier accepted Defines.
            for (i, phrase) in phrases.into_iter().enumerate() {
                let label = format!("session-{i}");
                let result =
                    typecheck(&[phrase], current.clone(), None).map_err(CompileError::Types);
                session.unit(&label, &result);
                recheck_unit(&mut recheck, &label, &result, &current);
                if fixture {
                    assert!(result.is_ok(), "{path} {label}: {result:#?}");
                }
                if let Ok(top) = result {
                    current = extend_seed(&current, &top);
                }
            }
        }
    }
    whole.save(output, path, "whole");
    session.save(output, path, "session");
    recheck.save(output, path, "recheck");
    if fixture {
        protect.save(output, path, "protect");
    }
}

fn recheck_unit(dump: &mut Dump, label: &str, result: &Checked, seed: &SessionSchemes) {
    if let Ok(top) = result {
        let checked =
            typecheck(&strip_schemes(top), seed.clone(), None).map_err(CompileError::Types);
        dump.unit(label, &checked);
    }
}

#[derive(Default)]
struct Dump {
    full: String,
    terms: String,
    full_names: Normalize,
    term_names: Normalize,
}

impl Dump {
    fn unit(&mut self, label: &str, result: &Checked) {
        writeln!(self.full, "=== {label} ===").unwrap();
        writeln!(self.terms, "=== {label} ===").unwrap();
        match result {
            Err(error) => {
                writeln!(self.full, "{error:#?}").unwrap();
                writeln!(self.terms, "{error:#?}").unwrap();
            }
            Ok(top) => {
                let raw = serde_json::to_value(top).unwrap();
                let mut full = raw.clone();
                self.full_names.apply(&mut full);
                writeln!(
                    self.full,
                    "{}",
                    serde_json::to_string_pretty(&full).unwrap()
                )
                .unwrap();
                let mut walk = Stored::default();
                for p in &top.phrases {
                    match &p.item {
                        Phrase::Define { comp, .. } | Phrase::Run(comp) => walk.comp(comp),
                    }
                }
                for (i, (kind, names)) in walk.occ.into_iter().enumerate() {
                    let mut names: Vec<_> =
                        names.iter().map(|n| self.full_names.renamed(n)).collect();
                    names.sort();
                    writeln!(
                        self.full,
                        "occ {i} {kind}: {}",
                        serde_json::to_string(&names).unwrap()
                    )
                    .unwrap();
                }
                writeln!(
                    self.full,
                    "rec-groups {label}: {}",
                    serde_json::to_string(&walk.rec).unwrap()
                )
                .unwrap();
                let mut terms = raw;
                erase(&mut terms);
                self.term_names.apply(&mut terms);
                writeln!(
                    self.terms,
                    "{}",
                    serde_json::to_string_pretty(&terms).unwrap()
                )
                .unwrap();
            }
        }
    }

    fn save(&self, output: &Path, path: &str, mode: &str) {
        let full = output.join(format!("{path}.{mode}.txt"));
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, &self.full).unwrap();
        fs::write(output.join(format!("{path}.{mode}.terms.txt")), &self.terms).unwrap();
    }
}

struct Normalize {
    synthetic: Regex,
    names: HashMap<String, String>,
    tags: HashMap<String, usize>,
    units: HashMap<(u64, u64), usize>,
}

impl Default for Normalize {
    fn default() -> Self {
        Self {
            synthetic: Regex::new(r"^%([A-Za-z][A-Za-z_-]*)([0-9]+)$").unwrap(),
            names: HashMap::new(),
            tags: HashMap::new(),
            units: HashMap::new(),
        }
    }
}

impl Normalize {
    fn apply(&mut self, value: &mut Value) {
        match value {
            Value::String(s) => {
                if let Some(c) = self.synthetic.captures(s) {
                    let tag = c[1].to_owned();
                    *s = self
                        .names
                        .entry(s.clone())
                        .or_insert_with(|| {
                            let next = self.tags.entry(tag.clone()).or_default();
                            let name = format!("%{tag}{next}");
                            *next += 1;
                            name
                        })
                        .clone();
                }
            }
            Value::Array(a) => {
                for v in a {
                    self.apply(v);
                }
            }
            Value::Object(o) => {
                // Only Site.unit, never arbitrary ids or user data.
                if o.len() == 3
                    && o.contains_key("nodes")
                    && o.contains_key("root")
                    && o.contains_key("unit")
                {
                    let unit = &o["unit"];
                    let key = (
                        unit["process"].as_u64().unwrap(),
                        unit["seq"].as_u64().unwrap(),
                    );
                    let next = self.units.len();
                    let id = *self.units.entry(key).or_insert(next);
                    o.insert("unit".into(), json!({"process": 0, "seq": id}));
                }
                for v in o.values_mut() {
                    self.apply(v);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    fn renamed(&self, name: &str) -> String {
        if self.synthetic.is_match(name) {
            self.names
                .get(name)
                .expect("occ name appears in JSON")
                .clone()
        } else {
            name.to_owned()
        }
    }
}

fn erase(value: &mut Value) {
    if let Some(body) = value.get("item").and_then(|i| i.get("Upcast")) {
        *value = body.clone();
        erase(value);
        return;
    }
    match value {
        Value::Object(o) => {
            o.remove("schemes");
            o.remove("admits");
            for (key, v) in o.iter_mut() {
                if key == "site" {
                    *v = Value::Null;
                } else {
                    erase(v);
                }
            }
        }
        Value::Array(a) => {
            for v in a {
                erase(v);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    if let Some(reduced) = eta(value) {
        *value = reduced;
    }
}

fn eta(value: &Value) -> Option<Value> {
    let mut body = value.get("Thunk")?;
    let mut params = Vec::new();
    while let Some(lam) = body.get("item")?.get("Lam") {
        let param = lam.get("param")?.as_str()?;
        if !param
            .strip_prefix("%eta")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        params.push(param);
        body = lam.get("body")?;
    }
    let item = body.get("item")?;
    if params.is_empty() {
        return item.get("Force").cloned();
    }
    let app = item.get("App")?;
    let args = app.get("args")?.as_array()?;
    if args.len() != params.len()
        || !args.iter().zip(params).all(|(a, p)| {
            a.get("item")
                .and_then(|v| v.get("Variable"))
                .and_then(Value::as_str)
                == Some(p)
        })
    {
        return None;
    }
    app.get("head")?.get("item")?.get("Force").cloned()
}

#[derive(Default)]
struct Stored {
    occ: Vec<(&'static str, Vec<String>)>,
    groups: HashMap<usize, usize>,
    rec: Vec<(usize, usize)>,
}

impl Stored {
    fn node<S>(&mut self, kind: &'static str, node: &Node<S>) {
        self.occ
            .push((kind, node.occ().names().map(ToString::to_string).collect()));
    }

    fn fields(&mut self, node: &FieldsNode) {
        self.node("fields", node);
        for (_, v) in node.shape() {
            self.val(&v.item);
        }
    }

    fn val(&mut self, value: &Val) {
        match value {
            Val::Thunk(node) => {
                self.node("thunk", node);
                self.comp(node.shape());
            }
            Val::List(node) => {
                self.node("list", node);
                for v in node.shape() {
                    self.val(&v.item);
                }
            }
            Val::Record(node) | Val::Map(node) => self.fields(node),
            Val::Variant { payload, .. } => self.val(payload),
            Val::Protected { value, .. } => self.val(value),
            Val::Unit
            | Val::String(_)
            | Val::Int(_)
            | Val::Float(_)
            | Val::Bool(_)
            | Val::Variable(_) => {}
        }
    }

    fn comp(&mut self, comp: &Comp) {
        match &comp.item {
            CompKind::Return(v) | CompKind::Force(v) => self.val(v),
            CompKind::Lam { body, .. }
            | CompKind::Capture(body)
            | CompKind::Upcast(body)
            | CompKind::Protect { body, .. } => self.comp(body),
            CompKind::App { head, args } => {
                self.comp(head);
                for arg in args {
                    self.val(&arg.item);
                }
            }
            CompKind::Bind { comp, rest, .. } | CompKind::Seq { first: comp, rest } => {
                self.comp(comp);
                self.comp(rest);
            }
            CompKind::Rec { group, index } => {
                let next = self.groups.len();
                let id = *self
                    .groups
                    .entry(Arc::as_ptr(group) as usize)
                    .or_insert(next);
                self.rec.push((id, *index));
                self.node("group", group);
                for (_, body) in group.shape() {
                    self.comp(body);
                }
            }
            CompKind::If { cond, then, else_ } => {
                self.val(&cond.item);
                self.val(&then.item);
                self.val(&else_.item);
            }
            CompKind::Case { scrutinee, arms } => {
                self.val(&scrutinee.item);
                for arm in arms {
                    self.val(&arm.body.item);
                }
            }
            CompKind::Try { body, handler }
            | CompKind::Guard {
                body,
                cleanup: handler,
            } => {
                self.val(body);
                self.val(handler);
            }
            CompKind::Prim(prim) => {
                for v in prim.operands() {
                    self.val(v);
                }
            }
            CompKind::Exec(exec) => {
                for arg in &exec.argv {
                    self.val(&arg.slot().item);
                }
                for v in exec.redirects.operands() {
                    self.val(v);
                }
            }
            CompKind::Pipeline { stages } => {
                for stage in stages {
                    self.comp(stage);
                }
            }
            CompKind::Redirect { body, redirects } => {
                self.comp(body);
                for v in redirects.operands() {
                    self.val(v);
                }
            }
            CompKind::Within {
                opts,
                handlers,
                body,
            } => {
                self.fields(opts);
                for arm in handlers.iter().flatten() {
                    self.val(&arm.value.item);
                }
                self.val(body);
            }
            CompKind::Grant { caps, body } => {
                self.fields(caps);
                self.val(body);
            }
            CompKind::Spawn { title, body } => {
                if let Some(t) = title {
                    self.val(&t.item);
                }
                self.val(body);
            }
            CompKind::Gather { body } | CompKind::Audit { body } => self.val(body),
            CompKind::Tilde(_) => {}
            CompKind::Native { .. } => unreachable!("native in checked source"),
        }
    }
}
