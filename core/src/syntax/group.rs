//! Pre-pass: partition a statement sequence into elaboration groups, telling
//! the elaborator which `let` bindings form a recursive knot.
//!
//! Only a thunk-shaped RHS — lambda or block — may join a knot.  `eval_letrec`
//! in `core/src/evaluator/comp.rs` installs every member as a thunk before any
//! body is evaluated, so members may name each other in any source order; a
//! plain value binding like `let n = f 5` would have to call `f` at binding
//! time, and stays out.
//!
//! Knots are *strongly* connected components, not merely connected ones: an
//! SCC generalises as a unit and is monomorphic within, so a binding that only
//! forward-references a helper is left a `Single` and gets the helper's scheme
//! polymorphically.  A reference resolves to the nearest preceding definition
//! of the name, or to the first if every definition follows it.  Groups are
//! emitted dependencies-first, which can lift a `let` ahead of its source
//! position.

use crate::source::Span;
use crate::syntax::ast::{Ast, Stmt};
use std::collections::{HashMap, HashSet};

/// A statement group produced by the pre-pass.
pub enum StmtGroup {
    /// Every non-recursive `let`, and every non-binding statement.
    Single(Stmt),
    /// A recursive knot, emitted as `CompKind::LetRec`.  Each member carries
    /// its own RHS span so the elaborator stamps them individually.
    LetRec(Vec<(String, Box<Ast>, Option<Span>)>),
}

/// Partition `stmts` into [`StmtGroup`]s, dependencies before their dependents
/// regardless of source order.
pub fn group_stmts(stmts: &[Stmt]) -> Vec<StmtGroup> {
    // def_list[i] = (stmt_idx, name, rhs, rhs_span); defs[name] = the def_list
    // indices defining it, in stmt_idx order.
    let mut def_list: Vec<(usize, &str, &Ast, Option<Span>)> = Vec::new();
    let mut defs: HashMap<&str, Vec<usize>> = HashMap::new();

    for (stmt_idx, stmt) in stmts.iter().enumerate() {
        if let Some((name, value)) = stmt.item.as_name_let()
            && value.item.is_thunk_form()
        {
            let di = def_list.len();
            def_list.push((stmt_idx, name, value.item.as_ref(), value.span));
            defs.entry(name).or_default().push(di);
        }
    }

    if def_list.is_empty() {
        return stmts.iter().map(|s| StmtGroup::Single(s.clone())).collect();
    }

    let candidate_names: HashSet<String> =
        defs.keys().map(std::string::ToString::to_string).collect();

    // Edge i→j: binding i's RHS freely references binding j's name.  Backward
    // edges are kept, not filtered: a backward reference into a self-recursive
    // binding still belongs in the same SCC.
    let n = def_list.len();
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, &(stmt_i, _, value, _)) in def_list.iter().enumerate() {
        for name_ref in value.free_refs(&candidate_names) {
            if let Some(def_indices) = defs.get(name_ref.as_str()) {
                let j = resolve_ref(stmt_i, def_indices, &def_list);
                if !adj[i].contains(&j) {
                    adj[i].push(j);
                }
            }
        }
    }

    let scc_id = find_sccs(n, &adj);
    let num_sccs = scc_id.iter().copied().max().map_or(0, |x| x + 1);

    // Members are sorted by source order, so `members[0]` is the SCC's head.
    let mut scc_members: Vec<Vec<usize>> = vec![Vec::new(); num_sccs];
    for (di, &cid) in scc_id.iter().enumerate() {
        scc_members[cid].push(di);
    }
    for members in &mut scc_members {
        members.sort_by_key(|&di| def_list[di].0);
    }

    // The condensation: edge A→B when some node of A references one of B.
    let mut scc_deps: Vec<HashSet<usize>> = vec![HashSet::new(); num_sccs];
    for (di, edges) in adj.iter().enumerate() {
        let from = scc_id[di];
        for &dj in edges {
            let to = scc_id[dj];
            if from != to {
                scc_deps[from].insert(to);
            }
        }
    }

    // Entering the DFS in source order keeps the topo order as close to the
    // written order as the dependencies allow.
    let mut topo: Vec<usize> = Vec::with_capacity(num_sccs);
    let mut topo_visited: Vec<bool> = vec![false; num_sccs];
    let mut scc_first_stmt: Vec<usize> = vec![0; num_sccs];
    for cid in 0..num_sccs {
        scc_first_stmt[cid] = def_list[scc_members[cid][0]].0;
    }
    let mut entry_order: Vec<usize> = (0..num_sccs).collect();
    entry_order.sort_by_key(|&cid| scc_first_stmt[cid]);
    for cid in entry_order {
        topo_dfs(cid, &scc_deps, &mut topo_visited, &mut topo);
    }

    // A non-head member never emits at its own source position; its SCC has
    // already emitted it at the head's.
    let mut consumed: HashSet<usize> = HashSet::new();
    for members in &scc_members {
        for &di in &members[1..] {
            consumed.insert(def_list[di].0);
        }
    }

    let mut head_at: HashMap<usize, usize> = HashMap::new();
    for (cid, members) in scc_members.iter().enumerate() {
        head_at.insert(def_list[members[0]].0, cid);
    }

    // At a head statement, flush the topo order up to and including its SCC,
    // so every dependency lands before it.
    let mut emitted: Vec<bool> = vec![false; num_sccs];
    let mut out: Vec<StmtGroup> = Vec::new();
    for (stmt_idx, stmt) in stmts.iter().enumerate() {
        if consumed.contains(&stmt_idx) {
            continue;
        }
        match head_at.get(&stmt_idx).copied() {
            None => out.push(StmtGroup::Single(stmt.clone())),
            Some(this_scc) if emitted[this_scc] => {
                // Already placed as a dependency of an earlier head.
            }
            Some(this_scc) => {
                for &cid in &topo {
                    if emitted[cid] {
                        continue;
                    }
                    emit_scc(cid, &scc_members, &adj, &def_list, stmts, &mut out);
                    emitted[cid] = true;
                    if cid == this_scc {
                        break;
                    }
                }
            }
        }
    }

    out
}

