//! The three addresses, on a real filesystem.

use super::*;
use crate::path::XdgKind;
use std::collections::BTreeMap;

/// A directory of the test's own, which the program keeps as it keeps its
/// state.
fn scratch(tag: &str) -> TestDir {
    OwnDir::for_test(tag).expect("a test directory is made")
}

/// What `pairs` hold, as the JSON value tests store.
fn value(pairs: &[(&str, u32)]) -> BTreeMap<String, u32> {
    pairs.iter().map(|&(k, v)| (k.to_owned(), v)).collect()
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::host::env::test_support::with_var;
    use crate::path;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    /// `p` as the text an environment variable holds.
    fn text(p: &Path) -> &str {
        path::utf8(p).expect("a test path is UTF-8")
    }

    /// The permission bits of `p`.
    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p)
            .expect("it exists")
            .permissions()
            .mode()
            & 0o777
    }

    /// `body`, run while `base` is the host's config base.
    fn under_config<R>(base: &Path, body: impl FnOnce() -> R) -> R {
        with_var("XDG_CONFIG_HOME", Some(text(base)), body)
    }

    #[test]
    fn a_link_at_a_name_is_refused_wherever_the_program_keeps_a_file() {
        let dir = scratch("linked");
        std::fs::write(dir.path().join("target"), "{}").unwrap();
        symlink(dir.path().join("target"), dir.path().join("l")).unwrap();
        assert!(matches!(dir.read("l"), Err(OwnError::Linked(_))));
        assert!(matches!(dir.open("l"), Err(OwnError::Linked(_))));
        assert!(matches!(dir.append("l"), Err(OwnError::Linked(_))));
        assert!(matches!(dir.open_write("l"), Err(OwnError::Linked(_))));
        assert!(matches!(dir.load::<u8>("l"), Err(OwnError::Linked(_))));
        assert!(matches!(
            dir.store("l", &1u8, Visibility::Public),
            Err(OwnError::Linked(_))
        ));
        assert_eq!(dir.stat("l").map(|s| s.kind), Some(Kind::Symlink));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("target")).unwrap(),
            "{}",
            "nothing was written through the link"
        );
    }

    #[test]
    fn a_link_to_a_directory_is_no_directory_of_the_programs_own() {
        let dir = scratch("linked-dir");
        std::fs::create_dir(dir.path().join("real")).unwrap();
        symlink(dir.path().join("real"), dir.path().join("l")).unwrap();
        assert!(matches!(dir.dir("l"), Err(OwnError::Linked(_))));
        assert!(matches!(dir.make_dir("l"), Err(OwnError::Linked(_))));
        let message = dir.dir("l").err().unwrap().to_string();
        assert!(message.contains("is a link"), "{message}");
    }

    #[test]
    fn remove_unlinks_a_link_and_never_follows_it() {
        let dir = scratch("remove");
        let outside = dir.make_dir("outside").unwrap();
        std::fs::write(outside.path().join("keep"), "kept").unwrap();
        let tree = dir.make_dir("tree").unwrap();
        std::fs::create_dir(tree.path().join("a")).unwrap();
        std::fs::write(tree.path().join("a").join("f"), "x").unwrap();
        symlink(outside.path(), tree.path().join("l")).unwrap();
        symlink(outside.path(), dir.path().join("top")).unwrap();

        dir.remove("tree").unwrap();
        dir.remove("top").unwrap();
        dir.remove("never-there").unwrap();

        assert!(dir.stat("tree").is_none());
        assert!(dir.stat("top").is_none());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("keep")).unwrap(),
            "kept",
            "what a link led to is not removed"
        );
    }

    #[test]
    fn secret_is_owner_only_over_a_looser_file_and_public_keeps_the_mode() {
        let dir = scratch("modes");
        let file = dir.path().join("f");
        std::fs::write(&file, "old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();

        dir.store("f", &value(&[("a", 1)]), Visibility::Public)
            .unwrap();
        assert_eq!(mode(&file), 0o640, "a replace keeps the mode");

        dir.store("f", &value(&[("a", 2)]), Visibility::Secret)
            .unwrap();
        assert_eq!(
            mode(&file),
            0o600,
            "a secret is owner-only whatever was there"
        );

        dir.store("s", &value(&[]), Visibility::Secret).unwrap();
        assert_eq!(mode(&dir.path().join("s")), 0o600);
    }

    #[test]
    fn a_directory_is_made_owner_only_and_made_again_idempotently() {
        let dir = scratch("make-dir");
        let made = dir.make_dir("x").unwrap();
        assert_eq!(mode(made.path()), 0o700);
        assert_eq!(made.path(), dir.path().join("x"));
        let again = dir.make_dir("x").unwrap();
        assert_eq!(again.path(), made.path());
        assert_eq!(dir.stat("x").map(|s| s.kind), Some(Kind::Dir));
    }

    #[test]
    fn a_fresh_file_is_owner_only_under_private() {
        let dir = scratch("fresh-mode");
        let (name, _file) = dir.create_fresh("", ".lock", Visibility::Private).unwrap();
        assert_eq!(mode(&dir.path().join(name)), 0o600);
    }

    #[test]
    fn list_names_each_entry_in_order_with_its_kind() {
        let dir = scratch("list");
        std::fs::write(dir.path().join("b"), "").unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        symlink(dir.path().join("b"), dir.path().join("c")).unwrap();
        let listed: Vec<(String, Kind)> = dir
            .list()
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.stat.kind))
            .collect();
        assert_eq!(
            listed,
            [
                ("a".to_owned(), Kind::Dir),
                ("b".to_owned(), Kind::File),
                ("c".to_owned(), Kind::Symlink),
            ]
        );
    }

    #[test]
    fn within_follows_a_link_by_where_it_lands_and_refuses_one_that_leaves_the_root() {
        let root = scratch("within-link");
        let a = root.make_dir("a").unwrap();
        symlink(a.path(), root.path().join("l")).unwrap();
        let beyond = scratch("within-beyond");
        symlink(beyond.path(), root.path().join("out")).unwrap();

        let reached = root.within(&root.path().join("l")).unwrap();
        assert_eq!(
            reached.path(),
            a.path(),
            "reached by its own names, not through the link"
        );
        let left = root.within(&root.path().join("out")).err().unwrap();
        assert!(matches!(left, OwnError::Outside { .. }), "{left}");
    }

    #[test]
    fn within_finds_a_run_reached_through_a_link_above_the_root() {
        let base = scratch("within-above");
        let root = base.make_dir("real").unwrap().make_dir("root").unwrap();
        let deep = root.make_dir("a").unwrap();
        symlink(base.path().join("real"), base.path().join("alias")).unwrap();
        let aliased = base.path().join("alias").join("root");

        let itself = root.within(&aliased).unwrap();
        assert_eq!(itself.path(), root.path());
        let reached = root.within(&aliased.join("a")).unwrap();
        assert_eq!(reached.path(), deep.path());
    }

    #[test]
    fn roots_are_resolved_once_for_each_base() {
        let (first, second) = (scratch("roots-a"), scratch("roots-b"));

        let (a, steps, again) = with_var("XDG_STATE_HOME", Some(text(first.path())), || {
            let a = OwnDir::xdg("own-test-roots", XdgKind::State).unwrap();
            let steps = path::walk::steps_taken();
            let again = OwnDir::xdg("own-test-roots", XdgKind::State).unwrap();
            (a, steps, again)
        });
        assert_eq!(a.path(), first.path().join("own-test-roots"));
        assert_eq!(
            path::walk::steps_taken(),
            steps,
            "a second call opens nothing"
        );
        assert!(Arc::ptr_eq(&a.0, &again.0), "it is the first call's handle");

        let moved = with_var("XDG_STATE_HOME", Some(text(second.path())), || {
            OwnDir::xdg("own-test-roots", XdgKind::State).unwrap()
        });
        assert_eq!(moved.path(), second.path().join("own-test-roots"));
        assert!(
            !Arc::ptr_eq(&a.0, &moved.0),
            "a host whose base has changed resolves a new root"
        );
    }

    #[test]
    fn a_root_linked_to_nothing_is_dangling() {
        let base = scratch("root-dangling");
        let missing = base.path().join("elsewhere").join("exarch");
        symlink(&missing, base.path().join("own-test-dangling")).unwrap();
        let err = with_var("XDG_STATE_HOME", Some(text(base.path())), || {
            OwnDir::xdg("own-test-dangling", XdgKind::State).err()
        });
        match err {
            Some(OwnError::Dangling { at, to }) => {
                assert_eq!(at, base.path().join("own-test-dangling"));
                assert_eq!(to, missing);
            }
            other => panic!("expected a dangling link, got {other:?}"),
        }
    }

    #[test]
    fn replace_goes_through_a_file_link_keeping_the_link_and_the_mode() {
        let home = scratch("replace-link");
        let (config, dots) = (home.path().join("config"), home.path().join("dots"));
        std::fs::create_dir_all(config.join("app")).unwrap();
        std::fs::create_dir(&dots).unwrap();
        std::fs::write(dots.join("rc"), "old").unwrap();
        std::fs::set_permissions(dots.join("rc"), std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink(dots.join("rc"), config.join("app").join("rc")).unwrap();

        under_config(&config, || {
            let rc = UserFile::xdg("app", XdgKind::Config, "rc").unwrap();
            assert_eq!(rc.read().unwrap().as_deref(), Some("old"));
            rc.replace("new").unwrap();
            assert_eq!(rc.read().unwrap().as_deref(), Some("new"));
        });

        let link = config.join("app").join("rc");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), dots.join("rc"));
        assert_eq!(std::fs::read_to_string(dots.join("rc")).unwrap(), "new");
        assert_eq!(mode(&dots.join("rc")), 0o640);
        let beside: Vec<_> = std::fs::read_dir(&dots)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(beside, ["rc"], "the staged sibling is gone");
    }

    /// A file the link reaches that the user may not write is not replaced,
    /// though its directory is writable, and the error says where to change it.
    #[test]
    fn replace_refuses_a_linked_file_the_user_may_not_write() {
        if rustix::process::geteuid().is_root() {
            eprintln!("skip: root writes every file");
            return;
        }
        let home = scratch("replace-read-only");
        let (config, store) = (home.path().join("config"), home.path().join("store"));
        std::fs::create_dir_all(config.join("app")).unwrap();
        std::fs::create_dir(&store).unwrap();
        std::fs::write(store.join("rc"), "generated").unwrap();
        std::fs::set_permissions(store.join("rc"), std::fs::Permissions::from_mode(0o444)).unwrap();
        symlink(store.join("rc"), config.join("app").join("rc")).unwrap();

        let refused = under_config(&config, || {
            let rc = UserFile::xdg("app", XdgKind::Config, "rc").unwrap();
            rc.replace("new").unwrap_err().to_string()
        });
        assert!(
            refused.contains("change it where it is generated"),
            "{refused}"
        );
        assert_eq!(
            std::fs::read_to_string(store.join("rc")).unwrap(),
            "generated"
        );
    }

    #[test]
    fn replace_makes_the_directory_and_the_file_where_absent() {
        let home = scratch("replace-new");
        let config = home.path().join("config");
        under_config(&config, || {
            let rc = UserFile::xdg("app", XdgKind::Config, "config.ral").unwrap();
            assert_eq!(rc.read().unwrap(), None);
            rc.replace("made").unwrap();
            assert_eq!(rc.read().unwrap().as_deref(), Some("made"));
        });
        assert_eq!(mode(&config.join("app")), 0o700);
    }

    #[test]
    fn replace_makes_the_file_a_dangling_link_names_and_refuses_a_missing_folder() {
        let home = scratch("replace-dangling");
        let (config, dots) = (home.path().join("config"), home.path().join("dots"));
        std::fs::create_dir_all(config.join("app")).unwrap();
        std::fs::create_dir(&dots).unwrap();
        symlink(dots.join("rc"), config.join("app").join("rc")).unwrap();
        let gone = home.path().join("gone").join("rc2");
        symlink(&gone, config.join("app").join("rc2")).unwrap();

        under_config(&config, || {
            let rc = UserFile::xdg("app", XdgKind::Config, "rc").unwrap();
            assert!(matches!(rc.read(), Err(OwnError::Dangling { .. })));
            rc.replace("made").unwrap();
            assert_eq!(rc.read().unwrap().as_deref(), Some("made"));

            let rc2 = UserFile::xdg("app", XdgKind::Config, "rc2").unwrap();
            match rc2.read() {
                Err(OwnError::Dangling { to, .. }) => assert_eq!(to, gone),
                other => panic!("expected a dangling link, got {other:?}"),
            }
            assert!(matches!(rc2.replace("x"), Err(OwnError::Dangling { .. })));
        });

        assert!(
            std::fs::symlink_metadata(config.join("app").join("rc"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(dots.join("rc")).unwrap(), "made");
    }

    #[test]
    fn seed_writes_nothing_over_a_link_dangling_or_not() {
        let home = scratch("seed");
        let config = home.path().join("config");
        let app = config.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(home.path().join("real"), "theirs").unwrap();
        symlink(home.path().join("real"), app.join("live")).unwrap();
        symlink(home.path().join("nowhere"), app.join("dangling")).unwrap();

        under_config(&config, || {
            let absent = UserFile::xdg("app", XdgKind::Config, "absent").unwrap();
            assert!(absent.seed("first").unwrap());
            assert!(!absent.seed("second").unwrap());
            assert_eq!(absent.read().unwrap().as_deref(), Some("first"));

            let live = UserFile::xdg("app", XdgKind::Config, "live").unwrap();
            assert!(!live.seed("mine").unwrap());
            assert_eq!(live.read().unwrap().as_deref(), Some("theirs"));

            let dangling = UserFile::xdg("app", XdgKind::Config, "dangling").unwrap();
            assert!(!dangling.seed("mine").unwrap());
        });

        assert!(
            std::fs::symlink_metadata(home.path().join("nowhere")).is_err(),
            "a dangling link is the user's: its target is not made"
        );
    }

    #[test]
    fn a_name_the_user_gives_is_never_made_over_anything_there() {
        let dir = scratch("named-create");
        let made = dir.path().join("out.txt");
        let mut file = create_named(&made).unwrap();
        file.write_all(b"exported").unwrap();
        drop(file);
        assert_eq!(read_named(&made).unwrap(), "exported");

        let again = create_named(&made).err().unwrap();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert!(again.to_string().contains("never overwritten"), "{again}");

        symlink(dir.path().join("nowhere"), dir.path().join("l")).unwrap();
        let linked = create_named(&dir.path().join("l")).err().unwrap();
        assert_eq!(linked.kind(), io::ErrorKind::AlreadyExists);
        assert!(
            !dir.path().join("nowhere").exists(),
            "nothing was made through the link"
        );
    }

    #[test]
    fn a_name_the_user_gives_is_followed_through_its_links() {
        let dir = scratch("named-read");
        std::fs::write(dir.path().join("f"), "text").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        symlink(dir.path().join("f"), dir.path().join("lf")).unwrap();
        symlink(dir.path().join("d"), dir.path().join("ld")).unwrap();
        assert_eq!(read_named(&dir.path().join("lf")).unwrap(), "text");
        assert_eq!(kind_named(&dir.path().join("lf")), Some(Kind::File));
        assert_eq!(kind_named(&dir.path().join("ld")), Some(Kind::Dir));
        assert_eq!(kind_named(&dir.path().join("none")), None);
        assert!(open_named(&dir.path().join("none")).is_err());
    }

    #[test]
    fn a_name_the_user_gives_that_is_a_link_to_nothing_is_dangling() {
        let dir = scratch("named-dangling");
        let (link, gone) = (dir.path().join("l"), dir.path().join("gone"));
        symlink(&gone, &link).unwrap();
        for error in [
            read_named(&link).err().unwrap(),
            open_named(&link).err().unwrap(),
        ] {
            match error {
                OwnError::Dangling { at, to } => assert_eq!((at, to), (link.clone(), gone.clone())),
                other => panic!("expected a dangling link, got {other:?}"),
            }
        }
        assert_eq!(kind_named(&link), None);
        assert_eq!(
            read_named(&gone).err().unwrap().kind(),
            io::ErrorKind::NotFound
        );
    }
}

#[test]
fn store_replaces_whole_and_load_reads_it_back() {
    let dir = scratch("store");
    assert_eq!(
        dir.load::<BTreeMap<String, u32>>("state.json").unwrap(),
        None
    );

    dir.store("state.json", &value(&[("a", 1)]), Visibility::Public)
        .unwrap();
    dir.store("state.json", &value(&[("b", 2)]), Visibility::Public)
        .unwrap();
    assert_eq!(
        dir.load::<BTreeMap<String, u32>>("state.json").unwrap(),
        Some(value(&[("b", 2)]))
    );
    let names: Vec<String> = dir.list().unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["state.json"], "no staged sibling is left");
}

