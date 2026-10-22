//! The folder grant — synod's whole notion of authority.
//!
//! A synod session begins when someone points at a folder.  Everything
//! downstream of that gesture is derived here and nowhere else: the
//! machine that will hold the folder ([`Grant::machine_spec`]) and the
//! capabilities the session runs under ([`Grant::capabilities`]).  There
//! is no second place where authority is widened, so reading this file
//! is reading the entire answer to "what can synod touch?".
//!
//! The answer is deliberately small.  The grant is *topology as policy* —
//! credentials, `HOME`, and the user's other folders are out of reach by
//! absence, not by a rule that could be argued with.  The network is the
//! one axis topology cannot settle alone: the guest has a wire, and what
//! it may say on it is a host-side allowlist, checked one layer out from
//! this module (`design/two-enforcers`).  This module is the typed
//! statement of the rest.
//!
//! ## Namespace
//!
//! [`Grant::root`] is a **host** path: the folder as the user picked it,
//! for everything that happens on this side of the wall — the mount, the
//! manifests, the window's own words.  The *engine* lives inside the
//! guest, where the folder appears at
//! [`MachineSpec::GUEST_WORKSPACE`](vm_manager::MachineSpec::GUEST_WORKSPACE)
//! and the working space is the guest's own [`GUEST_SCRATCH`] tmpfs, so
//! [`Grant::capabilities`] is minted over those guest paths: the value
//! rides every run into the guest and is enforced there, where a host
//! path names nothing.  One substitution, in one function, which is why
//! the construction lives here and is not scattered across the session.

use ral_core::capability::{Capabilities, Evidence, ExecKey, FsPolicy, Ungoverned};
use ral_core::path::{
    FrozenPath, GuestPath, LeafMode, LexicalPath, RealPath, Resolver, shown, walk_dir, walk_lenient,
};
use ral_core::types::EnvVars;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The image's office toolbox, as an allowlist of *bare command names*:
/// no path or dir key.
///
/// [`ExecGrant`](ral_core::capability::ExecGrant) admits by bare name, by path or by directory prefix,
/// and exarch's profiles lean hard on the directory half because a
/// developer's tool roots are open-ended: Homebrew, rustup toolchains,
/// nvm and pyenv install binaries nobody can enumerate in advance, so
/// the honest grant is "whatever lives under these roots".
///
/// Synod's toolbox is the opposite shape.  The image is the package
/// manager — anything installed mid-session is forgotten at the next
/// reboot, so what the agent may *rely* on is exactly the rootfs: fixed,
/// finite, and known before the user ever opens the app.  A directory
/// grant on `/usr/bin` would therefore hand the agent every package the
/// *next* image build happens to add, silently widening the grant each
/// time the rootfs is repackaged, and it would quietly re-admit exactly
/// the compilers and build systems the image cuts on purpose.  Naming
/// each tool keeps the image and the grant two independent reviews, and
/// leaves a policy a person can read start to finish.
///
/// Two things this list is not.  It is not a security boundary: `python3`
/// is on it, and a Python process can spawn whatever the image contains,
/// as can `find -exec`, so the map launders nothing.  The boundary is the
/// VM wall and the host process the guest's whole network runs in; the
/// map is a statement of the *job*, and a tripwire when the agent wanders
/// off it.  And it is not a shell: `sh`, `bash`, `env` and `xargs` are
/// absent because ral is the shell here — the agent loops and pipes in
/// ral, so a shell-out would buy nothing that ral does not already do.
/// That keeps the *common* path legible; it does not seal the map, and
/// nothing here pretends it does.
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
    // python-docx, python-pptx, pypdf, Pillow — all preinstalled), and its
    // package installer: a model-spawned `apt` fails outright on the fresh
    // UID every spawn runs under, so `pip3 install --user` is the one
    // install path the network's allowlist actually admits. `curl` rides
    // beside it for whatever `pip3` cannot reach directly.
    "python3",
    "pip3",
    "curl",
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
/// machine and gone when it stops.  It is the scratch the engine is
/// told, not the parent of one it mints; the prompt names it as the place for
/// intermediate files, so the grant admits it whole.
pub(crate) const GUEST_SCRATCH: GuestPath = GuestPath::from_static("/tmp");

