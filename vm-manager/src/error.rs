//! Refusals and console reports cross the broker pipe as data.
//! Paths remain lossless `PathBuf`s until the caller supplies spelling to `Error::worded`.
//! Platform causes are text; their paths are carried separately.

use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const UNKEPT: &str = "This backend sends the guest's console to the owning process's standard output \
                      and keeps no copy";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuestConsole {
    Kept {
        #[serde(with = "crate::os_path::option")]
        log: Option<PathBuf>,
        tail: Vec<String>,
    },
    Unkept,
}

impl GuestConsole {
    /// The engine's failure and its retained console, with paths spelled by the caller.
    #[must_use]
    pub fn report(&self, why: &str, spell: Spelling) -> String {
        let mut out = String::new();
        out.push_str("---- Assistant engine lost ----\n");
        out.push_str(why);
        out.push_str("\n\n");
        let tail = match self {
            Self::Unkept => {
                out.push_str(UNKEPT);
                out.push_str(". No last words were retained.\n");
                return out;
            }
            Self::Kept { log, tail } => {
                if let Some(path) = log {
                    let _ = write!(
                        out,
                        "The guest's whole console is at {}. It belongs to whichever process ran the \
                         machine, which under the machine broker is a LocalSystem service, so \
                         reading it may take an administrator.\n\n",
                        spell(path)
                    );
                } else {
                    out.push_str("The guest's console could not be kept on disk.\n\n");
                }
                tail
            }
        };
        if tail.is_empty() {
            out.push_str(
                "The guest's console offered no last words. Either it never got as far as a first \
                 message, or this host could not reach the console at all; the line above says \
                 which.\n",
            );
        } else {
            out.push_str("The guest's last words on its console, oldest first:\n");
            for line in tail {
                out.push_str("  ");
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
        out
    }
}

/// Path spelling belongs to the front-end, including on the broker's far side.
pub type Spelling = fn(&Path) -> String;

/// Paths cross the pipe in platform-native units and are worded only by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Error {
    NoBackend,
    Unentitled,
    NotPermitted,
    NoSuchFolder(#[serde(with = "crate::os_path")] PathBuf),
    NotAFolder(#[serde(with = "crate::os_path")] PathBuf),
    FolderUnreadable {
        #[serde(with = "crate::os_path")]
        path: PathBuf,
        cause: String,
    },
    MissingBootFile {
        #[serde(with = "crate::os_path")]
        path: PathBuf,
        cause: String,
    },
    ServiceUnlocated {
        cause: String,
    },
    NoGuestImage {
        #[serde(with = "crate::os_path::list")]
        looked_in: Vec<PathBuf>,
    },
    CorruptGuestImage {
        #[serde(with = "crate::os_path")]
        archive: PathBuf,
    },
    HostFile {
        file: HostFile,
        act: Act,
        #[serde(with = "crate::os_path")]
        path: PathBuf,
        cause: String,
    },
    /// The service checks the caller's read rights, plus write rights unless `read_only`.
    FolderForbidden {
        #[serde(with = "crate::os_path")]
        path: PathBuf,
        read_only: bool,
        cause: String,
    },
    FolderUnexamined {
        #[serde(with = "crate::os_path")]
        path: PathBuf,
        cause: String,
    },
    /// The granted folder is a junction or a symbolic link, which the machine
    /// service does not follow.
    FolderIsLink(#[serde(with = "crate::os_path")] PathBuf),
    /// The granted folder was changed into a link while its machine was
    /// starting, so the machine was stopped.
    FolderChanged(#[serde(with = "crate::os_path")] PathBuf),
    /// Lossy spelling could name another file, so non-Unicode hypervisor paths are refused.
    NotUnicode(#[serde(with = "crate::os_path")] PathBuf),
    /// macOS requires file URLs; empty paths and embedded NULs are refused.
    NotAFileUrl(#[serde(with = "crate::os_path")] PathBuf),
    /// Must remain legible across protocol skew; its shape never changes.
    VersionSkew {
        client: u32,
        service: u32,
    },
    /// The hypervisor's complaint names no file, so the supplied files travel beside it.
    NotConstructed {
        hypervisor: Cow<'static, str>,
        why: String,
        files: Box<MachineFiles>,
    },
    /// `None` means no console could be opened; an empty retained console is distinct.
    Undialled {
        hypervisor: Cow<'static, str>,
        wire: Wire,
        within_secs: u64,
        cause: String,
        console: Option<GuestConsole>,
    },
    ForcedStop {
        hypervisor: Cow<'static, str>,
        grace_secs: u64,
    },
    /// Platform causes carry no paths; those belong in the error's data.
    Backend {
        hypervisor: Cow<'static, str>,
        why: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostFile {
    GuestImage,
    /// The checksum describes the inflated image, not its archive.
    ImageChecksum,
    ImageCache,
    WrappedImage,
    SessionDirectory,
    SessionDisk,
    DiskImage,
    /// Includes kernel, initramfs, rootfs and session disk.
    MachineFile,
    MachineCache,
    ConsolePipe,
    ConsoleLog,
}

impl HostFile {
    const fn noun(self) -> &'static str {
        match self {
            Self::GuestImage => "the guest image",
            Self::ImageChecksum => "the guest image checksum",
            Self::ImageCache => "the guest image cache",
            Self::WrappedImage => "the wrapped guest image",
            Self::SessionDirectory => "the session directory",
            Self::SessionDisk => "the session disk",
            Self::DiskImage => "the disk image",
            Self::MachineFile => "the machine's file",
            Self::MachineCache => "the machine cache",
            Self::ConsolePipe => "the console pipe",
            Self::ConsoleLog => "the console log",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Act {
    Open,
    Read,
    Unpack,
    Write,
    Flush,
    Size,
    Create,
    Make,
    Place,
    /// Given to the machine's own account, which opens it as itself.
    Grant,
}

impl Act {
    const fn participle(self) -> &'static str {
        match self {
            Self::Open => "opened",
            Self::Read => "read",
            Self::Unpack => "unpacked",
            Self::Write => "written",
            Self::Flush => "flushed",
            Self::Size => "sized",
            Self::Create => "created",
            Self::Make => "made",
            Self::Place => "moved into place",
            Self::Grant => "opened to the machine",
        }
    }
}

/// The hypervisor opens these files in its own process; any may cause an unattributed path refusal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineFiles {
    #[serde(with = "crate::os_path")]
    pub kernel: PathBuf,
    #[serde(with = "crate::os_path")]
    pub initramfs: PathBuf,
    #[serde(with = "crate::os_path")]
    pub rootfs: PathBuf,
    #[serde(with = "crate::os_path")]
    pub session: PathBuf,
    #[serde(with = "crate::os_path")]
    pub folder: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wire {
    Control,
    Net,
}

impl Wire {
    pub(crate) const fn noun(self) -> &'static str {
        match self {
            Self::Control => "the control plane",
            Self::Net => "the net wire",
        }
    }
}

impl Error {
    pub fn backend(hypervisor: &'static str, why: impl Into<String>) -> Self {
        Self::Backend {
            hypervisor: hypervisor.into(),
            why: why.into(),
        }
    }

    pub fn host_file<E: fmt::Display>(
        file: HostFile,
        act: Act,
        path: &Path,
    ) -> impl FnOnce(E) -> Self + use<E> {
        let path = path.to_path_buf();
        move |cause| Self::HostFile {
            file,
            act,
            path,
            cause: cause.to_string(),
        }
    }

    /// This error as a sentence for the person who granted the folder, with
    /// every path in it spelled by `spell`.
    #[must_use]
    pub fn worded(&self, spell: Spelling) -> Worded<'_> {
        Worded { error: self, spell }
    }
}

pub struct Worded<'a> {
    error: &'a Error,
    spell: Spelling,
}

impl fmt::Display for Worded<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spell = self.spell;
        match self.error {
            Error::NoBackend => f.write_str(
                "this computer cannot run synod's virtual machine — it starts on an Apple Silicon \
                 Mac, or on Windows with the Virtual Machine Platform",
            ),
            Error::Unentitled => f.write_str(
                "this copy of synod is not signed with the entitlement it needs to start a virtual \
                 machine — ask whoever installed synod for a properly signed build",
            ),
            Error::NotPermitted => f.write_str(
                "synod is not allowed to create virtual machines on this computer — ask whoever \
                 administers this computer to add this account to the computer's 'Hyper-V \
                 Administrators' group, or to install synod with its own installer, which \
                 registers a service that holds that one privilege so the application does not \
                 have to",
            ),
            Error::NoSuchFolder(path) => write!(
                f,
                "there is no folder at {}. Has it been moved or renamed?",
                spell(path)
            ),
            Error::NotAFolder(path) => write!(
                f,
                "{} is a file, not a folder. Please choose the folder that holds the documents to work on.",
                spell(path)
            ),
            Error::FolderUnreadable { path, cause } => write!(
                f,
                "the folder {} could not be opened: {cause}. Do you have permission to open it?",
                spell(path)
            ),
            Error::MissingBootFile { path, cause } => write!(
                f,
                "synod's virtual-machine image is missing a file it needs to start: {} ({cause}). \
                 Ask whoever installed synod to reinstall it.",
                spell(path)
            ),
            Error::ServiceUnlocated { cause } => write!(
                f,
                "synod's machine service could not tell where it is installed, and so where its \
                 guest image is: {cause}"
            ),
            Error::NoGuestImage { looked_in } => write!(
                f,
                "no guest image in {}; an installation puts it there; a checkout builds it \
                 with `just guest-boot` and `just guest-rootfs`.",
                looked_in
                    .iter()
                    .map(|path| spell(path))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Error::CorruptGuestImage { archive } => write!(
                f,
                "the guest image unpacked from {} did not match its checksum — the download is corrupt",
                spell(archive)
            ),
            Error::HostFile {
                file,
                act,
                path,
                cause,
            } => write!(
                f,
                "{} {} could not be {}: {cause}",
                file.noun(),
                spell(path),
                act.participle()
            ),
            Error::FolderForbidden {
                path,
                read_only: true,
                cause,
            } => write!(
                f,
                "the folder {} cannot be opened by the account that asked for it: {cause}",
                spell(path)
            ),
            Error::FolderForbidden {
                path,
                read_only: false,
                cause,
            } => write!(
                f,
                "the folder {} cannot be opened for changes by the account that asked for it, so \
                 it cannot be granted with changes allowed: {cause}",
                spell(path)
            ),
            Error::FolderUnexamined { path, cause } => write!(
                f,
                "synod's machine service could not examine the folder {}: {cause}",
                spell(path)
            ),
            Error::FolderIsLink(path) => write!(
                f,
                "{} is a link to another folder (a junction or a symbolic link), and synod's \
                 machine service grants only a real folder: grant the folder it points to instead",
                spell(path)
            ),
            Error::FolderChanged(path) => write!(
                f,
                "the folder {} was changed into a link to somewhere else while its machine was \
                 starting, so the machine was stopped",
                spell(path)
            ),
            Error::NotUnicode(path) => write!(
                f,
                "{} cannot be handed to the virtual machine: its name holds characters that are \
                 not valid Unicode, and the machine is told every path as text. Renaming it, or \
                 whichever folder above it has such a name, lets synod use it",
                spell(path)
            ),
            Error::NotAFileUrl(path) => write!(
                f,
                "the path {} cannot be handed to the virtual machine: it is empty or holds a \
                 NUL character, so it has no file URL, and the machine is told every file as one",
                spell(path)
            ),
            Error::VersionSkew { client, service } => write!(
                f,
                "synod and its machine service are different versions (synod speaks {client}, the \
                 service speaks {service}) — ask whoever administers this computer to install \
                 them together"
            ),
            Error::NotConstructed {
                hypervisor,
                why,
                files,
            } => write!(
                f,
                "{hypervisor} would not construct the machine: {why}. The machine's files were: \
                 kernel {}, initramfs {}, rootfs {}, session disk {}, and the granted folder {}",
                spell(&files.kernel),
                spell(&files.initramfs),
                spell(&files.rootfs),
                spell(&files.session),
                spell(&files.folder),
            ),
            Error::Undialled {
                hypervisor,
                wire,
                within_secs,
                cause,
                console,
            } => {
                write!(
                    f,
                    "the guest did not dial {} within {within_secs}s of {hypervisor} starting it: \
                     {cause}. ",
                    wire.noun()
                )?;
                console_says(f, console.as_ref(), spell)
            }
            Error::ForcedStop {
                hypervisor,
                grace_secs,
            } => write!(
                f,
                "the guest did not shut down within {grace_secs}s, so {hypervisor} stopped the \
                 machine for it — anything it had not finished writing to the folder may be \
                 missing"
            ),
            Error::Backend { hypervisor, why } => write!(f, "{hypervisor}: {why}"),
        }
    }
}

/// A silent retained console, an unkept copy and an unopened console are distinct findings.
fn console_says(
    f: &mut fmt::Formatter<'_>,
    console: Option<&GuestConsole>,
    spell: Spelling,
) -> fmt::Result {
    let (log, tail) = match console {
        None => return f.write_str("The machine layer could not open a console"),
        Some(GuestConsole::Unkept) => return f.write_str(UNKEPT),
        Some(GuestConsole::Kept { log, tail }) => (log, tail),
    };
    let kept = log.as_deref().map_or_else(
        || "and its console could not be kept on disk either".to_string(),
        |path| format!("and its whole console log is at {}", spell(path)),
    );
    if tail.is_empty() {
        write!(
            f,
            "The guest said nothing at all on its console, so it never got as far as a first \
             message — {kept}"
        )
    } else {
        write!(
            f,
            "The guest's last words on its console were {} — {kept}",
            quoted(tail)
        )
    }
}

fn quoted(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("\u{201c}{}\u{201d}", line.trim()))
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn an_empty_console_says_whose_silence_it_is() {
        let nothing = GuestConsole::Unkept.report("The assistant could not be started.", marked);
        assert!(
            nothing.contains("keeps no copy"),
            "a backend that keeps no log must say so: {nothing}"
        );
        assert!(
            nothing.contains("No last words"),
            "an empty ring must be named as empty: {nothing}"
        );

        let kept = GuestConsole::Kept {
            log: Some(PathBuf::from("C:/cache/console-1.log")),
            tail: Vec::new(),
        }
        .report("The assistant could not be started.", marked);
        assert!(
            kept.contains("<console-1.log>"),
            "a log that exists is named so a reader can open it: {kept}"
        );
        assert!(
            kept.contains("which under the machine broker is a LocalSystem service"),
            "the report names the broker without naming a product: {kept}"
        );
        assert!(
            !kept.contains("keeps no copy"),
            "the two silences must not be described in the same words: {kept}"
        );
    }

    #[test]
    fn the_guests_words_and_the_users_sentence_are_both_in_the_file() {
        let text = GuestConsole::Kept {
            log: None,
            tail: vec![
                "ral-guest: mounting /work".to_string(),
                "engine: panicked at 'no workspace'".to_string(),
            ],
        }
        .report(
            "The assistant could not be started. Try again. (details in /run)",
            marked,
        );
        assert!(
            text.contains("could not be started. Try again."),
            "the file stands alone, so it repeats the sentence the user saw: {text}"
        );
        assert!(
            text.contains("panicked at 'no workspace'"),
            "the guest's own words are the reason this file exists: {text}"
        );
        assert!(text.contains("could not be kept on disk"), "{text}");
        assert!(!text.contains("keeps no copy"), "{text}");
        assert!(
            text.lines().any(|line| line.contains("mounting /work")),
            "every retained line is kept, oldest first: {text}"
        );
    }

    fn marked(path: &Path) -> String {
        format!(
            "<{}>",
            path.file_name()
                .map_or("", |name| name.to_str().unwrap_or(""))
        )
    }

    #[test]
    fn every_path_is_spelled_by_the_front_end() {
        let error = Error::NotConstructed {
            hypervisor: "Hyper-V".into(),
            why: "the system cannot find the path specified".to_string(),
            files: Box::new(MachineFiles {
                kernel: PathBuf::from("kernel"),
                initramfs: PathBuf::from("initramfs.img"),
                rootfs: PathBuf::from("rootfs.vhd"),
                session: PathBuf::from("session.vhd"),
                folder: PathBuf::from("Invoices"),
            }),
        };
        let text = error.worded(marked).to_string();
        for name in [
            "<kernel>",
            "<initramfs.img>",
            "<rootfs.vhd>",
            "<session.vhd>",
            "<Invoices>",
        ] {
            assert!(text.contains(name), "{name} in: {text}");
        }
    }

    #[test]
    fn an_error_crosses_a_pipe_as_data() {
        let error = Error::HostFile {
            file: HostFile::SessionDisk,
            act: Act::Create,
            path: PathBuf::from("synod-session-1-2.vhd"),
            cause: "access is denied".to_string(),
        };
        let wire = serde_json::to_string(&error).unwrap();
        let back: Error = serde_json::from_str(&wire).unwrap();
        assert_eq!(
            back.worded(marked).to_string(),
            "the session disk <synod-session-1-2.vhd> could not be created: access is denied"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_unicode_crosses_a_pipe() {
        use std::os::unix::ffi::OsStrExt;

        let folder = PathBuf::from(std::ffi::OsStr::from_bytes(b"/home/r\xe9sum\xe9s"));
        let log = PathBuf::from(std::ffi::OsStr::from_bytes(b"/var/log/\xff.log"));
        assert!(folder.to_str().is_none() && log.to_str().is_none());

        let wire = serde_json::to_string(&Error::NoSuchFolder(folder.clone())).unwrap();
        let back: Error = serde_json::from_str(&wire).unwrap();
        assert!(matches!(back, Error::NoSuchFolder(path) if path == folder));

        let looked_in = vec![folder, log.clone()];
        let wire = serde_json::to_string(&Error::NoGuestImage {
            looked_in: looked_in.clone(),
        })
        .unwrap();
        let back: Error = serde_json::from_str(&wire).unwrap();
        assert!(matches!(back, Error::NoGuestImage { looked_in: paths } if paths == looked_in));

        let undialled = Error::Undialled {
            hypervisor: "Hyper-V".into(),
            wire: Wire::Net,
            within_secs: 60,
            cause: "timed out".to_string(),
            console: Some(GuestConsole::Kept {
                log: Some(log.clone()),
                tail: Vec::new(),
            }),
        };
        let wire = serde_json::to_string(&undialled).unwrap();
        let back: Error = serde_json::from_str(&wire).unwrap();
        assert!(matches!(
            back,
            Error::Undialled { console: Some(GuestConsole::Kept { log: Some(path), .. }), .. }
                if path == log
        ));
    }

    #[cfg(windows)]
    #[test]
    fn a_path_that_is_not_unicode_crosses_a_pipe() {
        use std::os::windows::ffi::OsStringExt;

        let folder = PathBuf::from(std::ffi::OsString::from_wide(&[
            u16::from(b'C'),
            u16::from(b':'),
            u16::from(b'\\'),
            0xD800,
        ]));
        assert!(folder.to_str().is_none());

        let wire = serde_json::to_string(&Error::FolderIsLink(folder.clone())).unwrap();
        let back: Error = serde_json::from_str(&wire).unwrap();
        assert!(matches!(back, Error::FolderIsLink(path) if path == folder));

        let wire = serde_json::to_string(&Error::NoGuestImage {
            looked_in: vec![folder.clone()],
        })
        .unwrap();
        let back: Error = serde_json::from_str(&wire).unwrap();
        assert!(matches!(back, Error::NoGuestImage { looked_in } if looked_in == [folder]));
    }

    #[test]
    fn a_failing_boot_quotes_the_guest() {
        let said = quoted(&[
            "  EXT4-fs (sda): mounted filesystem  ".to_string(),
            "ral-guest: no ral.port on the command line".to_string(),
        ]);
        assert_eq!(
            said,
            "\u{201c}EXT4-fs (sda): mounted filesystem\u{201d} / \u{201c}ral-guest: no ral.port \
             on the command line\u{201d}"
        );
    }

    #[test]
    fn a_silent_guest_and_a_missing_console_are_different_sentences() {
        let undialled = |console| Error::Undialled {
            hypervisor: "Hyper-V".into(),
            wire: Wire::Control,
            within_secs: 60,
            cause: "timed out".to_string(),
            console,
        };
        let silence = undialled(Some(GuestConsole::Kept {
            log: Some(PathBuf::from("synod-console-0000-mute.log")),
            tail: Vec::new(),
        }))
        .worded(marked)
        .to_string();
        assert!(silence.contains("said nothing at all"), "{silence}");
        assert!(
            silence.contains("<synod-console-0000-mute.log>"),
            "the log must be named for a reader to open: {silence}"
        );

        let unkept = undialled(Some(GuestConsole::Unkept))
            .worded(marked)
            .to_string();
        assert!(unkept.contains("keeps no copy"), "{unkept}");
        assert!(!unkept.contains("said nothing"), "{unkept}");

        let lost = undialled(None).worded(marked).to_string();
        assert!(lost.contains("could not open a console"), "{lost}");
        assert!(
            !lost.contains("above"),
            "nothing may point at output a service never wrote: {lost}"
        );
    }
}