// ── Internal helpers ─────────────────────────────────────────────────────

/// Push `start`'s dependencies onto `topo` before `start` itself.  Iterative,
/// because a dependency chain of tens of thousands of bindings would recurse
/// as deep as the chain and overflow the host stack.
fn topo_dfs(
    start: usize,
    scc_deps: &[HashSet<usize>],
    visited: &mut [bool],
    topo: &mut Vec<usize>,
) {
    if visited[start] {
        return;
    }
    visited[start] = true;
    let mut stack: Vec<(usize, std::collections::hash_set::Iter<'_, usize>)> =
        vec![(start, scc_deps[start].iter())];
    while let Some((node, deps)) = stack.last_mut() {
        match deps.next() {
            Some(&dep) if !visited[dep] => {
                visited[dep] = true;
                stack.push((dep, scc_deps[dep].iter()));
            }
            Some(_) => {}
            None => {
                topo.push(*node);
                stack.pop();
            }
        }
    }
}

/// Emit one SCC: a `LetRec` if it has several members or a self-edge.
fn emit_scc(
    cid: usize,
    scc_members: &[Vec<usize>],
    adj: &[Vec<usize>],
    def_list: &[(usize, &str, &Ast, Option<Span>)],
    stmts: &[Stmt],
    out: &mut Vec<StmtGroup>,
) {
    let members = &scc_members[cid];
    let is_recursive = members.len() > 1 || adj[members[0]].contains(&members[0]);
    if is_recursive {
        let bindings = members
            .iter()
            .map(|&di| {
                let (_, name, value, span) = def_list[di];
                (name.to_string(), Box::new(value.clone()), span)
            })
            .collect();
        out.push(StmtGroup::LetRec(bindings));
    } else {
        // The original stmt, unchanged, so it lowers to `CompKind::Bind` and
        // generalises in the normal way.
        let stmt_idx = def_list[members[0]].0;
        out.push(StmtGroup::Single(stmts[stmt_idx].clone()));
    }
}

/// Which of a name's definitions a use at `use_stmt_idx` sees: the nearest
/// preceding one, or the first if every definition follows the use.
fn resolve_ref(
    use_stmt_idx: usize,
    def_indices: &[usize],
    def_list: &[(usize, &str, &Ast, Option<Span>)],
) -> usize {
    // `def_indices` ascends by stmt_idx, so the last match is the nearest.
    let mut best = def_indices[0];
    for &di in def_indices {
        if def_list[di].0 <= use_stmt_idx {
            best = di;
        }
    }
    best
}

/// Tarjan's algorithm: `scc_id[i]` is node `i`'s component, dense in
/// `0..num_sccs`.  The ids carry no order the caller relies on.
fn find_sccs(n: usize, adj: &[Vec<usize>]) -> Vec<usize> {
    let mut state = TarjanState {
        idx_counter: 0,
        indices: vec![None; n],
        lowlinks: vec![0; n],
        on_stack: vec![false; n],
        stack: Vec::new(),
        scc_id: vec![None; n],
        next_scc_id: 0,
    };
    for v in 0..n {
        if state.indices[v].is_none() {
            strongconnect(v, adj, &mut state);
        }
    }
    state.scc_id.into_iter().map(|x| x.unwrap()).collect()
}

struct TarjanState {
    idx_counter: usize,
    indices: Vec<Option<usize>>,
    lowlinks: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    scc_id: Vec<Option<usize>>,
    next_scc_id: usize,
}

/// Iterative Tarjan rooted at `v`, so a chain of tens of thousands of bindings
/// cannot overflow the host stack.  Popping a frame folds its lowlink into its
/// parent's — the work the recursive form does on return.
fn strongconnect(v: usize, adj: &[Vec<usize>], st: &mut TarjanState) {
    // Frame: a node, and where in `adj[node]` to resume.
    let mut work: Vec<(usize, usize)> = vec![(v, 0)];
    st.indices[v] = Some(st.idx_counter);
    st.lowlinks[v] = st.idx_counter;
    st.idx_counter += 1;
    st.stack.push(v);
    st.on_stack[v] = true;

    while let Some(&(node, next)) = work.last() {
        if next < adj[node].len() {
            work.last_mut().unwrap().1 += 1;
            let w = adj[node][next];
            if st.indices[w].is_none() {
                st.indices[w] = Some(st.idx_counter);
                st.lowlinks[w] = st.idx_counter;
                st.idx_counter += 1;
                st.stack.push(w);
                st.on_stack[w] = true;
                work.push((w, 0));
            } else if st.on_stack[w] {
                st.lowlinks[node] = st.lowlinks[node].min(st.indices[w].unwrap());
            }
            continue;
        }

        // A node whose lowlink never fell below its own index is the root of
        // its component: everything above it on the stack belongs to it.
        if st.lowlinks[node] == st.indices[node].unwrap() {
            let cid = st.next_scc_id;
            st.next_scc_id += 1;
            loop {
                let w = st.stack.pop().unwrap();
                st.on_stack[w] = false;
                st.scc_id[w] = Some(cid);
                if w == node {
                    break;
                }
            }
        }
        work.pop();
        if let Some(&(parent, _)) = work.last() {
            st.lowlinks[parent] = st.lowlinks[parent].min(st.lowlinks[node]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::ast::Pattern;
    use crate::syntax::parser::parse;
    use std::fmt::Write;

    /// One descriptor per group, in emission order: `let NAME`, `stmt`, or
    /// `rec [a, b]` with the knot's members sorted.
    fn groups_of(src: &str) -> Vec<String> {
        let stmts = parse(src).expect("parse");
        group_stmts(&stmts)
            .into_iter()
            .map(|g| match g {
                StmtGroup::Single(s) => match &s.item {
                    Ast::Let { pattern, .. } => match &pattern.item {
                        Pattern::Name(n) => format!("let {n}"),
                        _ => "let <pat>".to_string(),
                    },
                    _ => "stmt".to_string(),
                },
                StmtGroup::LetRec(bindings) => {
                    let mut names: Vec<&str> =
                        bindings.iter().map(|(n, _, _)| n.as_str()).collect();
                    names.sort_unstable();
                    format!("rec [{}]", names.join(", "))
                }
            })
            .collect()
    }

    #[test]
    fn non_recursive_let_is_a_single() {
        assert_eq!(groups_of("let x = 1\nreturn $x"), vec!["let x", "stmt"]);
    }

    #[test]
    fn self_recursive_lambda_is_a_singleton_letrec() {
        assert_eq!(
            groups_of("let f = { |x| f $x }\nreturn unit"),
            vec!["rec [f]", "stmt"]
        );
    }

    #[test]
    fn mutually_recursive_lambdas_share_one_letrec() {
        assert_eq!(
            groups_of("let f = { |x| g $x }\nlet g = { |y| f $y }\nreturn unit"),
            vec!["rec [f, g]", "stmt"]
        );
    }

    #[test]
    fn acyclic_forward_reference_splits_into_singles_in_dependency_order() {
        let groups = groups_of("let g = { f }\nlet f = { |x| f $x }\nreturn unit");
        assert_eq!(groups, vec!["rec [f]", "let g", "stmt"]);
    }

    #[test]
    fn non_thunk_rhs_never_joins_a_letrec() {
        let groups = groups_of("let f = { |x| f $x }\nlet n = f 5\nreturn $n");
        assert_eq!(groups, vec!["rec [f]", "let n", "stmt"]);
    }

    /// `g` sees the second `f`, which is acyclic, so nothing knots.
    #[test]
    fn shadowed_definitions_stay_separate() {
        let groups =
            groups_of("let f = { return 1 }\nlet f = { return 2 }\nlet g = { f }\nreturn unit");
        assert_eq!(groups, vec!["let f", "let f", "let g", "stmt"]);
    }

    /// The host-stack regression that makes both graph passes iterative.
    #[test]
    fn long_dependency_chain_does_not_overflow() {
        const N: usize = 50_000;
        let mut src = String::new();
        for i in 0..N {
            let _ = writeln!(src, "let f{i} = {{ f{} }}", i + 1);
        }
        let _ = write!(src, "let f{N} = {{ return 0 }}\nreturn 0\n");
        let stmts = parse(&src).expect("parse");
        let groups = group_stmts(&stmts);
        // Every link is acyclic: N+1 singletons plus the trailing `return 0`.
        assert_eq!(groups.len(), N + 2);
    }
}