/// One folder, opened as a session's whole authority.
#[derive(Debug)]
pub struct Grant {
    /// The granted folder: absolute, symlink-resolved, known to exist
    /// and to be a readable directory at the moment of opening.
    root: RealPath,
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
        let shown = shown(folder);
        let dir = walk_dir(&spelled(folder)?).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!(
                "There is nothing at {shown}. Check the name, \
                 or choose the folder again from the picker."
            ),
            std::io::ErrorKind::PermissionDenied => format!(
                "This computer will not let synod open {shown}. \
                 Ask whoever administers this computer for access to it, or choose a folder you can open yourself."
            ),
            std::io::ErrorKind::NotADirectory => format!(
                "{shown} is a single file, not a folder. \
                 Choose the folder that holds it, and synod will work on everything inside."
            ),
            _ => format!("Synod could not open {shown}: {e}. Please choose another folder."),
        })?;
        let root = dir.real().clone();

        // Readability is a separate question from existence: a folder can
        // sit on a share the user can see but not list.  Ask now, plainly,
        // rather than let the first exchange fail halfway through the work.
        // No grant exists yet: the question is the user's own permission,
        // which the handle answers.
        Ungoverned::read_dir(dir)
            .read_dir()
            .map_err(|e| {
                format!(
                    "Synod cannot see what is inside {root} ({e}). \
                     Ask whoever administers this computer whether you have permission to open this folder.",
                )
            })?;

        Self::refuse_too_much(&root, &Territory::of_this_host()?)?;
        Ok(Self { root })
    }

    /// Refuse a grant that is not a *folder for a job* but a whole
    /// territory — the disk, the home folder, or any folder containing
    /// the home folder — or that is a place where this user's programs
    /// keep their own settings.
    ///
    /// The first law is that a grant must not contain the user's home
    /// folder, with the disk root caught first, since the home folder may
    /// be unknown.  It is deliberately not a size or a depth test: a
    /// network share at `/Volumes/Registry/Admissions` is a perfectly
    /// ordinary grant and must stay one.
    ///
    /// The second law is about *where* inside home, not how much.  The
    /// folder is granted writable, and some folders inside home are read
    /// back by the computer itself: the Windows Startup folder under the
    /// roaming `AppData`, `~/Library/LaunchAgents` on macOS, and
    /// `~/.config/autostart` on Linux all run what they hold the next time
    /// the person signs in.  So a grant may neither lie inside one of
    /// [`Territory::settings`] nor contain one, and a hidden folder
    /// directly under home (`~/.ssh`, `~/.config`, …) is refused on the
    /// same ground.  The one carve-out is a folder strictly inside
    /// [`Territory::scratch`]: on Windows the temp folder lives under the
    /// local `AppData`, but it holds no settings and runs nothing at sign-in,
    /// and it is where a document opened from an email attachment lands.
    fn refuse_too_much(root: &RealPath, territory: &Territory) -> Result<(), String> {
        if root.is_root() {
            return Err(format!(
                "{root} is the whole disk — every file on this computer. \
                 Choose the one folder that holds the documents for this job."
            ));
        }

        let Some(home) = territory.home.as_ref() else {
            return Ok(()); // No home to contain: nothing for either law to say.
        };
        if root.contains_folded(home) {
            return Err(if home.contains_folded(root) {
                format!(
                    "{} is your whole home folder — your entire computer's worth of files. \
                     Choose the one folder that holds the documents for this job, \
                     such as {}.",
                    root,
                    shown(home.as_path().join("Documents").join("Admissions"))
                )
            } else {
                format!(
                    "{root} contains your home folder, and so nearly everything you own. \
                     Choose the one folder that holds the documents for this job."
                )
            });
        }

        let in_scratch = territory
            .scratch
            .as_ref()
            .is_some_and(|tmp| tmp.contains(root) && root != tmp);
        if in_scratch {
            return Ok(());
        }
        let settings_refusal = || {
            format!(
                "{} is where the programs on this computer keep their own settings, \
                 and some of what is kept there runs by itself the next time you sign in. \
                 Choose a folder of documents instead, such as one inside {}.",
                root,
                shown(home.as_path().join("Documents"))
            )
        };
        for tree in &territory.settings {
            if tree.contains_folded(root) {
                return Err(settings_refusal());
            }
            if root.contains_folded(tree) {
                return Err(format!(
                    "{root} holds {tree}, where the programs on this computer keep their own settings, \
                     and some of what is kept there runs by itself the next time you sign in. \
                     Choose the one folder inside it that holds the documents for this job."
                ));
            }
        }
        let hidden = (root.relative_to(home).unwrap_or_default().first())
            .is_some_and(|first| first.as_os_str().as_encoded_bytes().starts_with(b"."));
        if hidden {
            return Err(settings_refusal());
        }
        Ok(())
    }

    /// The granted folder, absolute and resolved.
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// The folder's name as the user sees it in Finder or Explorer — the
    /// word the prompt hands the model to speak, never the full path.
    /// Always the final component: [`Self::open`] refuses the disk root,
    /// the only canonical path without one.
    pub fn name(&self) -> String {
        self.root
            .as_path()
            .file_name()
            .map_or_else(|| self.root.to_string(), ral_core::path::shown)
    }

    /// What to boot to hold this folder: the granted folder, writable, on a
    /// machine sized for the heaviest thing the toolbox does — converting a
    /// document with the office suite running headless — possibly several
    /// times over.  A conversing assistant spreads a batch of documents
    /// across a few helpers, so "the heaviest thing" is several at once;
    /// synod builds its own spec rather than taking
    /// [`MachineSpec::for_folder`](vm_manager::MachineSpec::for_folder)'s 4
    /// vcpus / 4096 MiB, sized for three of that heaviest thing at once.
    /// Each spawn's own jail still caps a single command's memory well
    /// under that, so the sum stays honest even at the ceiling.
    ///
    /// The folder is granted writable; the guest mount enforces that.
    pub fn machine_spec(&self) -> vm_manager::MachineSpec {
        vm_manager::MachineSpec {
            memory_mib: 6144,
            ..vm_manager::MachineSpec::for_folder(self.root.as_path())
        }
    }

    /// The capabilities a session over this grant runs under.
    ///
    /// The hardware boundary is the first lock — the guest can reach only
    /// the granted folder and the control socket — and this value is the
    /// second: every claim below is enforced by ral's own in-process guard
    /// inside the guest, on top of the wall around it.  Because the guard runs
    /// *there*, the paths are guest paths: the folder at its mount point, never the
    /// host path the user picked, which names nothing inside the machine.
    /// They are also *spelled* there — minted through
    /// [`FrozenPath::from_guest`], because a host that writes its
    /// separator `\` must not be the one to write the guest's.
    ///
    /// - **fs** — the granted folder at
    ///   [`MachineSpec::GUEST_WORKSPACE`](vm_manager::MachineSpec::GUEST_WORKSPACE)
    ///   and the guest's [`GUEST_SCRATCH`] tmpfs, read and write.
    ///   Nothing else: not the guest's own rootfs, and no host path at
    ///   all.  Where exarch's profiles read a developer's whole tool
    ///   configuration so `git` and `cargo` behave, an office session has
    ///   no configuration to find; every byte it needs is in the folder it
    ///   was handed.
    /// - **net** — on.  The guest's `tun` has exactly one peer, a user-mode
    ///   TCP/IP stack in the host, which terminates every connection and
    ///   checks it against an allowlist before a byte crosses.  ral's `net`
    ///   is a flat boolean with no endpoint vocabulary of its own, so the
    ///   real narrowing is stated one layer out, in that host policy —
    ///   `design/two-enforcers` applied outward, not reproduced here.
    ///   `Some(false)` would strip the network from every command this grant
    ///   spawns (`core/src/sandbox/linux.rs`'s `bwrap --unshare-net`),
    ///   silently deleting that wire — this is a correctness bit, not prose.
    /// - **exec** — the image's toolbox and nothing else.  See [`TOOLBOX`]
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
    /// user was shown, and the real control on *changes* is the host-side
    /// account of them — the folder recorded before and after, and the
    /// difference reported — not a hidden read barrier.
    pub fn capabilities(&self) -> Capabilities {
        // Minted by the guest's rule, not this host's: `from_guest` folds
        // `/work` in the namespace the guard will match it in.  The ordinary
        // door would fold it with the *host's* kernel, which on Windows
        // rebuilds it as `\work` — and the agent would then be denied the
        // one folder it was given, by a grant that reads as if it had been
        // granted.  The same distinction `MachineSpec::resolve` draws when
        // it judges a guest path absolute with `starts_with('/')`.
        let prefixes = || {
            vec![
                FrozenPath::from_guest(&GuestPath::from_static(
                    vm_manager::MachineSpec::GUEST_WORKSPACE,
                )),
                FrozenPath::from_guest(&GUEST_SCRATCH),
            ]
        };
        Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: prefixes(),
                write_prefixes: prefixes(),
                deny_paths: Vec::new(),
            }),
            exec: Some(
                (TOOLBOX.iter())
                    .map(|name| (ExecKey::Name((*name).to_string()), Evidence::ALLOW))
                    .collect(),
            ),
            editor: Some(BTreeSet::new()),
            // `shell` is unattenuated: the guest VM already bounds every
            // survivor a detach could birth.
            ..Capabilities::default()
        }
    }
}

