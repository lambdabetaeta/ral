//! The folder grant — synod's whole notion of authority.
//!
//! A synod session begins when someone points at a folder.  Everything
//! downstream of that gesture is derived here and nowhere else: the
//! machine that will hold the folder ([`Grant::machine_spec`]) and the
//! capabilities the session runs under ([`Grant::capabilities`]).  There
//! is no second place where authority is widened, so reading this file
//! is reading the entire answer to "what can synod touch?".
//!
//! The answer is deliberately small.  `SYNOD.md` §8 lists eight security
//! layers; the grant is layer 2, *topology as policy* — credentials,
//! `$HOME`, the user's other folders and the network are out of reach by
//! absence, not by a rule that could be argued with.  This module is the
//! typed statement of that absence.
//!
//! ## Namespace
//!
//! [`Grant::root`] is a **host** path: the folder as the user picked it,
//! for everything that happens on this side of the wall — the mount, the
//! checkpoints, the window's own words.  The *engine* lives inside the
//! guest, where the folder appears at
//! [`MachineSpec::GUEST_WORKSPACE`](vm_manager::MachineSpec::GUEST_WORKSPACE)
//! and the working space is the guest's own [`GUEST_SCRATCH`] tmpfs, so
//! [`Grant::capabilities`] is minted over those guest paths: the value
//! rides every turn into the guest and is enforced there, where a host
//! path names nothing.  One substitution, in one function, which is why
//! the construction lives here and is not scattered across the session.

use ral_core::path::NormalizedPrefix;
use ral_core::types::{Capabilities, EditorPolicy, ExecMap, ExecPolicy, FsPolicy, ShellPolicy};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The office toolbox of `SYNOD.md` §7, as an allowlist of *literal
/// command names* — `ExecMap::dirs` stays empty.
///
/// [`ExecMap`] admits either bare-name literals or directory prefixes,
/// and exarch's profiles lean hard on the directory half because a
/// developer's tool roots are open-ended: Homebrew, rustup toolchains,
/// nvm and pyenv install binaries nobody can enumerate in advance, so
/// the honest grant is "whatever lives under these roots".
///
/// Synod's toolbox is the opposite shape.  §7 makes the image the
/// package manager — the guest has no network, so the rootfs is fixed,
/// finite, and known before the user ever opens the app.  A directory
/// grant on `/usr/bin` would therefore hand the agent every package the
/// *next* image build happens to add, silently widening the grant each
/// time the rootfs is repackaged, and it would quietly re-admit exactly
/// the compilers and build systems §7 cuts on purpose.  Naming each tool
/// keeps the image and the grant two independent reviews, and leaves a
/// policy a person can read start to finish.
///
/// Two things this list is not.  It is not a security boundary: `python3`
/// is on it, and a Python process can spawn whatever the image contains,
/// as can `find -exec`, so the map launders nothing.  The boundary is the
/// VM wall and the absence of a network (§1, §8); the map is a statement
/// of the *job*, and a tripwire when the agent wanders off it.  And it is
/// not a shell: `sh`, `bash`, `env` and `xargs` are absent because ral is
/// the shell here — the agent loops and pipes in ral, so a shell-out
/// would buy nothing that ral does not already do.  That keeps the
/// *common* path legible; it does not seal the map, and nothing here
/// pretends it does.
const TOOLBOX: &[&str] = &[
    // Microsoft formats, end to end: read, write, convert.
    "soffice",
    "libreoffice",
    // Everything else that is a document.
    "pandoc",
    // PDF: read it, split it, join it, repair it, make it searchable.
    "pdftotext",
    "pdftoppm",
    "pdftocairo",
    "pdfimages",
    "pdfinfo",
    "pdfseparate",
    "pdfunite",
    "qpdf",
    "ocrmypdf",
    "tesseract",
    // Pictures — scans, logos, figures pulled out of a report.
    "magick",
    "convert",
    "identify",
    "mogrify",
    // Spreadsheets treated as data.
    "in2csv",
    "csvclean",
    "csvcut",
    "csvformat",
    "csvgrep",
    "csvjoin",
    "csvjson",
    "csvlook",
    "csvsort",
    "csvstack",
    "csvstat",
    // The language the document libraries live in (pandas, openpyxl,
    // python-docx, python-pptx, pypdf, Pillow — all preinstalled).
    "python3",
    // Reading and reshaping text.
    "cat",
    "head",
    "tail",
    "wc",
    "sort",
    "uniq",
    "cut",
    "paste",
    "tr",
    "sed",
    "awk",
    "grep",
    "rg",
    "diff",
    "comm",
    "join",
    "split",
    "fold",
    "nl",
    "seq",
    "tee",
    "iconv",
    "file",
    "find",
    "basename",
    "dirname",
    "realpath",
    // Filing: moving documents about inside the folder.
    "ls",
    "cp",
    "mv",
    "rm",
    "mkdir",
    "rmdir",
    "touch",
    "du",
    "stat",
    "date",
    // A mail merge arrives as a zip often enough to matter.
    "zip",
    "unzip",
    "tar",
    "gzip",
    "gunzip",
];

