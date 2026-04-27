//! Pre-pass: partition statement sequences into elaboration groups.
//!
//! The elaborator needs to know which `let` bindings are mutually recursive so
//! it can emit `Comp::LetRec` for them.  This module does that analysis over
//! the full statement sequence before elaboration begins.
//!
//! # Why only lambdas?
//!
//! A `LetRec` establishes all bindings simultaneously before any of them is
//! evaluated.  A lambda is a thunk: its body is not evaluated at binding time,
//! so it can safely refer to any group member before the group has settled.
//! A plain value binding like `n = f 5` would need to call `f` at definition
//! time, creating a genuine cycle.  So only lambda/block RHS expressions may
//! participate in a `LetRec` group.
//!
//! # How LetRec handles both forward and backward references
//!
//! When the evaluator encounters `Comp::LetRec`, it:
//!
//! 1. Installs a *placeholder* thunk for each binding name.  The placeholder,
//!    when forced, re-evaluates the whole group and returns the real lambda for
//!    that slot.
//! 2. Evaluates each lambda body *with the placeholders in scope*, capturing
//!    the environment (including placeholders) in each thunk's closure.
//! 3. Replaces the placeholders with the actual lambdas.
//!
//! This means any lambda in the group can freely reference any other group
//! member — whether that member is defined earlier or later in the source —
//! because at call time the placeholder resolves to the real lambda.
//!
//! # Grouping rule
//!
//! Two lambda lets belong in the same `LetRec` when there is any directed path
//! between them in the dependency graph (i.e. they are in the same *weakly
//! connected component*).  This covers three cases:
//!
//! - Mutual recursion (`f` calls `g`, `g` calls `f`)
//! - Forward reference (`f` calls `g`, `g` defined later, no back-edge)
//! - Self-recursion (`f` calls itself)
//!
//! Lambda lets with no dependency edges at all are emitted as individual
//! `Single` groups.
//!
//! # Shadow handling
//!
//! When a name is defined more than once, each later definition shadows the
//! earlier one.  A reference to that name resolves to the nearest preceding
//! definition; if all definitions come after the reference site, the first one
//! is used.
//!
//! ```text
//! f = { |x| 1 }   # definition A
//! f = { |x| 2 }   # definition B (shadows A)
//! g = { f }       # g depends on B — the nearest preceding f
//! ```
//!
//! # Algorithm
//!
//! 1. Collect all named lambda `let` bindings across the full statement list,
//!    recording each one's statement index.
//! 2. Build a directed dependency graph: edge i→j when binding i's RHS
//!    contains a free reference to binding j's name, shadow-resolved to the
//!    nearest preceding definition of that name.
//! 3. Find weakly connected components (treating edges as undirected).
//! 4. If the graph has no edges at all, skip WCC computation and emit every
//!    statement as a `Single` — this is the common case.
//! 5. Walk statements in source order.  At the first-encountered member of each
//!    multi-node WCC (or a self-recursive singleton), collect all members and
//!    emit a `LetRec`.  All other statements are emitted as `Single`.
//!
//! Mutual recursion and forward references work across arbitrary intervening
//! statements:
//!
//! ```text
//! f = { |x| g x }
//! /bin/blah           # any non-let statement
//! g = { |x| f x }    # same LetRec group as f
//! ```

use crate::ast::{Ast, Pattern};
use std::collections::{HashMap, HashSet};

/// A statement group produced by the pre-pass.
pub enum StmtGroup {
    /// A single statement.  This covers every non-recursive `let`, every
    /// non-binding statement (commands, pipelines, …), and `Pos` markers.
    Single(Ast),
    /// A set of mutually recursive or forward-referencing lambda bindings to
    /// be emitted as `Comp::LetRec`.  All members are lambda or block
    /// expressions.
    LetRec(Vec<(String, Box<Ast>)>),
}