/// The parts of this user's computer a grant is judged against, each walked
/// once to the position the root is compared with.
///
/// Built from the real host by [`Territory::of_this_host`]; the tests build
/// their own, so no refusal test depends on the machine it runs on.
#[derive(Debug, Default)]
struct Territory {
    /// The user's home folder, if the host names one.
    home: Option<RealPath>,
    /// The per-user application-data trees: where programs keep their
    /// settings, and where the places that run things at sign-in live.
    /// See [`settings_trees`].
    settings: Vec<RealPath>,
    /// The system temp folder, strictly inside which a grant is allowed
    /// even when the temp folder itself lies in a settings tree.
    scratch: Option<RealPath>,
}

impl Territory {
    /// The territory of the user running synod.
    #[allow(
        clippy::disallowed_methods,
        reason = "host-env: the territory being protected is the launching user's real home and \
                  application-data folders, read from this process's own environment"
    )]
    fn of_this_host() -> Result<Self, String> {
        let home = ral_core::host::home().map(PathBuf::from);
        Self::of(
            home.as_deref(),
            |name| std::env::var_os(name),
            &std::env::temp_dir(),
        )
    }

    /// The territory of a user with this `home` and environment, whose temp
    /// folder is `temp`: each place walked, so one not created yet is still
    /// refused.
    fn of(
        home: Option<&Path>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
        temp: &Path,
    ) -> Result<Self, String> {
        Ok(Self {
            home: home.map(position).transpose()?,
            settings: settings_trees(home, env)
                .iter()
                .map(|p| position(p))
                .collect::<Result<_, _>>()?,
            scratch: Some(position(temp)?),
        })
    }
}

