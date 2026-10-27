//! Process-role vocabulary and argv classification, independent of dispatch.
//!
//! Launches name a [`Role`]; [`classify`] reads only the first argument.
//! [`crate::invocation`] serves the result.

use std::ffi::{OsStr, OsString};

/// A hidden role, named by one flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Reports the POSIX login environment on fd 3.
    #[cfg(unix)]
    EnvReport,
    /// A wire engine, which takes the process over on fd 3.
    #[cfg(unix)]
    Engine,
    /// A privileged engine that jails each command.
    #[cfg(seccomp)]
    GuestEngine,
    /// The process that holds a pipeline's pgid open.
    PipelineAnchor,
    /// A confined re-exec, whose warrant arrives on a descriptor.
    Warrant,
    /// The bundled-tool multicall.
    BundledTool,
}

impl Role {
    pub(crate) const ALL: &[Self] = &[
        #[cfg(unix)]
        Self::EnvReport,
        #[cfg(unix)]
        Self::Engine,
        #[cfg(seccomp)]
        Self::GuestEngine,
        Self::PipelineAnchor,
        Self::Warrant,
        Self::BundledTool,
    ];

    /// The argument that names this role.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            #[cfg(unix)]
            Self::EnvReport => "--ral-env-report",
            #[cfg(unix)]
            Self::Engine => "--engine",
            #[cfg(seccomp)]
            Self::GuestEngine => guest_abi::layout::ENGINE_FLAG,
            Self::PipelineAnchor => "--ral-pipeline-anchor",
            Self::Warrant => "--warrant",
            Self::BundledTool => "--ral-bundled-tool",
        }
    }
}

/// What a process is for, with whatever its role takes from argv.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invocation<'a> {
    #[cfg(unix)]
    EnvReport,
    #[cfg(unix)]
    Engine,
    PipelineAnchor,
    /// The jail's cgroup root, validated by the guest role.
    #[cfg(seccomp)]
    GuestEngine(&'a [OsString]),
    /// The warrant descriptor's argument, validated by the trampoline.
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
        Role::EnvReport => Invocation::EnvReport,
        #[cfg(unix)]
        Role::Engine => Invocation::Engine,
        #[cfg(seccomp)]
        Role::GuestEngine => Invocation::GuestEngine(rest),
        Role::PipelineAnchor => Invocation::PipelineAnchor,
        Role::Warrant => Invocation::Warrant(rest),
        Role::BundledTool => Invocation::BundledTool(rest),
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
        let anchor = Role::PipelineAnchor.flag();
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
        assert_eq!(
            classify(&argv(&[Role::EnvReport.flag()])),
            Invocation::EnvReport
        );
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
    }

    #[cfg(seccomp)]
    #[test]
    fn the_guest_role_carries_its_arguments() {
        for rest in [
            &[][..],
            &["/sys/fs/cgroup/ral-exec"],
            &["--engine", "extra"],
        ] {
            let args = argv(&[&[Role::GuestEngine.flag()], rest].concat());
            assert_eq!(classify(&args), Invocation::GuestEngine(&args[1..]));
        }
    }

    /// The warrant's arguments are carried, never read here: a flag after it
    /// names no role, and the trampoline judges what it carries.
    #[test]
    fn a_warrant_reads_nothing_after_its_flag() {
        let warrant = Role::Warrant.flag();
        for rest in [
            &[][..],
            &["sh"],
            &["--engine"],
            &[Role::PipelineAnchor.flag()],
            &[Role::BundledTool.flag(), "ls"],
        ] {
            let argv = argv(&[&[warrant], rest].concat());
            assert_eq!(classify(&argv), Invocation::Warrant(&argv[1..]));
        }
    }
}