#[test]
fn load_reports_what_is_not_the_value_and_none_only_for_nothing() {
    let dir = scratch("load");
    assert!(matches!(dir.load::<u32>("absent"), Ok(None)));

    dir.store("text", &"not a number", Visibility::Public)
        .unwrap();
    let wrong = dir.load::<u32>("text").err().unwrap();
    assert!(matches!(wrong, OwnError::Json { act: Act::Read, .. }));
    assert_eq!(wrong.kind(), io::ErrorKind::InvalidData);
    assert!(wrong.to_string().contains("as JSON"), "{wrong}");

    dir.make_dir("d").unwrap();
    assert!(
        dir.load::<u32>("d").is_err(),
        "a directory is no value, and no nothing"
    );
}

#[test]
fn append_lands_each_write_whole_at_the_end_whoever_else_appends() {
    let dir = scratch("append");
    let (mut a, mut b) = (dir.append("log").unwrap(), dir.append("log").unwrap());
    a.write_all(b"one\n").unwrap();
    b.write_all(b"two\n").unwrap();
    a.write_all(b"three\n").unwrap();
    assert_eq!(dir.read("log").unwrap(), b"one\ntwo\nthree\n");
}

#[test]
fn open_write_makes_where_absent_and_never_truncates() {
    let dir = scratch("open-write");
    let mut lock = dir.open_write("run.lock").unwrap();
    lock.write_all(b"abc").unwrap();
    drop(lock);
    let again = dir.open_write("run.lock").unwrap();
    assert_eq!(dir.read("run.lock").unwrap(), b"abc");
    again.set_len(1).unwrap();
    assert_eq!(dir.read("run.lock").unwrap(), b"a");
}

