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
//! # SCCs, not WCCs
//!
//! `LetRec` is the recursion knot, and within a knot every member is
//! monomorphic relative to the others until the whole group is generalised.
//! Grouping by *strongly connected components* of the call graph is what HM
//! lets-rec wants: each SCC of mutually recursive (or self-recursive)
//! lambdas becomes one `LetRec`, while bindings that merely *forward-
//! reference* a later helper get to use the helper polymorphically because
//! the helper is generalised first.
//!
//! Concretely, `g = { … f … }; f = { |x| f x }` forms two SCCs:
//! `{f}` (self-recursive — emitted as a one-binding `LetRec`) and `{g}`
//! (acyclic — emitted as a normal `let` so `g` gets ordinary
//! let-polymorphism over `f`'s scheme).
//!
//! # Forward and backward references
//!
//! A `LetRec` member may reference any other member regardless of source
//! order.  This is safe because the evaluator (see `eval_letrec`) installs a
//! placeholder thunk for every binding name before evaluating any body, so a
//! reference resolves to the real lambda at call time.
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
//! 3. Compute strongly connected components (Tarjan).
//! 4. Topologically sort the SCC condensation: dependencies first, dependents
//!    after.
//! 5. Walk statements in source order.  For each Let stmt, if it's the source-
//!    earliest member of an SCC and that SCC is not yet emitted, emit every
//!    SCC up to and including this one (in topo order — dependencies first).
//!    A non-trivial SCC (size >1, or size 1 with a self-edge) emits as
//!    `LetRec`; an acyclic singleton SCC emits as `Single`.
//!
//! Mutual recursion and forward references work across arbitrary intervening
//! statements:
//!
//! ```text
//! f = { |x| g x }
//! /bin/blah           # any non-let statement
//! g = { |x| f x }    # same LetRec group as f
//! ```

use crate::source::Span;
use crate::syntax::ast::{Ast, Pattern, Stmt};
use std::collections::{HashMap, HashSet};

/// A statement group produced by the pre-pass.
pub enum StmtGroup {
    /// A single statement.  This covers every non-recursive `let` and
    /// every non-binding statement (commands, pipelines, …).  The
    /// underlying [`Stmt`] carries the source span the elaborator will
    /// stamp onto emitted IR.
    Single(Stmt),
    /// A set of mutually recursive or forward-referencing lambda bindings to
    /// be emitted as `Comp::LetRec`.  All members are lambda or block
    /// expressions.  Each member carries its own RHS span, so the
    /// elaborator stamps every recursive binding's IR with its own source
    /// position rather than falling back to the group's.
    LetRec(Vec<(String, Box<Ast>, Option<Span>)>),
}

