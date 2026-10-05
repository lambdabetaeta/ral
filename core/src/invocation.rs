//! Which hidden role a ral-family process was started to play, read off its
//! argv alone.
//!
//! Every entry point — `ral`, `exarch`, a test binary's constructor — matches
//! on [`classify`], so the flags' precedence and disjointness live here and
//! nowhere else: the role is named by the first argument, and what follows it
//! is the role's own to read.

use crate::runtime::pipeline::helper::{ANCHOR_FLAG, BUNDLED_TOOL_FLAG};
use crate::sandbox::WARRANT_FLAG;
#[cfg(unix)]
use crate::test_helper::{DETACH_BIRTH_FLAG, PGID_CHECK_FLAG};
#[cfg(unix)]
use std::ffi::OsStr;
use std::ffi::OsString;

#[cfg(unix)]
const ENGINE_FLAG: &str = "--engine";

/// What a process is for, with whatever its role takes from argv.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invocation<'a> {
    /// A wire engine, which takes the process over on fd 3.
    #[cfg(unix)]
    Engine,
    /// The process that holds a pipeline's pgid open.
    PipelineAnchor,
    /// The test probe reporting the pgid its stage joined, under `tag`.
    #[cfg(unix)]
    PgidCheck { tag: Option<&'a OsStr> },
    /// The test fixture birthing a `detach` that appends `marker` to `trace`.
    #[cfg(unix)]
    DetachBirth { trace: &'a OsStr, marker: &'a OsStr },
    /// A confined re-exec, whose warrant arrives on a descriptor.  The
    /// arguments are those it was wrongly given: it takes none.
    Warrant(&'a [OsString]),
    /// The bundled-tool multicall: the tool's name, then its arguments.
    BundledTool(&'a [OsString]),
    /// The shell itself, which reads its own argv.
    Shell,
}

/// The role `argv` — without `argv[0]` — names.
#[must_use]
pub fn classify(argv: &[OsString]) -> Invocation<'_> {
    let Some((flag, rest)) = argv.split_first() else {
        return Invocation::Shell;
    };
    match (flag.to_str(), rest) {
        #[cfg(unix)]
        (Some(ENGINE_FLAG), _) => Invocation::Engine,
        (Some(ANCHOR_FLAG), _) => Invocation::PipelineAnchor,
        #[cfg(unix)]
        (Some(PGID_CHECK_FLAG), tag) => Invocation::PgidCheck {
            tag: tag.first().map(OsString::as_os_str),
        },
        #[cfg(unix)]
        (Some(DETACH_BIRTH_FLAG), [trace, marker, ..]) => Invocation::DetachBirth {
            trace: trace.as_os_str(),
            marker: marker.as_os_str(),
        },
        (Some(WARRANT_FLAG), extra) => Invocation::Warrant(extra),
        (Some(BUNDLED_TOOL_FLAG), args) => Invocation::BundledTool(args),
        _ => Invocation::Shell,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn each_role_is_named_by_its_first_argument() {
        assert_eq!(classify(&[]), Invocation::Shell);
        assert_eq!(
            classify(&argv(&["script.ral", ANCHOR_FLAG])),
            Invocation::Shell,
            "a flag a script was given names no role"
        );
        assert_eq!(
            classify(&argv(&[ANCHOR_FLAG, "ignored"])),
            Invocation::PipelineAnchor
        );
        let tool = argv(&[BUNDLED_TOOL_FLAG, "ls", "-l"]);
        assert_eq!(classify(&tool), Invocation::BundledTool(&tool[1..]));
        let bare = argv(&[BUNDLED_TOOL_FLAG]);
        assert_eq!(classify(&bare), Invocation::BundledTool(&[]));
    }

    #[cfg(unix)]
    #[test]
    fn the_unix_roles_are_named_by_their_first_argument() {
        assert_eq!(classify(&argv(&["--engine"])), Invocation::Engine);
        assert_eq!(
            classify(&argv(&["script.ral", "--engine"])),
            Invocation::Shell,
            "an engine is named by the first argument, never one a script was given"
        );
        assert_eq!(
            classify(&argv(&[BUNDLED_TOOL_FLAG, "rg", "--engine", "pcre2"])),
            Invocation::BundledTool(&argv(&["rg", "--engine", "pcre2"])),
            "a tool's own `--engine` is the tool's"
        );
        let tag = OsString::from("up");
        assert_eq!(
            classify(&argv(&[PGID_CHECK_FLAG, "up"])),
            Invocation::PgidCheck { tag: Some(&tag) }
        );
        assert_eq!(
            classify(&argv(&[PGID_CHECK_FLAG])),
            Invocation::PgidCheck { tag: None }
        );
        let birth = argv(&[DETACH_BIRTH_FLAG, "trace", "marker", "spare"]);
        assert_eq!(
            classify(&birth),
            Invocation::DetachBirth {
                trace: OsStr::new("trace"),
                marker: OsStr::new("marker"),
            }
        );
        assert_eq!(
            classify(&argv(&[DETACH_BIRTH_FLAG, "trace"])),
            Invocation::Shell,
            "a birth missing its marker is no birth"
        );
    }

    /// The warrant's arguments are carried, never read: a flag after it names
    /// no role, and what it carries is what the trampoline refuses.
    #[test]
    fn a_warrant_reads_nothing_after_its_flag() {
        for rest in [
            &[][..],
            &["sh"],
            &["--engine"],
            &[ANCHOR_FLAG],
            &[BUNDLED_TOOL_FLAG, "ls"],
        ] {
            let warrant = argv(&[&[WARRANT_FLAG], rest].concat());
            assert_eq!(classify(&warrant), Invocation::Warrant(&warrant[1..]));
        }
    }
}
