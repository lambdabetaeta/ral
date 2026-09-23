//! The REPL-side worksheet model: per-binding dependency edges and the
//! pure/effectful verdict, retained across runs.
//!
//! After evaluation the `Env` stores [`Value`](ral_core::Value)s, not ASTs,
//! so the free-reference edges between bindings — computed at compile time —
//! are discarded.  Rather than add a field to core's `Binding`, this parallel
//! REPL-side model records `(name, free_refs, effectful)` at each successful
//! top-level `Bind`, owned by the [`Session`](super::session) so it
//! accumulates as the user defines bindings.  The node's name, type, and value
//! preview still come from the engine each `read`; this model supplements
//! that live data with the edges and the effect verdict the env cannot
//! reconstruct.
//!
//! Neither analysis is reimplemented here.  The edges reuse
//! [`Ast::free_refs`](ral_core::syntax::ast) — the same free-variable
//! analysis `syntax::group` (private to `ral_core`) uses to form a `Rec`
//! group, a pure parse.  The effect verdict is the engine's `bind-effects`
//! reading: the mode-system verdict the typechecker already records, not a
//! new heuristic.
//!
//! A read-only projection of edges and classification: it records, the
//! frontend renders a dependency tree, and nothing re-evaluates.

use ral_core::protocol::reading::BindEffect;
use ral_core::syntax::ast::Ast;

use std::collections::HashSet;

/// One worksheet node: a user binding's dependency edges and effect verdict.
///
/// The name/type/value preview are *not* held here — they come from the
/// engine each `read`.  This entry carries only what the env cannot
/// reconstruct: the names this binding's RHS referenced freely (its inbound
/// dependency edges) and whether it is effectful.
pub(super) struct WsEntry {
    /// The bound name.
    pub(super) name: String,
    /// The names this binding's RHS referenced freely — the dependency
    /// edges into this node.  A subset of the binding names known at record
    /// time; an edge to a name not (yet) in the worksheet is dropped.
    pub(super) free_refs: HashSet<String>,
    /// Whether the binding's RHS is effectful per the checker's verdict.
    pub(super) effectful: bool,
}

/// The accumulating worksheet model, owned by the [`Session`](super::session).
///
/// Entries are kept in first-definition order; re-binding an existing name
/// updates its entry in place rather than appending a duplicate.
#[derive(Default)]
pub(super) struct Worksheet {
    entries: Vec<WsEntry>,
}

impl Worksheet {
    /// The recorded entries, in first-definition order.
    pub(super) fn entries(&self) -> &[WsEntry] {
        &self.entries
    }

    /// Record the top-level bindings of a *successfully evaluated* input,
    /// with the engine's `effects` verdict for it. Called from the success arm
    /// of the eval path. A top-level `let name = rhs` with a `Name` pattern
    /// yields one entry; other statements are ignored.
    pub(super) fn record(&mut self, input: &str, effects: &[BindEffect]) {
        let Ok(stmts) = ral_core::syntax::parser::parse(input) else {
            return;
        };

        // The candidate set for edge analysis: every name the worksheet
        // already knows, plus the names this input binds (so a mutually-
        // referencing run records edges among its own bindings).  Edges to
        // names outside this set — command heads, prelude functions — are
        // not dependency edges between user bindings and are dropped.
        let mut candidates: HashSet<String> = self.entries.iter().map(|e| e.name.clone()).collect();
        for stmt in &stmts {
            if let Some((name, _)) = top_level_let(&stmt.item) {
                candidates.insert(name.to_string());
            }
        }

        for stmt in &stmts {
            let Some((name, value)) = top_level_let(&stmt.item) else {
                continue;
            };
            let free_refs = value.free_refs(&candidates);
            // A name the engine gave no verdict for defaults to pure.
            let effectful = effects
                .iter()
                .find(|e| e.name == name)
                .is_some_and(|e| e.effectful);
            self.upsert(name, free_refs, effectful);
        }
    }

    /// Insert a fresh entry, or overwrite an existing name's edges and
    /// verdict in place (a re-bind), preserving first-definition order.
    fn upsert(&mut self, name: &str, free_refs: HashSet<String>, effectful: bool) {
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(entry) => {
                entry.free_refs = free_refs;
                entry.effectful = effectful;
            }
            None => self.entries.push(WsEntry {
                name: name.to_string(),
                free_refs,
                effectful,
            }),
        }
    }
}

/// A top-level `let name = rhs` with a simple `Name` pattern, yielding the
/// bound name and its RHS AST.  Destructuring patterns (`let [a, b] = …`)
/// bind multiple names and are not single worksheet nodes — they are
/// ignored, matching how `syntax::group` (private to `ral_core`) only
/// knots `Name` lets.
fn top_level_let(ast: &Ast) -> Option<(&str, &Ast)> {
    ast.as_name_let()
        .map(|(name, value)| (name, value.item.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(ws: &mut Worksheet, input: &str) {
        ws.record(input, &[]);
    }

    /// The names of every recorded entry, in order.
    fn names(ws: &Worksheet) -> Vec<&str> {
        ws.entries().iter().map(|e| e.name.as_str()).collect()
    }

    /// A binding's recorded entry.
    fn entry<'a>(ws: &'a Worksheet, name: &str) -> &'a WsEntry {
        ws.entries()
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("no entry for {name}"))
    }

    /// A binding's free refs as a sorted list, for stable assertions.
    fn refs(ws: &Worksheet, name: &str) -> Vec<String> {
        let mut v: Vec<String> = entry(ws, name).free_refs.iter().cloned().collect();
        v.sort();
        v
    }

    /// Recording `let b = $a` after `let a = 1` records the edge b→a: the
    /// later binding's free-ref set holds the earlier name.
    #[test]
    fn records_dependency_edge_to_a_prior_binding() {
        let mut ws = Worksheet::default();
        record(&mut ws, "let a = 1");
        record(&mut ws, "let b = $a");
        assert_eq!(names(&ws), vec!["a", "b"]);
        assert!(refs(&ws, "a").is_empty(), "a depends on nothing");
        assert_eq!(refs(&ws, "b"), vec!["a"], "b depends on a");
    }

    /// Edges among bindings defined in one run are recorded: the run's own
    /// names are candidates for each other's free-ref analysis.
    #[test]
    fn records_edges_within_one_run() {
        let mut ws = Worksheet::default();
        record(&mut ws, "let a = 1\nlet b = $a\nlet c = $[$a + $b]");
        assert_eq!(names(&ws), vec!["a", "b", "c"]);
        assert_eq!(refs(&ws, "b"), vec!["a"]);
        assert_eq!(refs(&ws, "c"), vec!["a", "b"]);
    }

    /// A re-bind overwrites the name's edges and verdict in place, keeping
    /// its first-definition position rather than appending a duplicate.
    #[test]
    fn rebind_updates_in_place() {
        let mut ws = Worksheet::default();
        record(&mut ws, "let a = 1");
        record(&mut ws, "let b = 2");
        record(&mut ws, "let a = $b");
        assert_eq!(names(&ws), vec!["a", "b"], "no duplicate, order preserved");
        assert_eq!(refs(&ws, "a"), vec!["b"], "a's edges were updated");
    }
}