/// Partition `stmts` into [`StmtGroup`]s, promoting lambda bindings that
/// reference each other (directly or transitively) to [`StmtGroup::LetRec`].
pub fn group_stmts(stmts: &[Ast]) -> Vec<StmtGroup> {
    // Collect all named lambda let-bindings with their statement indices.
    // def_list[i] = (stmt_idx, name, value_ast)
    let mut def_list: Vec<(usize, &str, &Ast)> = Vec::new();
    // defs[name] = list of def_list indices in stmt_idx order
    let mut defs: HashMap<&str, Vec<usize>> = HashMap::new();

    for (stmt_idx, stmt) in stmts.iter().enumerate() {
        if let Ast::Let {
            pattern: Pattern::Name(name),
            value,
        } = stmt
            && matches!(value.as_ref(), Ast::Lambda { .. })
        {
            let di = def_list.len();
            def_list.push((stmt_idx, name.as_str(), value.as_ref()));
            defs.entry(name.as_str()).or_default().push(di);
        }
    }

    if def_list.is_empty() {
        // No lambda bindings — nothing to group.
        return stmts.iter().map(|s| StmtGroup::Single(s.clone())).collect();
    }

    let candidate_names: HashSet<String> = defs.keys().map(|s| s.to_string()).collect();

    // Build a directed dependency graph over def_list indices.
    // Edge i→j: binding i's RHS has a free reference to binding j's name.
    let n = def_list.len();
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, &(stmt_i, _, value)) in def_list.iter().enumerate() {
        for name_ref in value.free_refs(&candidate_names) {
            if let Some(def_indices) = defs.get(name_ref.as_str()) {
                let j = resolve_ref(stmt_i, def_indices, &def_list);
                // Only forward edges and self-edges need LetRec.
                // Backward edges are safe: the referenced binding is
                // already bound when the referencing closure is created.
                if def_list[j].0 >= stmt_i && !adj[i].contains(&j) {
                    adj[i].push(j);
                }
            }
        }
    }

    let has_any_dep = adj.iter().any(|edges| !edges.is_empty());
    if !has_any_dep {
        // No dependency edges — nothing is recursive or forward-referencing.
        return stmts.iter().map(|s| StmtGroup::Single(s.clone())).collect();
    }

    // Find weakly connected components: two nodes are in the same WCC if
    // there is any directed path between them (in either direction).
    let components = find_wccs(n, &adj);

    // Group def_list indices by WCC id.
    let mut wcc_members: HashMap<usize, Vec<usize>> = HashMap::new();
    for (di, &cid) in components.iter().enumerate() {
        wcc_members.entry(cid).or_default().push(di);
    }

    // A WCC is a LetRec if it has >1 members, or 1 member with a self-edge.
    // Build, per group, the def_list indices in statement order.
    let mut groups: Vec<Vec<usize>> = wcc_members
        .into_values()
        .filter(|m| m.len() > 1 || adj[m[0]].contains(&m[0]))
        .collect();
    if groups.is_empty() {
        return stmts.iter().map(|s| StmtGroup::Single(s.clone())).collect();
    }
    for m in &mut groups {
        m.sort_by_key(|&di| def_list[di].0);
    }

    // stmt_idx → group id, for the first member of each LetRec only.
    // Other members are marked consumed by the same key.
    let mut head_at: HashMap<usize, usize> = HashMap::new();
    let mut consumed: HashSet<usize> = HashSet::new();
    for (gid, members) in groups.iter().enumerate() {
        head_at.insert(def_list[members[0]].0, gid);
        for &di in &members[1..] {
            consumed.insert(def_list[di].0);
        }
    }

    let mut out = Vec::new();
    for (stmt_idx, stmt) in stmts.iter().enumerate() {
        if consumed.contains(&stmt_idx) {
            continue;
        }
        match head_at.get(&stmt_idx) {
            Some(&gid) => {
                let bindings = groups[gid]
                    .iter()
                    .map(|&di| {
                        let (_, name, value) = def_list[di];
                        (name.to_string(), Box::new(value.clone()))
                    })
                    .collect();
                out.push(StmtGroup::LetRec(bindings));
            }
            None => out.push(StmtGroup::Single(stmt.clone())),
        }
    }

    out
}

// ── Internal helpers ─────────────────────────────────────────────────────

/// Given a reference to a name from a definition at `use_stmt_idx`, and a
/// list of def_list indices for all definitions of that name (in stmt_idx
/// order), return the def_list index that is "visible" from `use_stmt_idx`.
///
/// The visible definition is the last one whose statement index is ≤
/// `use_stmt_idx` (nearest preceding).  If all definitions come after
/// `use_stmt_idx`, the first definition is returned (forward reference).
fn resolve_ref(
    use_stmt_idx: usize,
    def_indices: &[usize],
    def_list: &[(usize, &str, &Ast)],
) -> usize {
    // def_indices is in stmt_idx order (built by iterating stmts in order).
    let mut best = def_indices[0];
    for &di in def_indices {
        if def_list[di].0 <= use_stmt_idx {
            best = di;
        }
    }
    best
}

/// Weakly connected components via union-find.
///
/// Returns `parent[i]` = representative (root) of node i's component.
/// Two nodes are in the same WCC iff they have the same root.
fn find_wccs(n: usize, adj: &[Vec<usize>]) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]]; // path compression (halving)
            x = parent[x];
        }
        x
    }

    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[ra] = rb;
        }
    }

    for (v, adj_v) in adj.iter().enumerate() {
        for &w in adj_v {
            union(&mut parent, v, w);
        }
    }

    // Normalise so that parent[i] is the true root.
    for i in 0..n {
        parent[i] = find(&mut parent, i);
    }

    parent
}
