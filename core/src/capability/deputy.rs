//! The confused-deputy verdict: a pure predicate over a folded
//! [`Capabilities`] frame.
//!
//! `design/grant.md`'s third concession names the shape in prose: a
//! prefix that is both `exec`-admitted and `fs`-writable is an escape
//! hatch — drop a binary there, the next call admits it.
//! [`deputy_prefixes`] turns that into a theorem with a stated premise
//! list instead of a caveat.
//!
//! **It reports; it does not deny.** Write-`cwd:` plus exec under
//! `cwd:` is the compile-and-run workflow every agent profile needs
//! (`cargo build && ./target/debug/app` *is* this shape), so a finding
//! marks a property worth surfacing, not a policy to reject. Callers
//! mint an audit node or a lint line from the result; neither attenuates
//! nor fails a load.
//!
//! **Judged on the folded frame, not a layer.** Two innocent layers
//! compose into a guilty one: a base grants exec under `/usr/bin`, an
//! overlay grants write under `/usr/bin`, and neither layer alone is a
//! deputy — only their meet is. So this takes a single already-folded
//! `Capabilities` — the shape a `GrantStack` reduces to under
//! [`Capabilities::meet`] — rather than the stack itself, so a caller
//! cannot pass an unfolded layer by mistake and read a per-layer answer
//! as the composed one.
//!
//! **The residue is stated, not hidden.** Two premises:
//!
//! - *Static but invisible.* The predicate can only fire where **both**
//!   dimensions are restricted. A frame with `fs: None` leaves every
//!   exec-admitted prefix writable and undetectable — treating `None`
//!   as "everything writable" would flag every exec-only grant, the
//!   ambient root first. So `None` on either `exec` or `fs` yields no
//!   finding, deliberately.
//! - *Dynamic.* Overlap is judged on the prefix strings the policy
//!   carries, lexically, at the moment the frame is folded. A symlink
//!   created afterward inside an admitted write region, pointing into
//!   an exec-admitted tree, stays open under the same stability
//!   hypothesis as TOCTOU (`design/grant.md`'s second concession).

use crate::path::{NormalizedPrefix, path_within_str};
use crate::types::{Capabilities, ExecDir};

/// The exec-admitted directory prefixes that are also writable under the
/// folded `caps` — the confused-deputy escape hatch.
///
/// Empty whenever `caps` carries no finding, including whenever `exec`
/// or `fs` is `None` (see the module doc's static residue).
///
/// Two directory prefixes never partially overlap: one contains the
/// other, or they are disjoint. So containment in either direction is
/// the escape-hatch condition, and the contained (narrower) prefix — the
/// region that is simultaneously writable and exec-admitted — is what's
/// reported.
pub fn deputy_prefixes(caps: &Capabilities) -> Vec<NormalizedPrefix> {
    let (Some(exec), Some(fs)) = (&caps.exec, &caps.fs) else {
        return Vec::new();
    };
    let mut found: Vec<NormalizedPrefix> = exec
        .dirs
        .iter()
        .filter(|(_, verdict)| **verdict == ExecDir::Allow)
        .filter_map(|(dir, _)| {
            fs.write_prefixes
                .iter()
                .find(|w| path_within_str(dir, w.as_str()) || path_within_str(w.as_str(), dir))
                .map(|w| {
                    if path_within_str(dir, w.as_str()) {
                        NormalizedPrefix::from_surface(dir)
                    } else {
                        w.clone()
                    }
                })
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::deputy_prefixes;
    use crate::path::NormalizedPrefix;
    use crate::types::{Capabilities, ExecDir, ExecMap, FsPolicy};
    use std::collections::BTreeMap;

    fn exec_dir(dir: &str) -> ExecMap {
        ExecMap {
            dirs: BTreeMap::from([(dir.to_string(), ExecDir::Allow)]),
            ..ExecMap::default()
        }
    }

    fn fs_write(prefix: &str) -> FsPolicy {
        FsPolicy {
            write_prefixes: vec![prefix.into()],
            ..FsPolicy::default()
        }
    }

    #[test]
    fn empty_capabilities_has_no_finding() {
        assert!(deputy_prefixes(&Capabilities::default()).is_empty());
    }

    #[test]
    fn fs_none_is_invisible_even_with_an_exec_dir() {
        let caps = Capabilities {
            exec: Some(exec_dir("/usr/bin")),
            fs: None,
            ..Capabilities::default()
        };
        assert!(
            deputy_prefixes(&caps).is_empty(),
            "an unrestricted fs dimension must not be read as \"everything writable\""
        );
    }

    #[test]
    fn two_innocent_layers_fold_into_a_finding() {
        let layer_a = Capabilities {
            exec: Some(exec_dir("/usr/bin")),
            ..Capabilities::default()
        };
        let layer_b = Capabilities {
            fs: Some(fs_write("/usr/bin")),
            ..Capabilities::default()
        };
        assert!(
            deputy_prefixes(&layer_a).is_empty() && deputy_prefixes(&layer_b).is_empty(),
            "neither layer alone names both dimensions"
        );
        let folded = layer_a.meet(layer_b);
        assert_eq!(
            deputy_prefixes(&folded),
            vec![NormalizedPrefix::from_surface("/usr/bin")]
        );
    }

    #[test]
    fn compile_and_run_shape_fires_benignly() {
        // `write: cwd:` plus exec under `cwd:` — the workflow every
        // agent profile needs, and exactly the shape the concession
        // warns about. The verdict fires; nothing here denies it.
        let caps = Capabilities {
            exec: Some(exec_dir("/work/target/debug")),
            fs: Some(fs_write("/work")),
            ..Capabilities::default()
        };
        assert_eq!(
            deputy_prefixes(&caps),
            vec![NormalizedPrefix::from_surface("/work/target/debug")]
        );
    }
}
