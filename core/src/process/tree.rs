//! A sample of the live process table.

use std::collections::{HashMap, HashSet};

/// The live descendants of `root`, `root` itself excluded: one `/bin/ps` sample
/// of every `(pid, ppid)` pair, inverted and walked transitively.
///
/// Denial records carry only `(comm, pid)`, so a PID set is the only way to tell
/// our subprocess tree from a system service that ran in the same wall second.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:ps-sample] sandbox diagnostics: shells out to `/bin/ps` to sample the live process tree for denial attribution; a diagnostic probe, not turn-time model data I/O, raises no surface card."
)]
pub(crate) fn sample_descendants(root: u32) -> HashSet<u32> {
    let mut cmd = std::process::Command::new("/bin/ps");
    cmd.args(["-axo", "pid=,ppid="]);
    let Ok(out) = super::output(&mut cmd) else {
        return HashSet::new();
    };
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, ppid) in String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(pair)
    {
        children_of.entry(ppid).or_default().push(pid);
    }
    let mut seen = HashSet::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for &c in children_of.get(&p).into_iter().flatten() {
            if seen.insert(c) {
                frontier.push(c);
            }
        }
    }
    seen
}

fn pair(line: &str) -> Option<(u32, u32)> {
    let mut fields = line.split_whitespace();
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::sample_descendants;

    #[test]
    fn sample_descendants_excludes_root() {
        let me = std::process::id();
        assert!(
            !sample_descendants(me).contains(&me),
            "root pid must be excluded"
        );
    }
}
