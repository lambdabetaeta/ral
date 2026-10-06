//! The hidden roles a ral-family process is re-exec'd to play, read off argv
//! alone.
//!
//! Every entry point (`ral`, `exarch`, a test binary's constructor) matches
//! on [`classify`], so each flag is spelled once here, and a launch is given a
//! [`Role`], never a string.  The role is named by the first argument; what
//! follows it is the role's own to read.

use std::ffi::{OsStr, OsString};

/// A hidden role, named by one flag.  The test roles exist only under
/// `test-util`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A wire engine, which takes the process over on fd 3.
    #[cfg(unix)]
    Engine,
    /// The process that holds a pipeline's pgid open.
    Anchor,
    /// A confined re-exec, whose warrant arrives on a descriptor.
    Warrant,
    /// The bundled-tool multicall.
    BundledTool,
    /// The test probe reporting the pgid its stage joined.
    #[cfg(all(unix, any(test, feature = "test-util")))]
    PgidCheck,
    /// The test fixture birthing a `detach`.
    #[cfg(all(unix, any(test, feature = "test-util")))]
    DetachBirth,
}

impl Role {
    pub(crate) const ALL: &[Self] = &[
        #[cfg(unix)]
        Self::Engine,
        Self::Anchor,
        Self::Warrant,
        Self::BundledTool,
        #[cfg(all(unix, any(test, feature = "test-util")))]
        Self::PgidCheck,
        #[cfg(all(unix, any(test, feature = "test-util")))]
        Self::DetachBirth,
    ];

    /// The argument that names this role.  `ral-daemon`, which cannot depend
    /// on core, spells `Engine`'s by hand.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            #[cfg(unix)]
            Self::Engine => "--engine",
            Self::Anchor => "--ral-pipeline-anchor",
            Self::Warrant => "--warrant",
            Self::BundledTool => "--ral-bundled-tool",
            #[cfg(all(unix, any(test, feature = "test-util")))]
            Self::PgidCheck => "--ral-test-pgid-check",
            #[cfg(all(unix, any(test, feature = "test-util")))]
            Self::DetachBirth => "--ral-test-detach-birth",
        }
    }
}

/// What a process is for, with whatever its role takes from argv.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invocation<'a> {
    #[cfg(unix)]
    Engine,
    PipelineAnchor,
    /// The pgid probe, reporting under `tag`.
    #[cfg(all(unix, any(test, feature = "test-util")))]
    PgidCheck {
        tag: Option<&'a OsStr>,
    },
    /// The detach fixture, appending `marker` to `trace`.
    #[cfg(all(unix, any(test, feature = "test-util")))]
    DetachBirth {
        trace: &'a OsStr,
        marker: &'a OsStr,
    },
    /// The arguments are those the warrant was wrongly given: it takes none.
    Warrant(&'a [OsString]),
    /// The tool's name, then its arguments.
    BundledTool(&'a [OsString]),
    /// The shell itself, which reads its own argv.
    Shell,
}

/// The invocation `argv` (without `argv[0]`) names.
#[must_use]
pub fn classify(argv: &[OsString]) -> Invocation<'_> {
    let Some((flag, rest)) = argv.split_first() else {
        return Invocation::Shell;
    };
    let Some(role) = Role::ALL.iter().find(|r| flag == OsStr::new(r.flag())) else {
        return Invocation::Shell;
    };
    match role {
        #[cfg(unix)]
        Role::Engine => Invocation::Engine,
        Role::Anchor => Invocation::PipelineAnchor,
        Role::Warrant => Invocation::Warrant(rest),
        Role::BundledTool => Invocation::BundledTool(rest),
        #[cfg(all(unix, any(test, feature = "test-util")))]
        Role::PgidCheck => Invocation::PgidCheck {
            tag: rest.first().map(OsString::as_os_str),
        },
        #[cfg(all(unix, any(test, feature = "test-util")))]
        Role::DetachBirth => match rest {
            [trace, marker, ..] => Invocation::DetachBirth { trace, marker },
            _ => Invocation::Shell,
        },
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
        let anchor = Role::Anchor.flag();
        let bundled = Role::BundledTool.flag();
        assert_eq!(classify(&[]), Invocation::Shell);
        assert_eq!(
            classify(&argv(&["script.ral", anchor])),
            Invocation::Shell,
            "a flag a script was given names no role"
        );
        assert_eq!(
            classify(&argv(&[anchor, "ignored"])),
            Invocation::PipelineAnchor
        );
        let tool = argv(&[bundled, "ls", "-l"]);
        assert_eq!(classify(&tool), Invocation::BundledTool(&tool[1..]));
        let bare = argv(&[bundled]);
        assert_eq!(classify(&bare), Invocation::BundledTool(&[]));
    }

    #[test]
    fn flags_are_distinct_and_each_names_a_role() {
        for (i, role) in Role::ALL.iter().enumerate() {
            assert!(
                Role::ALL[..i].iter().all(|r| r.flag() != role.flag()),
                "{role:?} shares its flag"
            );
            let payload = argv(&[role.flag(), "a", "b"]);
            assert_ne!(classify(&payload), Invocation::Shell, "{role:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_unix_roles_are_named_by_their_first_argument() {
        let engine = Role::Engine.flag();
        assert_eq!(classify(&argv(&[engine])), Invocation::Engine);
        assert_eq!(
            classify(&argv(&["script.ral", engine])),
            Invocation::Shell,
            "an engine is named by the first argument, never one a script was given"
        );
        assert_eq!(
            classify(&argv(&[Role::BundledTool.flag(), "rg", engine, "pcre2"])),
            Invocation::BundledTool(&argv(&["rg", engine, "pcre2"])),
            "a tool's own `--engine` is the tool's"
        );
        let (pgid, birth) = (Role::PgidCheck.flag(), Role::DetachBirth.flag());
        let tag = OsString::from("up");
        assert_eq!(
            classify(&argv(&[pgid, "up"])),
            Invocation::PgidCheck { tag: Some(&tag) }
        );
        assert_eq!(
            classify(&argv(&[pgid])),
            Invocation::PgidCheck { tag: None }
        );
        assert_eq!(
            classify(&argv(&[birth, "trace", "marker", "spare"])),
            Invocation::DetachBirth {
                trace: OsStr::new("trace"),
                marker: OsStr::new("marker"),
            }
        );
        assert_eq!(
            classify(&argv(&[birth, "trace"])),
            Invocation::Shell,
            "a birth missing its marker is no birth"
        );
    }

    /// The warrant's arguments are carried, never read: a flag after it names
    /// no role, and what it carries is what the trampoline refuses.
    #[test]
    fn a_warrant_reads_nothing_after_its_flag() {
        let warrant = Role::Warrant.flag();
        for rest in [
            &[][..],
            &["sh"],
            &["--engine"],
            &[Role::Anchor.flag()],
            &[Role::BundledTool.flag(), "ls"],
        ] {
            let argv = argv(&[&[warrant], rest].concat());
            assert_eq!(classify(&argv), Invocation::Warrant(&argv[1..]));
        }
    }
}
