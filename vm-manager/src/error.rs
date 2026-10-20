//! Why the machine layer refused, as data rather than as words.
//!
//! Every failure this crate reports is an [`Error`], and every path in one is
//! carried as the [`PathBuf`] it is rather than spelled into a sentence.  How a
//! path reads to a person is the front-end's convention, not the machine
//! layer's — synod spells every path `/`-separated, on Windows too — so the
//! sentence is assembled only where it is shown, by [`Error::worded`], from the
//! spelling the front-end hands it.
//!
//! Serialisable for the same reason.  A refusal made in the machine service
//! crosses the broker's pipe as this data (`broker::Reply`, Windows only) and
//! is worded in the process that shows it, with that process's spelling, which
//! a `LocalSystem` service has no business choosing.
//!
//! The causes are text.  Each is the platform's own account of a failed call —
//! an [`std::io::Error`], an HCS result, an `NSError` — rendered where it
//! happened: none of them names a path, which is this crate's to carry beside
//! it, and none could cross the pipe as the type it was.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{GuestConsole, MachineSpec};

/// How a front-end spells a path for a person — synod's is
/// `ral_core::path::shown`.
pub type Spelling = fn(&Path) -> String;

/// Why no machine could be had, or why one was not readied, started, or
/// stopped cleanly.
///
/// Worded by [`Error::worded`], for the person who granted the folder rather
/// than for a programmer.  A sentence says what went wrong and never at which
/// stage: the front-end asked for a backend, a boot, or a stop, and so it is
/// the one that knows — and says — which of those failed.
///
/// Every path is serialised as the platform's own units ([`crate::os_path`]),
/// so a refusal naming a path that is not Unicode still crosses the broker's
/// pipe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Error {
    /// This computer has no backend at all: it is neither an Apple Silicon
    /// Mac nor a Windows with the Virtual Machine Platform.
    NoBackend,
    /// The platform could run a machine, but the application has no boot
    /// media to start one from.
    NoBootMedia,
    /// This process is not signed with the entitlement Virtualization.framework
    /// demands.
    Unentitled,
    /// The compute service will not create machines for this account
    /// (Windows only).
    NotPermitted,
    /// The granted folder is not there.
    NoSuchFolder(#[serde(with = "crate::os_path")] PathBuf),
    /// The granted path exists but is not a folder.
    NotAFolder(#[serde(with = "crate::os_path")] PathBuf),
    /// The granted folder is there but could not be read.
    FolderUnreadable {
        #[serde(with = "crate::os_path")]
        path: PathBuf,
        cause: String,
    },
    /// The workspace's mount point inside the guest is not an absolute path.
    GuestPathNotAbsolute(#[serde(with = "crate::os_path")] PathBuf),
    /// The spec asked for a machine with no processors.
    NoProcessors,
    /// The spec asked for less memory than a guest needs to boot.
    TooLittleMemory { asked_mib: u32, least_mib: u32 },
    /// A file of the boot media this build ships is missing or unreadable.
    MissingBootFile {
        #[serde(with = "crate::os_path")]
        path: PathBuf,
        cause: String,
    },
    /// The machine service could not tell where it is installed, and so where
    /// its boot media is.
    ServiceUnlocated { cause: String },
    /// The machine service found no boot media installed beside itself, in
    /// `looked_in`, nor a checkout's image pipeline output.
    NoGuestImage {
        #[serde(with = "crate::os_path")]
        looked_in: PathBuf,
    },
    /// The guest image inflated from `archive` did not match its checksum.
    CorruptGuestImage {
        #[serde(with = "crate::os_path")]
        archive: PathBuf,
    },
    /// A file the machine layer keeps on the host could not be handled.
    HostFile {
        file: HostFile,
        act: Act,
        #[serde(with = "crate::os_path")]
        path: PathBuf,
        cause: String,
    },
    /// The machine service could not open the granted folder as the account
    /// that asked for it — for changes, unless `read_only`.
    FolderForbidden {
        #[serde(with = "crate::os_path")]
        path: PathBuf,
        read_only: bool,
        cause: String,
    },
    /// The machine service could not tell what the granted folder is, or
    /// where it really is.
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
    /// A path the hypervisor would be handed is not Unicode, and the
    /// hypervisor is told every path as text.
    ///
    /// Refused rather than approximated: a lossy spelling is the name of some
    /// *other* file or folder, which may exist, and may be a link to anywhere.
    NotUnicode(#[serde(with = "crate::os_path")] PathBuf),
    /// The client and the machine service speak different versions of the
    /// broker's protocol (Windows only).
    ///
    /// The one refusal that has to stay legible across a skew, so its shape is
    /// never to change — see `broker::VERSION`.
    VersionSkew { client: u32, service: u32 },
    /// The hypervisor would not construct the machine.  Its own complaint
    /// names no file, so the files it was handed are carried beside it.
    NotConstructed {
        hypervisor: Cow<'static, str>,
        why: String,
        files: Box<MachineFiles>,
    },
    /// The guest did not dial `wire` within `within_secs` of starting.
    /// `console` is what it said on its way to not dialling, or `None` if no
    /// console could be opened for it at all.
    Undialled {
        hypervisor: Cow<'static, str>,
        wire: Wire,
        within_secs: u64,
        cause: String,
        console: Option<GuestConsole>,
    },
    /// The guest did not power itself off within `grace_secs`, so the machine
    /// was stopped for it.
    ForcedStop {
        hypervisor: Cow<'static, str>,
        grace_secs: u64,
    },
    /// Any other failure of the backend, in its own words — which name no
    /// path: the hypervisor's complaint, or the machine layer's account of the
    /// host-side plumbing around it.
    Backend {
        hypervisor: Cow<'static, str>,
        why: String,
    },
}

/// Which host file an [`Error::HostFile`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostFile {
    /// The rootfs image the application ships, compressed or not.
    GuestImage,
    /// The rootfs as inflated from its archive.
    UnpackedImage,
    /// The checksum the rootfs archive ships with, of the image it inflates to.
    ImageChecksum,
    /// The record that the unpacked rootfs matched its checksum.
    UnpackMarker,
    /// The directory the inflated or wrapped rootfs is kept in.
    ImageCache,
    /// The rootfs wrapped as a fixed VHD.
    WrappedImage,
    /// The record of which image the wrapped rootfs was made from.
    WrapMarker,
    /// The directory a session's disk is made in.
    SessionDirectory,
    /// A session's read-write disk.
    SessionDisk,
    /// A disk image being attached to the machine.
    DiskImage,
    /// One of the files a machine is given access to: its kernel, initramfs,
    /// rootfs, or session disk.
    MachineFile,
    /// The directory a backend keeps its machines' own files in.
    MachineCache,
    /// The named pipe a machine's console is read from.
    ConsolePipe,
    /// The file a machine's console is kept in.
    ConsoleLog,
}

impl HostFile {
    const fn noun(self) -> &'static str {
        match self {
            Self::GuestImage => "the guest image",
            Self::UnpackedImage => "the unpacked guest image",
            Self::ImageChecksum => "the guest image checksum",
            Self::UnpackMarker => "the unpacked guest image's marker",
            Self::ImageCache => "the guest image cache",
            Self::WrappedImage => "the wrapped guest image",
            Self::WrapMarker => "the wrapped guest image's marker",
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

/// What could not be done to a [`HostFile`].
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

/// The files a machine's document hands the hypervisor, every one of which it
/// opens in its own process — so each is a candidate when it says only that
/// some path could not be found.
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

/// Which of a machine's two wires an [`Error::Undialled`] waited for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wire {
    Control,
    Net,
}

impl Wire {
    const fn noun(self) -> &'static str {
        match self {
            Self::Control => "the control plane",
            Self::Net => "the net wire",
        }
    }
}

impl Error {
    /// An [`Error::HostFile`] about `path`, for a `map_err` at the call that
    /// failed on it.
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

/// An [`Error`] with the spelling it is to be worded in — see
/// [`Error::worded`].
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
            Error::NoBootMedia => f.write_str(
                "synod could not find the virtual-machine image it ships with — ask whoever \
                 installed synod to reinstall it",
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
            Error::GuestPathNotAbsolute(path) => write!(
                f,
                "the folder has to appear inside the machine at a full path such as {}, but {} is not one",
                MachineSpec::GUEST_WORKSPACE,
                spell(path)
            ),
            Error::NoProcessors => {
                f.write_str("a machine needs at least one processor, and none were asked for")
            }
            Error::TooLittleMemory {
                asked_mib,
                least_mib,
            } => write!(
                f,
                "a machine needs at least {least_mib} MB of memory to start, and only {asked_mib} MB were asked for"
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
                "the machine service found no guest image: it looked beside itself, in {}, and for \
                 the image pipeline's own output if this is a checkout. An installation puts the \
                 image there; a checkout builds it with `just guest-boot amd64` and `just \
                 guest-rootfs amd64`",
                spell(looked_in)
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

/// What a guest that would not dial has to say for itself, as the second half
/// of the boot failure's sentence.
///
/// The message this replaces told the reader the console was "above", which on
/// an installed synod it never was: the broker is a `LocalSystem` service and
/// its standard output goes nowhere, so the one line explaining the refusal was
/// written and thrown away.  Three answers are possible and each leaves the
/// reader somewhere different:
///
/// - the guest **spoke**, and its last words are quoted here, which is usually
///   the whole diagnosis;
/// - the guest **said nothing**, which is itself the finding — a machine that
///   started and never reached a kernel message — so the log is named for
///   whoever wants to confirm the emptiness;
/// - there was **no console**, so the diagnostic was lost before the machine
///   existed, and saying so is better than implying the guest was silent.
fn console_says(
    f: &mut fmt::Formatter<'_>,
    console: Option<&GuestConsole>,
    spell: Spelling,
) -> fmt::Result {
    let Some(console) = console else {
        return f.write_str(
            "Synod could not open a console for this machine, so the guest has no words to quote \
             — and that is a fault in synod, not in the guest",
        );
    };
    let kept = console.log.as_deref().map_or_else(
        || "and its console could not be kept on disk either".to_string(),
        |path| format!("and its whole console log is at {}", spell(path)),
    );
    if console.tail.is_empty() {
        write!(
            f,
            "The guest said nothing at all on its console, so it never got as far as a first \
             message — {kept}"
        )
    } else {
        write!(
            f,
            "The guest's last words on its console were {} — {kept}",
            quoted(&console.tail)
        )
    }
}

/// Console lines as one quotation: oldest first, each in its own quotes and
/// separated by a slash, so a reader can see where one line ended and the next
/// began without the error becoming a dump.
fn quoted(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("\u{201c}{}\u{201d}", line.trim()))
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spelling no platform uses, so a test can see that every path in a
    /// sentence went through it.
    #[allow(
        clippy::disallowed_methods,
        reason = "[test] a fixture's ASCII file name, read back to mark it"
    )]
    fn marked(path: &Path) -> String {
        format!(
            "<{}>",
            path.file_name()
                .map_or("", |name| name.to_str().unwrap_or(""))
        )
    }

    /// Every path a sentence names is spelled by the front-end, never by the
    /// machine layer — including each of the five a refused construction
    /// carries.
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

    /// A refusal survives the broker's pipe as the data it was, to be worded
    /// on the far side.
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

    /// A path that is not Unicode crosses the pipe whole — in a variant of its
    /// own and inside a console's log alike — rather than failing the frame
    /// that was to carry the refusal.
    #[cfg(unix)]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "[test] asserting the fixture really is not Unicode"
    )]
    fn a_path_that_is_not_unicode_crosses_a_pipe() {
        use std::os::unix::ffi::OsStrExt;

        let folder = PathBuf::from(std::ffi::OsStr::from_bytes(b"/home/r\xe9sum\xe9s"));
        let log = PathBuf::from(std::ffi::OsStr::from_bytes(b"/var/log/\xff.log"));
        assert!(folder.to_str().is_none() && log.to_str().is_none());

        let wire = serde_json::to_string(&Error::NoSuchFolder(folder.clone())).unwrap();
        let back: Error = serde_json::from_str(&wire).unwrap();
        assert!(matches!(back, Error::NoSuchFolder(path) if path == folder));

        let undialled = Error::Undialled {
            hypervisor: "Hyper-V".into(),
            wire: Wire::Net,
            within_secs: 60,
            cause: "timed out".to_string(),
            console: Some(GuestConsole {
                log: Some(log.clone()),
                tail: Vec::new(),
            }),
        };
        let wire = serde_json::to_string(&undialled).unwrap();
        let back: Error = serde_json::from_str(&wire).unwrap();
        assert!(matches!(
            back,
            Error::Undialled { console: Some(GuestConsole { log: Some(path), .. }), .. }
                if path == log
        ));
    }

    /// The same law on Windows, whose paths are UTF-16 and may hold an
    /// unpaired surrogate that no `str` can.
    #[cfg(windows)]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "[test] asserting the fixture really is not Unicode"
    )]
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
    }

    /// A guest that will not dial is quoted, not pointed at: the failure
    /// carries the console's last lines, oldest first, each in its own quotes,
    /// because under a service there is no "above" to point to.
    #[test]
    fn a_failing_boot_quotes_the_guest() {
        let said = quoted(&[
            "  EXT4-fs (sda): mounted filesystem  ".to_string(),
            "ral-daemon: no ral.port on the command line".to_string(),
        ]);
        assert_eq!(
            said,
            "\u{201c}EXT4-fs (sda): mounted filesystem\u{201d} / \u{201c}ral-daemon: no ral.port \
             on the command line\u{201d}"
        );
    }

    /// A guest that said nothing is said to have said nothing, and its log is
    /// named so a reader can confirm the silence rather than being told to look
    /// "above" at output a service never wrote anywhere.  A session with no
    /// console at all admits the loss is synod's.
    #[test]
    fn a_silent_guest_and_a_missing_console_are_different_sentences() {
        let undialled = |console| Error::Undialled {
            hypervisor: "Hyper-V".into(),
            wire: Wire::Control,
            within_secs: 60,
            cause: "timed out".to_string(),
            console,
        };
        let silence = undialled(Some(GuestConsole {
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

        let lost = undialled(None).worded(marked).to_string();
        assert!(lost.contains("could not open a console"), "{lost}");
        assert!(
            !lost.contains("above"),
            "nothing may point at output a service never wrote: {lost}"
        );
    }
}