#[test]
fn open_reads_a_file_already_there_and_refuses_one_that_is_not() {
    let dir = scratch("open");
    let missing = dir.open("record.jsonl").err().unwrap();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    dir.store("record.jsonl", &1u8, Visibility::Public).unwrap();
    let mut text = String::new();
    dir.open("record.jsonl")
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "1");
}

#[test]
fn create_fresh_invents_a_name_and_makes_the_file() {
    let dir = scratch("fresh");
    let (name, mut file) = dir
        .create_fresh("frame-", ".json", Visibility::Public)
        .unwrap();
    let invented = name
        .strip_prefix("frame-")
        .and_then(|rest| rest.strip_suffix(".json"));
    assert!(
        invented.is_some_and(|s| s.len() == 16 && s.chars().all(|c| c.is_ascii_alphanumeric())),
        "{name}"
    );
    file.write_all(b"x").unwrap();
    assert_eq!(dir.read(&name).unwrap(), b"x");
    let (other, _) = dir
        .create_fresh("frame-", ".json", Visibility::Public)
        .unwrap();
    assert_ne!(name, other);
    assert!(dir.create_fresh("a/", "", Visibility::Public).is_err());
}

#[test]
fn rename_replaces_whatever_is_at_the_new_name() {
    let dir = scratch("rename");
    dir.store("a", &1u8, Visibility::Public).unwrap();
    dir.store("b", &2u8, Visibility::Public).unwrap();
    dir.rename("a", "b").unwrap();
    assert_eq!(dir.read("b").unwrap(), b"1");
    assert!(dir.stat("a").is_none());
    let missing = dir.rename("a", "c").err().unwrap();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
}