/// Partition `stmts` into [`StmtGroup`]s.  Strongly-connected groups of
/// lambda bindings become `LetRec`; acyclic singletons stay as `Single` and
/// receive ordinary let-polymorphism.  Dependencies are emitted before their
/// dependents regardless of source order.
pub fn group_stmts(stmts: &[Stmt]) -> Vec<StmtGroup> {
    // Collect all named lambda let-bindings with their statement indices.
    // def_list[i] = (stmt_idx, name, value_ast, rhs_span)
    let mut def_list: Vec<(usize, &str, &Ast, Option<Span>)> = Vec::new();
    // defs[name] = list of def_list indices in stmt_idx order
    let mut defs: HashMap<&str, Vec<usize>> = HashMap::new();

    for (stmt_idx, stmt) in stmts.iter().enumerate() {
        if let Ast::Let { pattern, value } = &stmt.item
            && let Pattern::Name(name) = &pattern.item
            && value.item.is_thunk_form()
        {
            let di = def_list.len();
            def_list.push((stmt_idx, name.as_str(), value.item.as_ref(), value.span));
            defs.entry(name.as_str()).or_default().push(di);
        }
    }

    if def_list.is_empty() {
        return stmts.iter().map(|s| StmtGroup::Single(s.clone())).collect();
    }

    let candidate_names: HashSet<String> = defs.keys().map(std::string::ToString::to_string).collect();

    // Build a directed dependency graph over def_list indices.
    // Edge i→j: binding i's RHS has a free reference to binding j's name.
    // We do not filter to forward edges only — the SCC algorithm treats
    // backward references as ordinary edges, and a backward reference into
    // a self-recursive binding still belongs in the same SCC.
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

    // Compute SCCs.
    let scc_id = find_sccs(n, &adj);
    let num_sccs = scc_id.iter().copied().max().map_or(0, |x| x + 1);

    // Group def_list indices by SCC id, sorted within each by source order.
    let mut scc_members: Vec<Vec<usize>> = vec![Vec::new(); num_sccs];
    for (di, &cid) in scc_id.iter().enumerate() {
        scc_members[cid].push(di);
    }
    for members in &mut scc_members {
        members.sort_by_key(|&di| def_list[di].0);
    }

    // Build SCC condensation graph: edge from SCC A to SCC B when some node
    // in A references a node in B and A != B.
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

    // Topo-sort the condensation: dependencies come before dependents.
    // DFS post-order over SCCs in source order produces a topo order.
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

    // Mark non-head members as consumed (they don't emit at their own source
    // position; their SCC emits at the head's position).
    let mut consumed: HashSet<usize> = HashSet::new();
    for members in &scc_members {
        for &di in &members[1..] {
            consumed.insert(def_list[di].0);
        }
    }

    // Map source stmt_idx → SCC id when that stmt is the head of its SCC.
    let mut head_at: HashMap<usize, usize> = HashMap::new();
    for (cid, members) in scc_members.iter().enumerate() {
        head_at.insert(def_list[members[0]].0, cid);
    }

    // Walk source.  When we reach a head stmt, emit its SCC plus every
    // un-emitted dependency in topo order so deps land before dependents.
    let mut emitted: Vec<bool> = vec![false; num_sccs];
    let mut out: Vec<StmtGroup> = Vec::new();
    for (stmt_idx, stmt) in stmts.iter().enumerate() {
        if consumed.contains(&stmt_idx) {
            continue;
        }
        match head_at.get(&stmt_idx).copied() {
            None => out.push(StmtGroup::Single(stmt.clone())),
            Some(this_scc) if emitted[this_scc] => {
                // Emitted out-of-order as a dep of an earlier head.  Drop the
                // source-position visit; the SCC has already been placed.
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

/// DFS post-order: visit `scc`'s dependencies first, then push `scc`.  This
/// produces a topological order where dependencies precede dependents.
///
/// Iterative (explicit work stack) rather than recursive: a long
/// dependency chain — `let f_i = { f_{i+1} }` repeated tens of thousands
/// of times — would otherwise recurse as deep as the chain and overflow
/// the host stack.  Each frame carries the node and an iterator over its
/// remaining dependencies; a node is pushed to `topo` only once all its
/// dependencies have been emitted (true post-order).
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

/// Emit one SCC as either a `LetRec` (multi-member, or self-recursive
/// singleton) or a `Single` (acyclic singleton — gets ordinary
/// let-polymorphism through `Comp::Bind`).
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
        // Acyclic singleton — emit the original Let stmt unchanged so it
        // flows through `Comp::Bind` and generalises in the normal way.
        let stmt_idx = def_list[members[0]].0;
        out.push(StmtGroup::Single(stmts[stmt_idx].clone()));
    }
}

/// Given a reference to a name from a definition at `use_stmt_idx`, and a
/// list of `def_list` indices for all definitions of that name (in `stmt_idx`
/// order), return the `def_list` index that is "visible" from `use_stmt_idx`.
///
/// The visible definition is the last one whose statement index is ≤
/// `use_stmt_idx` (nearest preceding).  If all definitions come after
/// `use_stmt_idx`, the first definition is returned (forward reference).
fn resolve_ref(
    use_stmt_idx: usize,
    def_indices: &[usize],
    def_list: &[(usize, &str, &Ast, Option<Span>)],
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

/// Tarjan's strongly-connected-components algorithm.
///
/// Returns `scc_id[i]` = the SCC index of node `i`.  Nodes in the same SCC
/// share an id; SCC ids are dense in `0..num_sccs`.  Order of ids is
/// reverse topological — deeper SCCs (leaves) get lower ids — but the
/// caller does its own topo sort and does not rely on this.
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

/// Iterative Tarjan rooted at `v`.  A work stack of `(node, next-neighbour
/// index)` replaces the recursion so a dependency chain of tens of
/// thousands of bindings (`let f_i = { f_{i+1} }`) cannot overflow the
/// host stack.  Descending into an unvisited neighbour pushes a frame;
/// when that frame finishes it propagates its lowlink to its parent — the
/// one place the recursive form did its work on return.
fn strongconnect(v: usize, adj: &[Vec<usize>], st: &mut TarjanState) {
    // Each work frame: the node and the index of the next neighbour to
    // examine in `adj[node]`.
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
                // Descend into `w`; its lowlink is folded back into
                // `node` when the `w` frame pops (the `None` arm below).
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

        // All neighbours examined.  If `node` is an SCC root, pop its
        // component; then propagate its lowlink up to its parent frame.
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
    use crate::syntax::parser::parse;
    use std::fmt::Write;

    /// Summarise the grouping of `src` as a list of group descriptors: a
    /// `Single` carries its statement's shape via a one-char tag, a
    /// `LetRec` lists its member names sorted.  This gives the SCC/topo
    /// logic a compact behavioural oracle.
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

    /// A forward reference that is *not* part of a cycle gets its own
    /// `Single` group and is emitted after the helper it forward-refers
    /// to — dependencies before dependents.
    #[test]
    fn acyclic_forward_reference_splits_into_singles_in_dependency_order() {
        // `g` forward-references self-recursive `f`; `g`'s own SCC is
        // acyclic so it stays a Single, emitted after `f`'s LetRec.
        let groups = groups_of("let g = { f }\nlet f = { |x| f $x }\nreturn unit");
        assert_eq!(groups, vec!["rec [f]", "let g", "stmt"]);
    }

    /// A non-thunk RHS that references a candidate is never knotted — only
    /// thunk-shaped RHS participate in a `LetRec`.
    #[test]
    fn non_thunk_rhs_never_joins_a_letrec() {
        // `f` is a self-recursive lambda; `n = f 5` calls it but is a
        // plain value binding, so it stays a Single after `f`.
        let groups = groups_of("let f = { |x| f $x }\nlet n = f 5\nreturn $n");
        assert_eq!(groups, vec!["rec [f]", "let n", "stmt"]);
    }

    /// Shadowing: a reference resolves to the nearest preceding definition.
    /// `g` depends on the second `f` (definition B), which is acyclic, so
    /// both `f`s and `g` are singles.
    #[test]
    fn shadowed_definitions_stay_separate() {
        let groups =
            groups_of("let f = { return 1 }\nlet f = { return 2 }\nlet g = { f }\nreturn unit");
        assert_eq!(groups, vec!["let f", "let f", "let g", "stmt"]);
    }

    /// The host-stack regression: a long chain of thunk bindings each
    /// referencing the next must group without overflowing — the SCC and
    /// topo passes are iterative.  Each link is acyclic, so every binding
    /// is its own `Single`.
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
        // N+1 thunk bindings (all acyclic singletons) + the trailing
        // `return 0` statement.
        assert_eq!(groups.len(), N + 2);
    }
}