/// `path` as a spelling a walk takes: a relative one is read against the
/// process's working folder.
///
/// # Errors
/// A sentence for the person at the window when the process has no working
/// folder to read it against.
pub fn spelled(path: &Path) -> Result<LexicalPath, String> {
    let cwd = ral_core::host::cwd().ok_or_else(|| {
        format!(
            "Synod cannot tell which folder it is running in, so it cannot find {}.",
            shown(path)
        )
    })?;
    Ok(Resolver::at(&EnvVars::new(), &cwd).anchor(path))
}

/// Where `path` is, or will be once its missing tail is made: lenient, so a
/// settings tree not created yet is still refused.  Failing closed: a place
/// that cannot be looked up cannot be protected.
fn position(path: &Path) -> Result<RealPath, String> {
    walk_lenient(&spelled(path)?, LeafMode::Follow)
        .map(|trace| trace.real().clone())
        .map_err(|e| format!("Synod could not look up {}: {e}.", shown(path)))
}

/// The per-user application-data trees for this platform, from the home
/// folder and an environment lookup — pure, so a test can hand it both.
///
/// - **Windows** — the roaming and the local `AppData` folders as
///   `%APPDATA%` and `%LOCALAPPDATA%` name them (either may be redirected
///   elsewhere, to a network profile for instance), and `home\AppData`
///   besides, which is where both live when nothing redirects them and so
///   the fallback when the variables are unset.  A variable holding a
///   relative path names nothing and is ignored.
/// - **macOS** — `~/Library`, which holds `LaunchAgents`.
/// - **Linux and the other Unixes** — `~/.config`, which holds
///   `autostart`, and `~/.local`.  Every hidden folder directly under home
///   is refused besides, by [`Grant::refuse_too_much`]'s own rule.
fn settings_trees(
    home: Option<&Path>,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let mut trees = Vec::new();
    if cfg!(windows) {
        trees.extend(
            ["APPDATA", "LOCALAPPDATA"]
                .into_iter()
                .filter_map(|name| env(name).map(PathBuf::from))
                .filter(|p| ral_core::path::is_absolute(p)),
        );
        trees.extend(home.map(|h| h.join("AppData")));
    } else if cfg!(target_os = "macos") {
        trees.extend(home.map(|h| h.join("Library")));
    } else if let Some(h) = home {
        trees.extend([h.join(".config"), h.join(".local")]);
    }
    trees
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fixtures write and read scratch files; no model in \
              the loop"
)]
mod tests {
    use super::*;
    use crate::test_fixture::workshop;
    #[cfg(unix)]
    use ral_core::capability::FsOp;
    #[cfg(unix)]
    use ral_core::path::Reading;