#[test]
fn a_name_is_one_entry() {
    let dir = scratch("names");
    for name in ["", ".", "..", "a/b", "../x"] {
        assert_eq!(
            dir.read(name).err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidInput),
            "{name:?}"
        );
    }
    assert!(dir.make_dir("a/b").is_err());
}

#[test]
fn a_directory_that_is_no_directory_is_not_one() {
    let dir = scratch("not-dir");
    dir.store("f", &1u8, Visibility::Public).unwrap();
    assert_eq!(
        dir.dir("f").err().map(|e| e.kind()),
        Some(io::ErrorKind::NotADirectory)
    );
    assert_eq!(
        dir.dir("absent").err().map(|e| e.kind()),
        Some(io::ErrorKind::NotFound)
    );
}

#[test]
fn within_reaches_a_descendant_by_its_names_and_refuses_the_rest() {
    let root = scratch("within");
    let deep = root.make_dir("a").unwrap().make_dir("b").unwrap();

    let reached = root.within(&root.path().join("a").join("b")).unwrap();
    assert_eq!(reached.path(), deep.path());
    let folded = root.within(&root.path().join("a/../a/b")).unwrap();
    assert_eq!(folded.path(), deep.path());
    let itself = root.within(root.path()).unwrap();
    assert_eq!(itself.path(), root.path());

    let outside = root.within(&crate::host::temp_dir()).err().unwrap();
    assert!(matches!(outside, OwnError::Outside { .. }), "{outside}");
    assert!(outside.to_string().contains("move it there"), "{outside}");
    let escaped = root.within(&root.path().join("a/../..")).err().unwrap();
    assert!(matches!(escaped, OwnError::Outside { .. }), "{escaped}");

    let absent = root
        .within(&root.path().join("a").join("nope"))
        .err()
        .unwrap();
    assert_eq!(absent.kind(), io::ErrorKind::NotFound);
}

