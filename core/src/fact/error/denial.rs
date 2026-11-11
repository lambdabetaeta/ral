//! Authority refusals contain requests and causes, never runtime errors.

use crate::fact::{Check, Resource};
use crate::{label, record, variant};
use strum::{IntoStaticStr, VariantArray};

pub use crate::authority::grant::value::Target as ExecExclusion;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    ExecRun(DenialExecRun),
    ExecRead(DenialExecRead),
    Fs(DenialFs),
    Flag(DenialFlag),
    Device(DenialDevice),
}
variant!(Denial {
    ExecRun(DenialExecRun): "exec-run", ExecRead(DenialExecRead): "exec-read",
    Fs(DenialFs): "fs", Flag(DenialFlag): "flag", Device(DenialDevice): "device",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialExecRun {
    requested: String,
    cause: Withholding<ExecExclusion>,
}
record!(DenialExecRun {
    requested: "requested",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialExecRead {
    requested: String,
    subject: ExecReadSubject,
    cause: Withholding<String>,
}
record!(DenialExecRead {
    requested: "requested",
    subject: "subject",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialFs {
    requested: String,
    op: ReadOrWrite,
    cause: FsRefusal,
}
record!(DenialFs {
    requested: "requested",
    op: "op",
    cause: "cause"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialFlag {
    who: String,
    flag: FlagId,
}
record!(DenialFlag {
    who: "who",
    flag: "flag"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialDevice {
    requested: String,
    device: String,
}
record!(DenialDevice {
    requested: "requested",
    device: "device"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Withholding<T> {
    MissingAllowance(SingleOrNested),
    Excluded(WithholdingExcluded<T>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithholdingExcluded<T> {
    pub scope: T,
    pub matched_by: ExclusionMatch,
}
record!(WithholdingExcluded<ExecExclusion> { scope: "scope", matched_by: "matched-by" });
record!(WithholdingExcluded<String> { scope: "scope", matched_by: "matched-by" });
variant!(Withholding<ExecExclusion> {
    MissingAllowance(SingleOrNested): "missing-allowance",
    Excluded(WithholdingExcluded<ExecExclusion>): "excluded",
});
variant!(Withholding<String> {
    MissingAllowance(SingleOrNested): "missing-allowance",
    Excluded(WithholdingExcluded<String>): "excluded",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum SingleOrNested {
    Single,
    Nested,
}
label!(SingleOrNested);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ExclusionMatch {
    Name,
    FoldedName,
    Identity,
}
label!(ExclusionMatch);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ReadOrWrite {
    Read,
    Write,
}
label!(ReadOrWrite);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum PinnedBinary {
    Ral,
    Bwrap,
}
label!(PinnedBinary);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsRefusal {
    Withheld(Withholding<String>),
    Pinned(PinnedBinary),
}
variant!(FsRefusal { Withheld(Withholding<String>): "withheld", Pinned(PinnedBinary): "pinned" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecReadSubject {
    Executable,
    Bundled(ExecReadSubjectBundled),
}
variant!(ExecReadSubject { Executable: "executable", Bundled(ExecReadSubjectBundled): "bundled" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecReadSubjectBundled {
    pub tool: String,
    pub binary: Option<String>,
}
record!(ExecReadSubjectBundled {
    tool: "tool",
    binary: "binary"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagId {
    Editor(EditorFlag),
    Shell(ShellFlag),
}
variant!(FlagId { Editor(EditorFlag): "editor", Shell(ShellFlag): "shell" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum EditorFlag {
    Read,
    Write,
    Tui,
}
label!(EditorFlag);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ShellFlag {
    Chdir,
    Detach,
}
label!(ShellFlag);

impl Denial {
    pub(crate) fn exec_run(requested: String, cause: Withholding<ExecExclusion>) -> Self {
        Self::ExecRun(DenialExecRun { requested, cause })
    }

    pub(crate) fn exec_read(requested: String, cause: Withholding<String>) -> Self {
        Self::ExecRead(DenialExecRead {
            requested,
            subject: ExecReadSubject::Executable,
            cause,
        })
    }

    pub(crate) fn bundled_read(
        requested: String,
        tool: String,
        binary: Option<String>,
        exclusion: WithholdingExcluded<String>,
    ) -> Self {
        Self::ExecRead(DenialExecRead {
            requested,
            subject: ExecReadSubject::Bundled(ExecReadSubjectBundled { tool, binary }),
            cause: Withholding::Excluded(exclusion),
        })
    }

    pub(crate) fn fs_withheld(
        requested: String,
        op: ReadOrWrite,
        cause: Withholding<String>,
    ) -> Self {
        Self::Fs(DenialFs {
            requested,
            op,
            cause: FsRefusal::Withheld(cause),
        })
    }

    pub(crate) fn pinned_write(requested: String, binary: PinnedBinary) -> Self {
        Self::Fs(DenialFs {
            requested,
            op: ReadOrWrite::Write,
            cause: FsRefusal::Pinned(binary),
        })
    }

    pub(crate) fn flag(who: String, flag: FlagId) -> Self {
        Self::Flag(DenialFlag { who, flag })
    }

    pub(crate) fn device(requested: String, device: String) -> Self {
        Self::Device(DenialDevice { requested, device })
    }

    pub fn missing_allowance(&self) -> bool {
        match self {
            Self::ExecRun(request) => matches!(request.cause, Withholding::MissingAllowance(_)),
            Self::ExecRead(request) => matches!(request.cause, Withholding::MissingAllowance(_)),
            Self::Fs(request) => matches!(
                request.cause,
                FsRefusal::Withheld(Withholding::MissingAllowance(_))
            ),
            Self::Flag(_) | Self::Device(_) => false,
        }
    }

    pub fn audit_check(&self) -> Option<Check> {
        let (resource, mut fields) = match self {
            Self::ExecRun(request) => {
                let mut fields =
                    std::collections::BTreeMap::from([("name".into(), request.requested.clone())]);
                if let Withholding::Excluded(exclusion) = &request.cause {
                    fields.insert("deny".into(), exclusion.scope.to_string());
                }
                (Resource::Exec, fields)
            }
            Self::ExecRead(request) => {
                let mut fields =
                    std::collections::BTreeMap::from([("name".into(), request.requested.clone())]);
                if let Withholding::Excluded(exclusion) = &request.cause {
                    fields.insert("deny".into(), exclusion.scope.clone());
                }
                (Resource::Exec, fields)
            }
            Self::Fs(request) => {
                let fields = std::collections::BTreeMap::from([
                    ("op".into(), <&str>::from(request.op).to_owned()),
                    ("path".into(), request.requested.clone()),
                ]);
                (Resource::Fs, fields)
            }
            Self::Flag(_) | Self::Device(_) => return None,
        };
        if let Self::Fs(request) = self {
            match &request.cause {
                FsRefusal::Withheld(Withholding::Excluded(exclusion)) => {
                    fields.insert("deny".into(), exclusion.scope.clone());
                }
                FsRefusal::Pinned(binary) => {
                    fields.insert("pinned".into(), <&str>::from(*binary).to_owned());
                }
                FsRefusal::Withheld(Withholding::MissingAllowance(_)) => {}
            }
        }
        Some(Check::new(resource, fields))
    }

    pub fn describe(&self) -> String {
        match self {
            Self::ExecRun(request) => {
                let cause = match &request.cause {
                    Withholding::MissingAllowance(SingleOrNested::Single) => {
                        "nothing in the grant's exec `allow` list lets it run".into()
                    }
                    Withholding::MissingAllowance(SingleOrNested::Nested) => {
                        "some layer's exec `allow` list does not let it run".into()
                    }
                    Withholding::Excluded(exclusion) => exclusion.describe(
                        &format!("{:?}", request.requested),
                        &crate::text::code(&exclusion.scope.to_string()),
                    ),
                };
                format!(
                    "command {:?} denied by active grant: {cause}",
                    request.requested
                )
            }
            Self::ExecRead(request) => {
                let (lead, subject) = match &request.subject {
                    ExecReadSubject::Executable => {
                        (String::new(), format!("{:?}", request.requested))
                    }
                    ExecReadSubject::Bundled(tool) => (
                        format!("{:?} is built into ral, and ", tool.tool),
                        tool.binary.as_ref().map_or_else(
                            || "ral's binary".into(),
                            |binary| format!("ral's binary {binary:?}"),
                        ),
                    ),
                };
                let cause = match &request.cause {
                    Withholding::MissingAllowance(_) => {
                        format!("{subject} may run, but it is not readable under the active grant")
                    }
                    Withholding::Excluded(exclusion) => {
                        exclusion.describe(&subject, &format!("the fs deny {:?}", exclusion.scope))
                    }
                };
                format!(
                    "command {:?} denied by active grant: {lead}{cause}",
                    request.requested
                )
            }
            Self::Fs(request) => {
                let op = <&str>::from(request.op);
                match &request.cause {
                    FsRefusal::Withheld(Withholding::MissingAllowance(_)) => format!(
                        "grant does not allow {op} access to {:?}",
                        request.requested
                    ),
                    FsRefusal::Withheld(Withholding::Excluded(exclusion)) => format!(
                        "fs {op} denied by grant: {}",
                        exclusion.describe(
                            &format!("{:?}", request.requested),
                            &format!("{:?}", exclusion.scope),
                        )
                    ),
                    FsRefusal::Pinned(binary) => format!(
                        "fs {op} refused: {:?} is the {} binary ral pinned at startup, which confines every command run under a grant",
                        request.requested,
                        <&str>::from(*binary)
                    ),
                }
            }
            Self::Flag(request) => match request.flag {
                FlagId::Shell(ShellFlag::Detach) => format!(
                    "{}: an active grant withholds it, so nothing here may outlive this session",
                    request.who
                ),
                flag => format!("denied: {} requires {}", request.who, flag.describe()),
            },
            Self::Device(request) => format!(
                "path {:?} names the reserved DOS device {:?}, not a filesystem object",
                request.requested, request.device
            ),
        }
    }

    pub fn advice(&self) -> Vec<String> {
        const NESTED: &str = "an enclosing grant may be the one that withholds it, and a nested grant cannot add authority its caller withholds";
        const EXCLUSION: &str = "an allowance cannot override an exclusion; remove the exclusion from the policy that imposes it";
        let advice = match self {
            Self::ExecRun(request) => match &request.cause {
                Withholding::MissingAllowance(SingleOrNested::Single) => {
                    "add it to the grant's exec `allow` list: `` `name git `` for a command name, `` `absolute /path/to/it `` for a file, or `` `dir `absolute /path `` for its directory"
                }
                Withholding::MissingAllowance(SingleOrNested::Nested) => NESTED,
                Withholding::Excluded(_) => EXCLUSION,
            },
            Self::ExecRead(request) => match &request.cause {
                Withholding::MissingAllowance(SingleOrNested::Single) => {
                    "running a program reads it: add it to the grant's fs `read` list"
                }
                Withholding::MissingAllowance(SingleOrNested::Nested) => NESTED,
                Withholding::Excluded(_) => EXCLUSION,
            },
            Self::Fs(request) => match &request.cause {
                FsRefusal::Withheld(Withholding::MissingAllowance(SingleOrNested::Nested)) => {
                    NESTED
                }
                FsRefusal::Withheld(Withholding::MissingAllowance(SingleOrNested::Single)) => {
                    match request.op {
                        ReadOrWrite::Read => "add it to the grant's fs `read` or `write` list",
                        ReadOrWrite::Write => "add it to the grant's fs `write` list",
                    }
                }
                FsRefusal::Withheld(Withholding::Excluded(_)) => EXCLUSION,
                FsRefusal::Pinned(_) => {
                    "ral never writes into the binary that enforces its grants, whatever the grant allows. Install a new one by replacing the file rather than rewriting it: a rename leaves this session's pinned copy intact"
                }
            },
            Self::Flag(request) => match request.flag {
                FlagId::Shell(ShellFlag::Detach) => {
                    "Would `spawn` do, since it ends when the session does?"
                }
                FlagId::Shell(ShellFlag::Chdir) | FlagId::Editor(_) => return Vec::new(),
            },
            Self::Device(request) if request.is_null_device() => {
                r"use \\.\NUL to name the Windows null device explicitly"
            }
            Self::Device(_) => return Vec::new(),
        };
        vec![advice.to_owned()]
    }
}

impl<T> WithholdingExcluded<T> {
    fn describe(&self, subject: &str, scope: &str) -> String {
        match self.matched_by {
            ExclusionMatch::Name => format!("{subject} is excluded by {scope}"),
            ExclusionMatch::FoldedName => format!(
                "{subject} is excluded by {scope} after case or Unicode folding; a deny holds under every spelling"
            ),
            ExclusionMatch::Identity => {
                format!(
                    "{subject} is excluded by {scope} through a filesystem object reached under another name"
                )
            }
        }
    }
}

impl FlagId {
    pub fn describe(self) -> String {
        match self {
            Self::Editor(flag) => format!("editor: [`{}]", <&str>::from(flag)),
            Self::Shell(flag) => format!("shell: [`{}]", <&str>::from(flag)),
        }
    }
}

impl DenialDevice {
    fn is_null_device(&self) -> bool {
        let component = self.device.trim_end_matches([' ', '.']);
        let stem = component
            .split_once('.')
            .map_or(component, |(stem, _)| stem);
        stem.trim_end_matches([' ', '.'])
            .eq_ignore_ascii_case("NUL")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::grant::freeze::Anchor;
    use crate::data::datum::Datum;
    use crate::text::code;

    fn matched_description(matched_by: ExclusionMatch, subject: &str, scope: &str) -> String {
        match matched_by {
            ExclusionMatch::Name => format!("{subject} is excluded by {scope}"),
            ExclusionMatch::FoldedName => format!(
                "{subject} is excluded by {scope} after case or Unicode folding; a deny holds under every spelling"
            ),
            ExclusionMatch::Identity => {
                format!(
                    "{subject} is excluded by {scope} through a filesystem object reached under another name"
                )
            }
        }
    }

    fn assert_projection(denial: &Denial, expected: &str) {
        assert_eq!(denial.describe(), expected);
        let decoded = Denial::decode(&denial.clone().encode()).unwrap();
        assert_eq!(&decoded, denial);
        assert_eq!(decoded.describe(), expected);
        assert_eq!(decoded.advice(), denial.advice());
        assert_eq!(decoded.audit_check(), denial.audit_check());
    }

    #[test]
    fn denial_exclusions_preserve_the_exec_run_exec_read_and_fs_axes() {
        for matched_by in [
            ExclusionMatch::Name,
            ExclusionMatch::FoldedName,
            ExclusionMatch::Identity,
        ] {
            let target = ExecExclusion::File(Anchor::Absolute.place("/safe/tool"));
            assert_projection(
                &Denial::exec_run(
                    "tool".into(),
                    Withholding::Excluded(WithholdingExcluded {
                        scope: target.clone(),
                        matched_by,
                    }),
                ),
                &format!(
                    "command \"tool\" denied by active grant: {}",
                    matched_description(matched_by, "\"tool\"", &code(&target.to_string()))
                ),
            );
            assert_projection(
                &Denial::exec_read(
                    "tool".into(),
                    Withholding::Excluded(WithholdingExcluded {
                        scope: "/safe/tool".into(),
                        matched_by,
                    }),
                ),
                &format!(
                    "command \"tool\" denied by active grant: {}",
                    matched_description(matched_by, "\"tool\"", "the fs deny \"/safe/tool\"")
                ),
            );
            assert_projection(
                &Denial::fs_withheld(
                    "/requested".into(),
                    ReadOrWrite::Read,
                    Withholding::Excluded(WithholdingExcluded {
                        scope: "/safe/tool".into(),
                        matched_by,
                    }),
                ),
                &format!(
                    "fs read denied by grant: {}",
                    matched_description(matched_by, "\"/requested\"", "\"/safe/tool\"")
                ),
            );
        }
    }

    #[test]
    fn denial_bundled_exec_read_names_the_fs_exclusion_and_safe_binary() {
        for (binary, subject) in [
            (None, "ral's binary"),
            (Some("/safe/ral".into()), "ral's binary \"/safe/ral\""),
        ] {
            assert_projection(
                &Denial::bundled_read(
                    "ls".into(),
                    "ls".into(),
                    binary,
                    WithholdingExcluded {
                        scope: "/safe".into(),
                        matched_by: ExclusionMatch::Name,
                    },
                ),
                &format!(
                    "command \"ls\" denied by active grant: \"ls\" is built into ral, and {subject} is excluded by the fs deny \"/safe\""
                ),
            );
        }
    }
}