    /// Whether `sh` admits a write at `path`, walked leniently as a name.
    #[cfg(unix)]
    fn writable(sh: &ral_core::Shell, path: &LexicalPath) -> bool {
        walk_lenient(path, LeafMode::Follow).is_ok_and(|trace| sh.admits_fs(&FsOp::Write, &trace))
    }

    fn refusal(folder: &Path) -> String {
        Grant::open(folder).expect_err("this folder must be refused")
    }

    #[test]
    fn a_missing_folder_is_refused_by_name() {
        let dir = workshop("grant-missing");
        let message = refusal(&dir.path().join("Admissions"));
        assert!(
            message.contains("There is nothing at") && message.contains("picker"),
            "a missing folder should say so and say what to do: {message}"
        );
    }

    #[test]
    fn a_file_is_not_a_folder() {
        let dir = workshop("grant-file");
        let letter = dir.path().join("letter.docx");
        std::fs::write(&letter, b"not a folder").expect("write fixture");
        let message = refusal(&letter);
        assert!(
            message.contains("is a single file, not a folder"),
            "a file should be refused in the user's vocabulary: {message}"
        );
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
    /// must not contain the user's home.  Uses the real `HOME` rather
    /// than a synthetic one, so no test has to mutate the environment.
    #[test]
    fn the_home_folder_and_its_parents_are_refused() {
        let Some(home) = ral_core::host::home().and_then(|h| std::fs::canonicalize(h).ok()) else {
            return; // No usable home on this host; nothing to assert.
        };
        assert!(
            refusal(&home).contains("your whole home folder"),
            "granting HOME should be refused"
        );
        if let Some(parent) = home.parent().filter(|p| p.parent().is_some()) {
            assert!(
                refusal(parent).contains("contains your home folder"),
                "granting a folder above HOME should be refused"
            );
        }
    }

    /// A made-up home, in a workshop of its own, so the territory tests judge
    /// no real machine.  Nothing is made under it: a lenient walk ends where
    /// the disk does.
    struct Fake {
        _dir: tempfile::TempDir,
        home: PathBuf,
    }

    impl Fake {
        fn new() -> Self {
            let dir = workshop("grant-territory");
            let home = dir.path().join("ada");
            Self { _dir: dir, home }
        }

        /// A territory over the home whose one settings tree is the roaming
        /// `AppData`, and whose temp folder lies under the local one.
        fn territory(&self) -> Territory {
            let app_data = self.home.join("AppData");
            let at = |path: PathBuf| position(&path).expect("a fixture resolves");
            Territory {
                settings: vec![at(app_data.join("Roaming")), at(app_data.join("Local"))],
                scratch: Some(at(app_data.join("Local").join("Temp"))),
                home: Some(at(self.home.clone())),
            }
        }

        fn judged(&self, root: &Path) -> Result<(), String> {
            let root = position(root).expect("a fixture resolves");
            Grant::refuse_too_much(&root, &self.territory())
        }
    }

    /// A settings tree not created yet is still refused: the territory is
    /// walked leniently, so a folder inside an absent `~/.config` (or the
    /// platform's tree) lies in it.
    #[test]
    fn a_folder_inside_an_absent_settings_tree_is_refused() {
        let dir = workshop("grant-absent-settings");
        let home = dir.path().join("ada");
        std::fs::create_dir(&home).expect("home fixture");
        let territory = Territory::of(Some(&home), |_| None, &dir.path().join("tmp"))
            .expect("the territory resolves");
        let tree = settings_trees(Some(&home), |_| None).remove(0);
        assert!(!tree.exists(), "the settings tree must be absent: {tree:?}");
        let root = position(&tree.join("autostart")).expect("an absent folder resolves");
        let message = Grant::refuse_too_much(&root, &territory)
            .expect_err("a folder inside an absent settings tree must be refused");
        assert!(
            message.contains("keep their own settings"),
            "the refusal should say why: {message}"
        );
    }

    /// The finding this law answers: the Startup folder runs what it holds
    /// at the next sign-in, so a writable grant there, or of any settings
    /// tree whole, is refused and says why.
    #[test]
    fn a_folder_inside_the_settings_trees_is_refused() {
        let fake = Fake::new();
        let roaming = fake.home.join("AppData").join("Roaming");
        let startup = roaming
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup");
        for root in [&startup, &roaming] {
            let message = fake
                .judged(root)
                .expect_err("a settings folder must be refused");
            assert!(
                message.contains("keep their own settings")
                    && message.contains("runs by itself the next time you sign in")
                    && message.contains("Documents"),
                "the refusal should say why and what to pick instead: {message}"
            );
        }
    }

    #[test]
    fn a_folder_holding_a_settings_tree_is_refused() {
        let fake = Fake::new();
        let message = fake
            .judged(&fake.home.join("AppData"))
            .expect_err("a folder over AppData must be refused");
        assert!(
            message.contains("holds") && message.contains("Roaming"),
            "the refusal should name the settings folder it holds: {message}"
        );
    }

    /// `~/.config/autostart`, `~/.ssh` and their kind: a hidden folder
    /// directly under home is refused, however deep the grant inside it.
    /// A hidden folder deeper down, inside an ordinary one, is just a
    /// folder.
    #[test]
    fn a_hidden_folder_under_home_is_refused() {
        let fake = Fake::new();
        let home = fake.home.clone();
        for root in [
            home.join(".config").join("autostart"),
            home.join(".ssh"),
            home.join(".local").join("share"),
        ] {
            let message = fake
                .judged(&root)
                .expect_err("a hidden home folder must be refused");
            assert!(
                message.contains("keep their own settings"),
                "{} should be refused as settings: {message}",
                ral_core::path::shown(&root)
            );
        }
        fake.judged(&home.join("Documents").join(".drafts"))
            .expect("a hidden folder inside an ordinary one is an ordinary grant");
        fake.judged(&home.join("Documents").join("Admissions"))
            .expect("an ordinary folder inside home is an ordinary grant");
    }

    /// The temp folder lies under the local `AppData` on Windows but runs
    /// nothing at sign-in: a folder inside it opens, the whole of it does
    /// not.
    #[test]
    fn a_folder_inside_the_temp_folder_is_allowed() {
        let fake = Fake::new();
        let temp = fake.home.join("AppData").join("Local").join("Temp");
        fake.judged(&temp.join("Admissions"))
            .expect("a folder inside temp must open");
        fake.judged(&temp)
            .expect_err("the whole temp folder is still a settings tree's");
    }

    #[cfg(windows)]
    #[test]
    fn the_settings_trees_on_windows_follow_the_environment() {
        let fake = Fake::new();
        let home = fake.home;
        let env = |name: &str| match name {
            "APPDATA" => Some(r"\\profiles\ada\Roaming".into()),
            "LOCALAPPDATA" => Some(r"Local".into()), // relative: names nothing
            _ => None,
        };
        assert_eq!(
            settings_trees(Some(&home), env),
            [
                PathBuf::from(r"\\profiles\ada\Roaming"),
                home.join("AppData")
            ]
        );
        assert_eq!(
            settings_trees(Some(&home), |_| None),
            [home.join("AppData")],
            "with nothing set, the fallback is home's own AppData"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_settings_tree_on_macos_is_the_library() {
        let fake = Fake::new();
        let home = fake.home.clone();
        assert_eq!(
            settings_trees(Some(&home), |_| None),
            [home.join("Library")]
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_settings_trees_on_linux_are_config_and_local() {
        let fake = Fake::new();
        let home = fake.home;
        assert_eq!(
            settings_trees(Some(&home), |_| None),
            [home.join(".config"), home.join(".local")]
        );
    }

    /// The real host, once: the Startup folder this machine would actually
    /// run things from is refused, and the temp folder the fixtures above
    /// work in is not.
    #[cfg(windows)]
    #[test]
    fn this_hosts_startup_folder_is_refused() {
        let territory = Territory::of_this_host().expect("the host's territory resolves");
        let Some(roaming) = std::env::var_os("APPDATA").map(PathBuf::from) else {
            return; // Nothing names a roaming AppData here; nothing to assert.
        };
        let startup = position(&roaming.join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
            .expect("the Startup folder resolves");
        Grant::refuse_too_much(&startup, &territory).expect_err("the real Startup folder");
        let (_dir, grant) = granted("grant-real-temp");
        Grant::refuse_too_much(
            &position(grant.root()).expect("the grant resolves"),
            &territory,
        )
        .expect("a folder inside temp");
    }

    /// A folder the user can see but not list is refused in the user's
    /// words, naming the folder and the reason.  Skipped where permissions
    /// do not bind the process: it lists anything.
    #[cfg(unix)]
    #[test]
    fn an_unlistable_folder_is_refused_with_the_listing_sentence() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workshop("grant-unlistable");
        let locked = dir.path().join("Admissions");
        std::fs::create_dir(&locked).expect("fixture folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o311))
            .expect("lock the fixture");
        let message = if std::fs::read_dir(&locked).is_ok() {
            None
        } else {
            Some(refusal(&locked))
        };
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock the fixture");
        if let Some(message) = message {
            assert!(
                message.contains("Synod cannot see what is inside")
                    && message.contains("Admissions")
                    && message.contains("whether you have permission"),
                "an unlistable folder should be refused with the listing sentence: {message}"
            );
        }
    }

    /// An ordinary folder opens, and `root` is the resolved form — the
    /// grant must not depend on how the picker spelled the path.
    #[test]
    fn an_ordinary_folder_opens_resolved() {
        let dir = workshop("grant-ordinary");
        let admissions = dir.path().join("Admissions");
        std::fs::create_dir(&admissions).expect("fixture folder");
        let grant = Grant::open(
            &dir.path()
                .join("Admissions")
                .join(".")
                .join("..")
                .join("Admissions"),
        )
        .expect("an ordinary folder must open");
        assert_eq!(grant.root(), admissions);
        assert_eq!(grant.name(), "Admissions");
    }

    /// Fixture: a granted folder under a private workshop, so paths
    /// beside it are host paths *outside* the grant.  The workshop comes back
    /// as its guard: hold it, or the folder the grant names goes away.
    fn granted(tag: &str) -> (tempfile::TempDir, Grant) {
        let dir = workshop(tag);
        let root = dir.path().join("Admissions");
        std::fs::create_dir(&root).expect("granted folder");
        let grant = Grant::open(&root).expect("fixture folder must open");
        (dir, grant)
    }

    /// The point of the whole file: the value admits the guest namespace —
    /// the mount point and the guest scratch — and denies the host one,
    /// the granted folder's own host path included.  Judged by ral's own
    /// in-process guard rather than by reading the struct's fields.  Unix
    /// hosts only: the guest resolves its own ceiling, and a host walks the
    /// guest's `/work` only where `/` is a root.
    #[cfg(unix)]
    #[test]
    fn the_policy_admits_the_guest_namespace_and_denies_the_host_one() {
        let (_dir, grant) = granted("grant-fs");
        let layer = ral_core::test_helper::layer(grant.capabilities());

        let mut shell = ral_core::test_helper::core_shell();
        // Read asks the probe door, write asks the stack directly: the write
        // door is `locate`, which walks the name first and would fail on
        // these paths for not existing, long before it reached the grant.
        shell.with_layer(layer, |sh| {
            for admitted in ["/work/letter.docx", "/tmp/draft.docx"] {
                let path = sh.resolve(admitted);
                sh.locate_existing::<Reading>(&path, LeafMode::Follow)
                    .unwrap_or_else(|_| panic!("{admitted} must be readable"));
                assert!(writable(sh, &path), "{admitted} must be writable");
            }
            // The folder's host path names nothing inside the guest; a
            // grant that admitted it would strand the agent in exactly
            // the namespace confusion the mount exists to end.
            let host = grant.root().join("letter.docx");
            let path = sh.anchor(&host);
            assert!(
                sh.locate_existing::<Reading>(&path, LeafMode::Follow)
                    .is_err(),
                "the granted folder's host path must not be readable"
            );
            assert!(
                !writable(sh, &path),
                "the granted folder's host path must not be writable"
            );
        });
    }

    /// The bytes the guest will be handed, which the test above cannot
    /// see and this one exists for.
    ///
    /// That test runs the grant through ral's guard *on the host*, so both
    /// sides of the comparison fold with the host's kernel and agree even
    /// when both are wrong: on Windows it passed while the shipped product
    /// denied `/work` on the first read, because the real access side is
    /// the engine inside the machine and it folds like Linux.  A host-side
    /// simulation of a guest-side guard cannot catch a host/guest
    /// normaliser split — only the spelling can, because the spelling is
    /// the whole of what crosses the wire.
    #[test]
    fn the_guest_prefixes_are_spelled_the_guests_way_on_every_host() {
        let (_dir, grant) = granted("grant-guest-spelling");
        let fs = grant
            .capabilities()
            .fs
            .expect("the office grant restricts the filesystem");
        let scratch = GUEST_SCRATCH;
        let guest = [vm_manager::MachineSpec::GUEST_WORKSPACE, scratch.as_str()];
        for (which, prefixes) in [("read", &fs.read_prefixes), ("write", &fs.write_prefixes)] {
            let spelled: Vec<&str> = prefixes.iter().map(FrozenPath::as_str).collect();
            assert_eq!(
                spelled, guest,
                "the fs {which} set must name the guest's paths as the guest spells them"
            );
        }
    }

    /// The user's home is outside the grant even when the grant is a
    /// folder inside it: unreachable by absence, not by a rule.  Unix hosts
    /// only, as the first of these.
    #[cfg(unix)]
    #[test]
    fn the_home_folder_is_unreachable_from_inside_a_grant() {
        let (_dir, grant) = granted("grant-home-unreachable");
        let Some(home) = ral_core::host::home() else {
            return;
        };
        let mut shell = ral_core::test_helper::core_shell();
        shell.with_layer(ral_core::test_helper::layer(grant.capabilities()), |sh| {
            let path = sh.anchor(&PathBuf::from(&home).join(".ssh").join("id_ed25519"));
            assert!(
                sh.locate_existing::<Reading>(&path, LeafMode::Follow)
                    .is_err(),
                "the office grant must not reach into HOME"
            );
        });
    }

    /// A bare name is the file the guest's `PATH` finds, so the toolbox is
    /// asserted as the names it lists; a developer tool is absent, and no
    /// file of its name is admitted.  Unix hosts only, as the first of these.
    #[cfg(unix)]
    #[test]
    fn the_office_toolbox_is_admitted_and_the_developer_one_is_not() {
        let (_dir, grant) = granted("grant-exec");
        let caps = grant.capabilities();
        let exec = caps.exec.as_ref().expect("the office grant restricts exec");
        for tool in ["pandoc", "soffice", "python3", "qpdf", "csvcut"] {
            assert_eq!(
                exec.get(&ExecKey::Name(tool.into())),
                Some(&Evidence::ALLOW),
                "the office toolbox must admit {tool}"
            );
        }
        let cuts = ["cc", "gcc", "make", "cargo", "sh", "bash", "apt"];
        let tmp = tempfile::tempdir().unwrap();
        let bin =
            ral_core::test_helper::Tree::lay(tmp.path(), cuts.map(|cut| format!("/usr/bin/{cut}")));
        let mut shell = ral_core::test_helper::core_shell();
        shell.with_layer(ral_core::test_helper::layer(caps), |sh| {
            // The image cuts these on purpose: what has to be built from
            // source on a machine that forgets it at reboot is a delay,
            // not a capability.
            for cut in cuts {
                let path = bin.at(&format!("/usr/bin/{cut}"));
                assert!(
                    !ral_core::test_access::admits_file(sh, &path),
                    "the office grant must not admit {cut}"
                );
            }
        });
    }

    #[test]
    fn the_guest_reaches_the_network_through_the_hosts_allowlist() {
        let (_dir, grant) = granted("grant-net");
        assert_eq!(
            grant.capabilities().net,
            None,
            "the guest has a wire; `core/src/sandbox/linux.rs` turns \
             `Offline` into `bwrap --unshare-net`, which would strip it back off"
        );
    }

    #[test]
    fn the_machine_holds_exactly_the_granted_folder() {
        let (_dir, grant) = granted("grant-machine");
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
    }

    /// Synod sizes its own machine rather than taking the folder default's
    /// 4096 MiB, now that a batch of documents may run across a few
    /// helpers rather than one office-suite conversion at a time.
    #[test]
    fn the_machine_is_sized_for_a_few_helpers_at_once() {
        let (_dir, grant) = granted("grant-machine-memory");
        assert_eq!(grant.machine_spec().memory_mib, 6144);
    }
}