#[test]
fn a_test_directory_is_removed_when_it_is_dropped() {
    let dir = scratch("dropped");
    let path = dir.path().to_path_buf();
    dir.make_dir("x").unwrap();
    drop(dir);
    assert!(!path.exists());
}

#[test]
fn the_temp_directory_is_held_and_not_removed_when_it_is_let_go() {
    let temp = OwnDir::temp_for_test().unwrap();
    drop(temp);
    assert!(crate::host::temp_dir().is_dir());
}

#[test]
fn an_error_says_what_to_do() {
    let state = OwnError::NoBase(XdgKind::State).to_string();
    assert!(
        state.contains("XDG_STATE_HOME") && state.contains("set HOME"),
        "{state}"
    );
    assert_eq!(
        OwnError::NoBase(XdgKind::State).kind(),
        io::ErrorKind::NotFound
    );

    let dangling = OwnError::Dangling {
        at: PathBuf::from("/h/.config/ral/rc"),
        to: PathBuf::from("/h/dotfiles/ral/rc"),
    };
    assert!(
        dangling.to_string().contains("is a link to")
            && dangling.to_string().contains("restore it"),
        "{dangling}"
    );

    let read_only = OwnError::Io {
        act: Act::Write,
        at: PathBuf::from("/h/.config/exarch/config.ral"),
        to: Some(PathBuf::from("/nix/store/x-config.ral")),
        error: io::ErrorKind::ReadOnlyFilesystem.into(),
    };
    let sentence = read_only.to_string();
    assert!(
        sentence.contains("it is a link to /nix/store/x-config.ral")
            && sentence.contains("change it where it is generated"),
        "{sentence}"
    );

    let plain = OwnError::Io {
        act: Act::Read,
        at: PathBuf::from("/s/x"),
        to: None,
        error: io::ErrorKind::PermissionDenied.into(),
    };
    assert!(
        plain.to_string().starts_with("could not read /s/x: "),
        "{plain}"
    );

    let as_io = io::Error::from(OwnError::Linked(PathBuf::from("/s/oauth.json")));
    assert_eq!(as_io.kind(), io::ErrorKind::InvalidInput);
    assert!(String::from(OwnError::Linked(PathBuf::from("/s/oauth.json"))).contains("is a link"));
}
