//! Authority is derived from one resolved host folder; capabilities name guest paths only.

use crate::site::{Silent, site};
use ral_core::authority::grant::freeze::Place;
use ral_core::authority::grant::value::{Axes, ExecTable, FsTable, GrantValue, Target};
use ral_core::authority::policy::{Gpu, Ungoverned};
use ral_core::path::{LeafMode, RealPath, shown, spelled, walk_dir, walk_lenient};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Bare names keep the toolbox independent of additions to image directories.
const TOOLBOX: &[&str] = &[
    "soffice",
    "libreoffice",
    "pandoc",
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
    "magick",
    "convert",
    "identify",
    "mogrify",
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
    "python3",
    "pip3",
    "curl",
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
    "zip",
    "unzip",
    "tar",
    "gzip",
    "gunzip",
];

#[derive(Debug)]
pub(crate) struct Grant {
    /// Absolute, symlink-resolved and readable when opened.
    root: RealPath,
}

impl Grant {
    /// Resolve at selection; the machine checks again at boot because the folder may disappear.
    pub(crate) fn open(folder: &Path) -> Result<Self, String> {
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

        // Before a grant exists, test the user's own permission to list the folder.
        site!(Silent::GrantReadability, Ungoverned::read(dir))
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

    /// Refuse home, its ancestors and settings trees; only strict temp descendants are exempt.
    fn refuse_too_much(root: &RealPath, territory: &Territory) -> Result<(), String> {
        if root.is_root() {
            return Err(format!(
                "{root} is the whole disk: every file on this computer. \
                 Choose the one folder that holds the documents for this job."
            ));
        }

        let Some(home) = territory.home.as_ref() else {
            return Ok(());
        };
        if root.contains_folded(home) {
            return Err(if home.contains_folded(root) {
                format!(
                    "{} is your whole home folder: your entire computer's worth of files. \
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

    pub(crate) fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// The final component exists: open refuses the disk root.
    pub(crate) fn name(&self) -> String {
        self.root
            .as_path()
            .file_name()
            .map_or_else(|| self.root.to_string(), ral_core::path::shown)
    }

    /// The guest's ceiling, a grant value the guest resolves in its own
    /// namespace: the folder and the scratch, the office toolbox, no editor
    /// and no GPU.  `shell` is omitted: the guest VM bounds every survivor a
    /// detach could birth.
    #[allow(
        clippy::unused_self,
        reason = "a ceiling requires an admitted grant even though guest paths are fixed"
    )]
    pub(crate) fn ceiling(&self) -> GrantValue {
        let places = || {
            [guest_abi::layout::WORK, guest_abi::layout::SCRATCH]
                .map(|p| Place::Absolute(p.to_owned()))
                .to_vec()
        };
        Axes {
            exec: Some(ExecTable {
                allow: (TOOLBOX.iter())
                    .map(|name| Target::Name((*name).to_owned()))
                    .collect(),
                deny: Vec::new(),
            }),
            fs: Some(FsTable {
                read: places(),
                write: places(),
                deny: Vec::new(),
            }),
            gpu: Some(Gpu::Withheld),
            editor: Some(BTreeSet::new()),
            ..Axes::default()
        }
        .into()
    }
}

#[derive(Debug, Default)]
struct Territory {
    home: Option<RealPath>,
    settings: Vec<RealPath>,
    /// Strict descendants are allowed even when temp lies in a settings tree.
    scratch: Option<RealPath>,
}

impl Territory {
    fn of_this_host() -> Result<Self, String> {
        let home = site!(Silent::HostTerritory, ral_core::host::home()).map(PathBuf::from);
        Self::of(
            home.as_deref(),
            ral_core::host::var_os,
            &ral_core::host::temp_dir(),
        )
    }

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

/// Walk leniently to protect absent settings trees; lookup failure refuses the grant.
fn position(path: &Path) -> Result<RealPath, String> {
    walk_lenient(&spelled(path)?, LeafMode::Follow)
        .map(|trace| trace.real().clone())
        .map_err(|e| format!("Synod could not look up {}: {e}.", shown(path)))
}

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
#[allow(clippy::disallowed_methods, reason = "test code")]
#[allow(
    clippy::disallowed_macros,
    reason = "[test] libtest captures print macros"
)]
mod tests {
    use super::*;
    use crate::test_fixture::workshop;
    #[cfg(unix)]
    use ral_core::authority::policy::FsOp;
    use ral_core::data::FOValue;
    #[cfg(unix)]
    use ral_core::path::{LexicalPath, Reading};

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

    #[cfg(unix)]
    #[test]
    fn the_whole_disk_is_refused() {
        let message = refusal(Path::new("/"));
        assert!(
            message.contains("the whole disk"),
            "granting / should be refused as a slip: {message}"
        );
    }

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

    #[test]
    fn the_home_folder_and_whatever_contains_it_are_refused_for_different_reasons() {
        let fake = Fake::new();
        let parent = fake.home.parent().expect("the fixture home has a parent");
        let home = fake.judged(&fake.home).expect_err("home is refused");
        let above = fake.judged(parent).expect_err("what holds home is refused");
        assert_ne!(home, above);
    }

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
    fn this_hosts_startup_folder_is_refused() {
        let territory = Territory::of_this_host().expect("the host's territory resolves");
        let Some(roaming) = std::env::var_os("APPDATA").map(PathBuf::from) else {
            return;
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

    #[cfg(unix)]
    #[test]
    fn an_unlistable_folder_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp workshop");
        let locked = dir.path().join("Admissions");
        std::fs::create_dir(&locked).expect("fixture folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o311))
            .expect("lock the fixture");
        let binds = std::fs::read_dir(&locked).is_err();
        let opened = Grant::open(&locked);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock the fixture");
        if !binds {
            eprintln!("permissions do not bind here (running as root); skipping");
            return;
        }
        assert!(opened.is_err());
    }

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

    fn granted(tag: &str) -> (tempfile::TempDir, Grant) {
        let dir = workshop(tag);
        let root = dir.path().join("Admissions");
        std::fs::create_dir(&root).expect("granted folder");
        let grant = Grant::open(&root).expect("fixture folder must open");
        (dir, grant)
    }

    #[cfg(unix)]
    #[test]
    fn the_policy_admits_the_guest_namespace_and_denies_the_host_one() {
        let (_dir, grant) = granted("grant-fs");
        let mut shell = ral_core::test_helper::core_shell();
        let layer = (shell.decode_grant(grant.ceiling().as_ref(), "the ceiling"))
            .expect("the ceiling resolves");
        // Probe reads and test writes directly: locate would fail first on these absent paths.
        shell.with_layer(layer, |sh| {
            for admitted in ["/work/letter.docx", "/tmp/draft.docx"] {
                let path = sh.resolve(admitted);
                sh.locate_existing::<Reading>(&path, LeafMode::Follow)
                    .unwrap_or_else(|_| panic!("{admitted} must be readable"));
                assert!(writable(sh, &path), "{admitted} must be writable");
            }
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

    /// The payloads of a list of `label` tags, as the wire carries them.
    fn tagged<'a>(list: Option<&'a FOValue>, label: &str) -> Vec<&'a str> {
        let Some(FOValue::List { items }) = list else {
            panic!("a list of `{label}: {list:?}");
        };
        (items.iter())
            .map(|item| match ral_core::data::datum::untag(item) {
                Some((tag, Some(FOValue::String { value }))) if tag == label => value.as_str(),
                other => panic!("a `{label}: {other:?}"),
            })
            .collect()
    }

    /// Host-side guard simulation cannot detect a host/guest normalizer split;
    /// the ceiling crosses unfolded, for the guest to resolve, so assert the
    /// wire spelling.
    #[test]
    fn the_guest_prefixes_are_spelled_the_guests_way_on_every_host() {
        let (_dir, grant) = granted("grant-guest-spelling");
        let ceiling = FOValue::from(grant.ceiling());
        let fs = (ceiling.field("fs")).expect("the office grant restricts the filesystem");
        let guest = [guest_abi::layout::WORK, guest_abi::layout::SCRATCH];
        for which in ["read", "write"] {
            assert_eq!(
                tagged(fs.field(which), "absolute"),
                guest,
                "the fs {which} set must name the guest's paths as the guest spells them"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_office_toolbox_is_admitted_and_the_developer_one_is_not() {
        let (_dir, grant) = granted("grant-exec");
        let ceiling = FOValue::from(grant.ceiling());
        let exec = (ceiling.field("exec")).expect("the office grant restricts exec");
        let allowed = tagged(exec.field("allow"), "name");
        assert!(tagged(exec.field("deny"), "name").is_empty(), "{exec:?}");
        for tool in ["pandoc", "soffice", "python3", "qpdf", "csvcut"] {
            assert!(
                allowed.contains(&tool),
                "the office toolbox must admit {tool}"
            );
        }
        let cuts = ["cc", "gcc", "make", "cargo", "sh", "bash", "apt"];
        let tmp = tempfile::tempdir().unwrap();
        let bin =
            ral_core::test_helper::Tree::lay(tmp.path(), cuts.map(|cut| format!("/usr/bin/{cut}")));
        let mut shell = ral_core::test_helper::core_shell();
        let layer = (shell.decode_grant(&ceiling, "the ceiling")).expect("the ceiling resolves");
        shell.with_layer(layer, |sh| {
            for cut in cuts {
                let path = bin.at(&format!("/usr/bin/{cut}"));
                assert!(
                    !ral_core::test_access::admits_file(sh, &path),
                    "the office grant must not admit {cut}"
                );
            }
        });
    }
}