/// The guest's disposable working space: the tmpfs `ral-daemon` mounts at
/// `/tmp` (its mount plan, beside the `/work` mount itself), born with the
/// machine and gone when it stops.  The engine's own scratch lands under
/// it, and the prompt names it as the place for intermediate files, so the
/// grant admits it whole.
pub(crate) const GUEST_SCRATCH: &str = "/tmp";

/// One folder, opened as a session's whole authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The granted folder: absolute, symlink-resolved, known to exist
    /// and to be a readable directory at the moment of opening.
    root: PathBuf,
}

impl Grant {
    /// Open `folder` as the session's grant.
    ///
    /// [`MachineSpec::resolve`](vm_manager::MachineSpec::resolve) asks
    /// three of these questions again at boot.  That is not duplication to
    /// delete: a grant must resolve its folder *before* a spec can name it,
    /// and the two ask at different moments — this one when the user
    /// pointed, in the user's own words; that one when resources are about
    /// to be committed, after however long the pointing took.  A folder can
    /// stop existing in between.
    ///
    /// # Errors
    /// Returns a sentence for the person who picked the folder — not a
    /// Rust error — when the folder is missing, is a file, cannot be
    /// read, or is so large a choice (`/`, the home folder, anything
    /// containing it) that it is far likelier to be a slip than an
    /// intention.  Every refusal says what to pick instead.
    pub fn open(folder: &Path) -> Result<Self, String> {
        let shown = folder.display();
        let root = std::fs::canonicalize(folder).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!(
                "There is nothing at {shown}. Check the name, \
                 or choose the folder again from the picker."
            ),
            std::io::ErrorKind::PermissionDenied => format!(
                "This computer will not let synod open {shown}. \
                 Ask your IT team for access to it, or choose a folder you can open yourself."
            ),
            _ => format!("Synod could not open {shown}: {e}. Please choose another folder."),
        })?;

        if !root.is_dir() {
            return Err(format!(
                "{shown} is a single file, not a folder. \
                 Choose the folder that holds it, and synod will work on everything inside."
            ));
        }

        // Readability is a separate question from existence: a folder can
        // sit on a share the user can see but not list.  Ask now, plainly,
        // rather than let the first turn fail halfway through the work.
        std::fs::read_dir(&root).map_err(|e| {
            format!(
                "Synod cannot see what is inside {} ({e}). \
                 Ask your IT team whether you have permission to open this folder.",
                root.display()
            )
        })?;

        Self::refuse_too_much(&root)?;
        Ok(Self { root })
    }

    /// Refuse a grant that is not a *folder for a job* but a whole
    /// territory: the disk, the home folder, or any folder containing
    /// the home folder.
    ///
    /// One law covers all three — a grant must not contain the user's
    /// home folder — with the disk root caught first, since the home
    /// folder may be unknown.  It is deliberately not a size or a
    /// depth test: a departmental share at `/Volumes/Registry/Admissions`
    /// is a perfectly ordinary grant and must stay one.
    fn refuse_too_much(root: &Path) -> Result<(), String> {
        if root.parent().is_none() {
            return Err(format!(
                "{} is the whole disk — every file on this computer. \
                 Choose the one folder that holds the documents for this job.",
                root.display()
            ));
        }

        let home = ral_core::path::home_from_env();
        let home = std::fs::canonicalize(&home).unwrap_or_else(|_| PathBuf::from(&home));
        if home.as_os_str().is_empty() {
            return Ok(());
        }
        if home == root {
            return Err(format!(
                "{} is your whole home folder — your entire computer's worth of files. \
                 Choose the one folder that holds the documents for this job, \
                 such as {}.",
                root.display(),
                home.join("Documents").join("Admissions").display()
            ));
        }
        if home.starts_with(root) {
            return Err(format!(
                "{} contains your home folder, and so nearly everything you own. \
                 Choose the one folder that holds the documents for this job.",
                root.display()
            ));
        }
        Ok(())
    }

    /// The granted folder, absolute and resolved.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder's name as the user sees it in Finder or Explorer — the
    /// word the prompt hands the model to speak, never the full path.
    /// Always the final component: [`Self::open`] refuses the disk root,
    /// the only canonical path without one.
    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// What to boot to hold this folder: the granted folder, writable,
    /// on a machine sized for the heaviest thing the toolbox does —
    /// converting a document with the office suite running headless.  That is
    /// [`MachineSpec::for_folder`](vm_manager::MachineSpec::for_folder)
    /// exactly, so synod takes it rather than restating it and drifting.
    ///
    /// The folder is granted writable; the guest mount enforces that.
    pub fn machine_spec(&self) -> vm_manager::MachineSpec {
        vm_manager::MachineSpec::for_folder(self.root.clone())
    }

    /// The capabilities a session over this grant runs under.
    ///
    /// The hardware boundary is the first lock — the guest can reach only
    /// the granted folder and the control socket — and this value is the
    /// second: every claim below is enforced by ral's own gate inside the
    /// guest, on top of the wall around it.  Because the gate runs *there*,
    /// the paths are guest paths: the folder at its mount point, never the
    /// host path the user picked, which names nothing inside the machine.
    ///
    /// - **fs** — the granted folder at
    ///   [`MachineSpec::GUEST_WORKSPACE`](vm_manager::MachineSpec::GUEST_WORKSPACE)
    ///   and the guest's [`GUEST_SCRATCH`] tmpfs, read and write.
    ///   Nothing else: not the guest's own rootfs, and no host path at
    ///   all.  Where exarch's profiles read a developer's whole tool
    ///   configuration so `git` and `cargo` behave, an office session has
    ///   no configuration to find; every byte it needs is in the folder it
    ///   was handed.
    /// - **net** — off, flatly.  §6 gives the guest no network device at
    ///   all, so this is the policy agreeing with the topology rather
    ///   than substituting for it.  The web arrives, when it arrives, as
    ///   the host's `fetch-url` verb; not in v1.
    /// - **exec** — the toolbox of §7 and nothing else.  See [`TOOLBOX`]
    ///   for why it is a list of names rather than of directories.
    /// - **editor / shell** — the ral editor is off in every mode: this
    ///   session has no terminal and no one at it who wants one.  `chdir`
    ///   stays on, because moving between subfolders is what filing is.
    ///
    /// `deny_paths` is empty, and that is a decision rather than an
    /// omission.  exarch carves holes inside its own grant because the
    /// grant overlaps things the agent must not reach — its own profile
    /// file, credential directories under a wholesale-readable config
    /// root.  Synod's grant overlaps nothing: it is the user's documents,
    /// all of which the user handed over on purpose.  Carving a subtree
    /// back out would make the grant say something other than what the
    /// user was shown, and the real control on *changes* is §4's safety
    /// net — checkpoint, report, undo — not a hidden read barrier.
    ///
    /// `audit` is on: synod's entire product is a review surface, and a
    /// reported change is easier to trust beside the record of what the
    /// agent was allowed to do while producing it.
    pub fn capabilities(&self) -> Capabilities {
        let prefixes = || {
            vec![
                NormalizedPrefix::from_surface(vm_manager::MachineSpec::GUEST_WORKSPACE),
                NormalizedPrefix::from_surface(GUEST_SCRATCH),
            ]
        };
        Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: prefixes(),
                write_prefixes: prefixes(),
                deny_paths: Vec::new(),
            }),
            net: Some(false),
            exec: Some(ExecMap {
                literals: TOOLBOX
                    .iter()
                    .map(|name| ((*name).to_string(), ExecPolicy::Allow))
                    .collect::<BTreeMap<_, _>>(),
                dirs: BTreeMap::new(),
            }),
            editor: Some(EditorPolicy::default()),
            shell: Some(ShellPolicy { chdir: true }),
            audit: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::types::Shell;

    /// A private directory for one test, named after it so a failed run
    /// leaves readable wreckage.
    fn workshop(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("synod-grant-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp workshop");
        std::fs::canonicalize(&dir).expect("temp workshop resolves")
    }

    fn refusal(folder: &Path) -> String {
        Grant::open(folder).expect_err("this folder must be refused")
    }

    #[test]
    fn a_missing_folder_is_refused_by_name() {
        let dir = workshop("missing");
        let message = refusal(&dir.join("Admissions"));
        assert!(
            message.contains("There is nothing at") && message.contains("picker"),
            "a missing folder should say so and say what to do: {message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_is_not_a_folder() {
        let dir = workshop("file");
        let letter = dir.join("letter.docx");
        std::fs::write(&letter, b"not a folder").expect("write fixture");
        let message = refusal(&letter);
        assert!(
            message.contains("is a single file, not a folder"),
            "a file should be refused in the user's vocabulary: {message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The disk root is caught before the home rule, since the home
    /// folder may be unknown.  Unix-shaped: `/` is the root there.
    #[cfg(unix)]
    #[test]
    fn the_whole_disk_is_refused() {
        let message = refusal(Path::new("/"));
        assert!(
            message.contains("the whole disk"),
            "granting / should be refused as a slip: {message}"
        );
    }

    /// The home folder, and anything containing it, are one law: a grant
    /// must not contain the user's home.  Uses the real `$HOME` rather
    /// than a synthetic one, so no test has to mutate the environment.
    #[test]
    fn the_home_folder_and_its_parents_are_refused() {
        let home = ral_core::path::home_from_env();
        let Ok(home) = std::fs::canonicalize(&home) else {
            return; // No usable home on this host; nothing to assert.
        };
        assert!(
            refusal(&home).contains("your whole home folder"),
            "granting $HOME should be refused"
        );
        if let Some(parent) = home.parent().filter(|p| p.parent().is_some()) {
            assert!(
                refusal(parent).contains("contains your home folder"),
                "granting a folder above $HOME should be refused"
            );
        }
    }

    /// An ordinary folder opens, and `root` is the resolved form — the
    /// grant must not depend on how the picker spelled the path.
    #[test]
    fn an_ordinary_folder_opens_resolved() {
        let dir = workshop("ordinary");
        let admissions = dir.join("Admissions");
        std::fs::create_dir(&admissions).expect("fixture folder");
        let grant = Grant::open(
            &dir.join("Admissions")
                .join(".")
                .join("..")
                .join("Admissions"),
        )
        .expect("an ordinary folder must open");
        assert_eq!(grant.root(), admissions);
        assert_eq!(grant.name(), "Admissions");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fixture: a granted folder under a private workshop, so paths
    /// beside it are host paths *outside* the grant.
    fn granted(tag: &str) -> (PathBuf, Grant) {
        let dir = workshop(tag);
        let root = dir.join("Admissions");
        std::fs::create_dir(&root).expect("granted folder");
        let grant = Grant::open(&root).expect("fixture folder must open");
        (dir, grant)
    }

    /// The point of the whole file: the value admits the guest namespace —
    /// the mount point and the guest scratch — and denies the host one,
    /// the granted folder's own host path included.  Judged by ral's own
    /// point-of-use gate rather than by reading the struct's fields.
    #[test]
    fn the_policy_admits_the_guest_namespace_and_denies_the_host_one() {
        let (dir, grant) = granted("fs");
        let caps = grant.capabilities();

        let mut shell = Shell::default();
        shell.with_capabilities(caps, |sh| {
            for admitted in ["/work/letter.docx", "/tmp/draft.docx"] {
                let path = sh.resolve(admitted);
                sh.check_fs_read(&path)
                    .unwrap_or_else(|_| panic!("{admitted} must be readable"));
                let path = sh.resolve(admitted);
                sh.check_fs_write(&path)
                    .unwrap_or_else(|_| panic!("{admitted} must be writable"));
            }
            // The folder's host path names nothing inside the guest; a
            // grant that admitted it would strand the agent in exactly
            // the namespace confusion the mount exists to end.
            let host = grant.root().join("letter.docx");
            let path = sh.resolve(&host.to_string_lossy());
            assert!(
                sh.check_fs_read(&path).is_err(),
                "the granted folder's host path must not be readable"
            );
            let path = sh.resolve(&host.to_string_lossy());
            assert!(
                sh.check_fs_write(&path).is_err(),
                "the granted folder's host path must not be writable"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The user's home is outside the grant even when the grant is a
    /// folder inside it — the case §8's "topology as policy" is about.
    #[test]
    fn the_home_folder_is_unreachable_from_inside_a_grant() {
        let (dir, grant) = granted("home-unreachable");
        let home = ral_core::path::home_from_env();
        if home.is_empty() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let mut shell = Shell::default();
        shell.with_capabilities(grant.capabilities(), |sh| {
            let path = sh.resolve(&format!("{home}/.ssh/id_ed25519"));
            assert!(
                sh.check_fs_read(&path).is_err(),
                "the office grant must not reach into $HOME"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_office_toolbox_is_admitted_and_the_developer_one_is_not() {
        let (dir, grant) = granted("exec");
        let caps = grant.capabilities();
        let mut shell = Shell::default();
        shell.with_capabilities(caps, |sh| {
            for tool in ["pandoc", "soffice", "python3", "qpdf", "csvcut"] {
                sh.check_exec_args(tool, &[tool], &[])
                    .unwrap_or_else(|_| panic!("the office toolbox must admit {tool}"));
            }
            // §7 cuts these on purpose: they exist to install software,
            // and a network-less guest has nothing to install from.
            for cut in ["cc", "gcc", "make", "cargo", "sh", "bash", "apt"] {
                let path = format!("/usr/bin/{cut}");
                assert!(
                    sh.check_exec_args(cut, &[cut, &path], &[]).is_err(),
                    "the office grant must not admit {cut}"
                );
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_guest_has_no_network() {
        let (dir, grant) = granted("net");
        assert_eq!(
            grant.capabilities().net,
            Some(false),
            "SYNOD.md §6: the guest has no network at all"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_machine_holds_exactly_the_granted_folder() {
        let (dir, grant) = granted("machine");
        let spec = grant.machine_spec();
        assert_eq!(spec.workspace.host_path, grant.root());
        assert_eq!(
            spec.workspace.guest_path,
            Path::new(vm_manager::MachineSpec::GUEST_WORKSPACE)
        );
        assert!(
            !spec.workspace.read_only,
            "the agent works in the folder; the accept gate is what makes changes real"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
