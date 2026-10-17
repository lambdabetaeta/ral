//! The six built-in capability bases, embedded from the `.ral` scripts in
//! `exarch/data/` and loaded through
//! [`load_capabilities_from_str`](ral_core::load::profile::load_capabilities_from_str),
//! the same surface the ral CLI's `--capabilities` flag consumes.
//!
//! Loosest to tightest: `dangerous` (the lattice top), `reasonable`,
//! `edit-only`, `read-only`, `minimal`, `confined`.  Every profile but
//! `dangerous` names `cwd` and `tempdir` places, which the loader freezes at
//! session start — so the per-invocation working directory is baked into the
//! policy and exarch never injects it dynamically.

use ral_core::capability::{Capabilities, ExecKey};
use ral_core::guard::freeze::FreezeCtx;
use ral_core::terminal::TerminalState;

const MINIMAL_RAL: &str = include_str!("../../data/minimal.exarch.ral");
const REASONABLE_RAL: &str = include_str!("../../data/reasonable.exarch.ral");
const READ_ONLY_RAL: &str = include_str!("../../data/read-only.exarch.ral");
const EDIT_ONLY_RAL: &str = include_str!("../../data/edit-only.exarch.ral");
const CONFINED_RAL: &str = include_str!("../../data/confined.exarch.ral");
const DANGEROUS_RAL: &str = include_str!("../../data/dangerous.exarch.ral");
// Gated with its only consumers, the Unix-shaped extension tests below.
#[cfg(all(test, unix))]
const GIT_EXTENSION_RAL: &str = include_str!("../../examples/git.exarch.ral");

/// Resolve `name` to a frozen [`Capabilities`], every sigil resolved against
/// `ctx`, so `super::for_invocation` composes on already-resolved bundles.
pub(super) fn resolve_base(name: &str, ctx: &FreezeCtx) -> Result<Capabilities, String> {
    let text = match name {
        "minimal" => MINIMAL_RAL,
        "reasonable" => REASONABLE_RAL,
        "read-only" => READ_ONLY_RAL,
        "edit-only" => EDIT_ONLY_RAL,
        "confined" => CONFINED_RAL,
        "dangerous" => DANGEROUS_RAL,
        other => {
            return Err(format!(
                "unknown base '{other}'; \
                 expected one of: dangerous, reasonable, edit-only, read-only, minimal, confined"
            ));
        }
    };
    let mut shell = ral_core::HostSurface::default().shell(TerminalState::default());
    let virtual_path = format!("<built-in:{name}>");
    let mut caps = ral_core::load::profile::load_capabilities_from_str(
        &ral_core::types::Mooring::adrift(),
        &mut shell,
        text,
        &virtual_path,
        ctx,
    )
    .map_err(|e| match e {
        ral_core::types::Break::Error(err) => {
            format!("built-in base '{name}' failed to parse: {}", err.message)
        }
        other @ ral_core::types::Break::Escape(_) => {
            format!("built-in base '{name}' failed: {other:?}")
        }
    })?;
    drop_dead_exec_grants(&mut caps, cfg!(unix));
    Ok(caps)
}

/// Withdraw the allowances of bare exec keys naming a coreutil this platform
/// cannot bundle (the `coreutils-unix-only` set, whose `uu_*` crates compile on
/// Unix alone), so a rendered profile never advertises a grant it cannot
/// honour.  Exclusions stay: a veto does not depend on the tool existing.
///
/// `unix_available` is a parameter rather than a `cfg!(unix)` read here so the
/// non-Unix shape is testable from a Unix host.
fn drop_dead_exec_grants(caps: &mut Capabilities, unix_available: bool) {
    if unix_available {
        return;
    }
    if let Some(exec) = caps.exec.as_mut() {
        exec.withdraw(|key| {
            matches!(key, ExecKey::Name(name)
                if ral_core::uutils::COREUTILS_UNIX_ONLY_TOOLS.contains(&name.as_str()))
        });
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use ral_core::capability::ExecGrant;
    #[cfg(unix)]
    use ral_core::capability::{Evidence, Fragment};
    use ral_core::guard::freeze::FreezeCtx;
    #[cfg(unix)]
    use ral_core::guard::freeze::Place;
    #[cfg(unix)]
    use ral_core::host::XdgKind;

    fn bare(s: &str) -> ExecKey {
        ExecKey::Name(s.into())
    }

    /// Each dir key's surface, and whether it admits.
    fn dirs(exec: &ExecGrant) -> impl Iterator<Item = (&str, bool)> {
        exec.iter().filter_map(|(key, v)| match key {
            ExecKey::Dir(dir) => Some((dir.as_str(), v.permits())),
            _ => None,
        })
    }
    use ral_core::capability::Capabilities;
    #[cfg(unix)]
    use ral_core::capability::Net;
    #[cfg(unix)]
    use std::path::Path;

    /// Parse *and* freeze a bake-in: the loader resolves places, so a failure
    /// here is malformed source, a refused payload, or an xdg escape.
    fn load(name: &str, text: &str, ctx: &FreezeCtx) -> Capabilities {
        load_with_env(name, text, ctx, &[])
    }

    /// [`load`] under shell-level overrides of the environment a profile's
    /// `env` reads.
    fn load_with_env(
        name: &str,
        text: &str,
        ctx: &FreezeCtx,
        env: &[(&str, &str)],
    ) -> Capabilities {
        let mut shell = ral_core::test_helper::core_shell();
        for (k, v) in env {
            shell.set_env_var(*k, *v);
        }
        ral_core::load::profile::load_capabilities_from_str(
            &ral_core::types::Mooring::adrift(),
            &mut shell,
            text,
            &format!("<test-base:{name}>"),
            ctx,
        )
        .unwrap_or_else(|e| panic!("base '{name}' failed to load: {e:?}"))
    }

    /// [`load`] for an extension file.
    #[cfg(unix)]
    fn fragment(name: &str, text: &str, ctx: &FreezeCtx) -> Fragment {
        let mut shell = ral_core::test_helper::core_shell();
        ral_core::load::profile::load_fragment_from_str(
            &ral_core::types::Mooring::adrift(),
            &mut shell,
            text,
            &format!("<test-fragment:{name}>"),
            ctx,
        )
        .unwrap_or_else(|e| panic!("extension '{name}' failed to load: {e:?}"))
    }

    /// Expect a grant through the door that authored it.  `expand_path_prefix`
    /// is the runtime resolver's infallible twin and normalises differently,
    /// so an equality against it agrees with a frozen grant only by
    /// coincidence — and disagrees exactly where the freeze would have failed.
    #[cfg(unix)]
    fn frozen(place: &Place, ctx: &FreezeCtx) -> ral_core::path::FrozenPath {
        ctx.place(place)
            .ok()
            .flatten()
            .unwrap_or_else(|| panic!("{place:?} should freeze"))
    }

    #[cfg(unix)]
    fn xdg(kind: XdgKind, sub: &str) -> Place {
        Place::Xdg(kind, sub.into())
    }

    /// The real path of a file at `path`, as the in-process exec guard judges it.
    #[cfg(unix)]
    fn object(path: &str) -> String {
        ral_core::path::Resolver::shell_less()
            .resolve(path)
            .canonicalise_lenient()
            .to_string_lossy()
            .into_owned()
    }

    /// The real path of the `name` the host `PATH` finds, if any.
    #[cfg(unix)]
    fn host_real(name: &str) -> Option<String> {
        let path = std::env::var("PATH").ok()?;
        let hit =
            ral_core::path::resolve_in_path(name, &path, ral_core::path::SearchCwd::nowhere())?;
        Some(object(&hit))
    }

    /// Whether `caps` admits running the file whose real path is `real`.
    #[cfg(unix)]
    fn admits(caps: Capabilities, real: &str) -> bool {
        let mut shell = ral_core::test_helper::core_shell();
        shell.with_capabilities(caps, |sh| ral_core::test_access::admits_file(sh, real))
    }

    /// Every bake-in names `home`/`xdg-*` places, so a host with no `HOME` cannot
    /// load one at all — said here rather than by each test failing obscurely.
    fn host_home() -> String {
        ral_core::host::home()
            .expect("these profiles name `home`/`xdg-*` places, so the test host needs HOME")
    }

    /// Every bake-in must load against the real `HOME`, so a broken profile
    /// fails at `cargo test` rather than at a user's first invocation.
    #[cfg(unix)]
    #[test]
    fn bakeins_parse() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: "/".into(),
        };
        for (name, text) in [
            ("minimal", MINIMAL_RAL),
            ("reasonable", REASONABLE_RAL),
            ("read-only", READ_ONLY_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("confined", CONFINED_RAL),
            ("dangerous", DANGEROUS_RAL),
        ] {
            load(name, text, &ctx);
        }
    }

    /// Every bake-in carries Unix-only literals (`/tmp`, `/opt/homebrew/`, …).
    /// On Windows they are foreign-rooted and must drop as dead grants rather
    /// than trip the freeze pass's absoluteness check, or the whole profile
    /// would refuse to load and `dangerous` would be the only usable base.
    /// Gated because that branch fires only under a real `cfg!(windows)`.
    #[cfg(windows)]
    #[test]
    fn bakeins_parse_on_windows() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home.clone()),
            cwd: r"C:\work".into(),
        };
        for (name, text) in [
            ("minimal", MINIMAL_RAL),
            ("reasonable", REASONABLE_RAL),
            ("read-only", READ_ONLY_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("confined", CONFINED_RAL),
            ("dangerous", DANGEROUS_RAL),
        ] {
            load(name, text, &ctx);
        }
    }

    /// The drop reaches fs literals and exec-dir overrides alike: a grant that
    /// can never match a real access must not show up as authority.
    #[cfg(windows)]
    #[test]
    fn minimal_drops_foreign_rooted_grants_on_windows() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home.clone()),
            cwd: r"C:\work".into(),
        };
        let caps = load("minimal", MINIMAL_RAL, &ctx);
        let fs = caps.fs.as_ref().expect("minimal declares fs");
        assert!(
            !fs.read_prefixes.iter().any(|p| p.as_str() == "/tmp"),
            "the Unix-only '/tmp' literal must not survive freeze on Windows"
        );
        let exec = caps.exec.as_ref().expect("minimal declares exec");
        assert!(
            !dirs(exec).any(|(p, _)| p == "/opt/homebrew"),
            "the foreign-rooted '/opt/homebrew/' override must not survive freeze on Windows"
        );
    }

    /// The three load-bearing differences from `reasonable`, pinned so an edit
    /// cannot quietly widen the build jail.  `confined` names no `home`/`xdg-*`
    /// place, so a synthetic home suffices — and nothing may resolve under it.
    #[cfg(unix)]
    #[test]
    fn confined_is_offline_subpath_only_no_home_reads() {
        let ctx = FreezeCtx {
            home: Some("/h".into()),
            cwd: "/work/proj".into(),
        };
        let caps = load("confined", CONFINED_RAL, &ctx);
        assert_eq!(caps.net, Some(Net::Offline), "confined must have net off");
        let exec = caps.exec.as_ref().expect("confined declares exec");
        let bundled = ral_core::uutils::bundled_tools();
        let host_named: Vec<&str> = (exec.iter())
            .filter_map(|(key, _)| match key {
                ExecKey::Name(name) if !bundled.contains(&name.as_str()) => Some(name.as_str()),
                ExecKey::Path(path) => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            host_named.is_empty(),
            "confined names host binaries {host_named:?}; the build jail admits those by \
             directory prefix, and only the bundled tools (which have no path) are listed"
        );
        let fs = caps.fs.as_ref().expect("confined declares fs");
        for prefix in fs.read_prefixes.iter().chain(fs.write_prefixes.iter()) {
            assert!(
                !prefix.as_str().starts_with("/h"),
                "confined fs prefix '{}' reaches into home: build jail must not",
                prefix.as_str()
            );
        }
    }

    /// The one difference from `reasonable`: the working tree is readable but
    /// not writable.  Guards against `cwd` creeping back into `write`.
    #[cfg(unix)]
    #[test]
    fn read_only_does_not_write_cwd() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: "/work/proj".into(),
        };
        let caps = load("read-only", READ_ONLY_RAL, &ctx);
        let fs = caps.fs.as_ref().expect("read-only declares fs");
        assert!(
            !fs.write_prefixes.iter().any(|p| p == "/work/proj"),
            "read-only must not write the working tree"
        );
        assert!(
            fs.read_prefixes.iter().any(|p| p == "/work/proj"),
            "read-only must read the working tree"
        );
    }

    #[test]
    fn dangerous_is_root() {
        let ctx = FreezeCtx {
            home: Some("/h".into()),
            cwd: "/".into(),
        };
        assert_eq!(
            load("dangerous", DANGEROUS_RAL, &ctx),
            Capabilities::default()
        );
    }

    /// `xdg-bin` freezes to `${XDG_BIN_HOME:-~/.local/bin}` and must land in
    /// the grant's `dirs`, where directory keys live.
    #[cfg(unix)]
    #[test]
    fn reasonable_carries_xdg_bin_subpath_in_exec() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: "/".into(),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);
        let exec = caps.exec.as_ref().expect("reasonable should declare exec");
        let xdg_bin = frozen(&xdg(XdgKind::Bin, "."), &ctx);
        assert_eq!(
            exec.get(&ExecKey::Dir(xdg_bin.clone())),
            Some(&Evidence::ALLOW),
            "reasonable should list the resolved xdg-bin ({}) in exec dirs",
            xdg_bin.as_str()
        );
    }

    /// The per-invocation working tree and temp dir reach both halves of the
    /// policy: directory keys in `exec`, plain sigils in `fs`.
    #[cfg(unix)]
    #[test]
    fn minimal_and_reasonable_carry_cwd_and_tempdir_sigils() {
        let home = host_home();
        let cwd = Path::new("/work/proj");
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: cwd.to_path_buf(),
        };
        // Freeze folds away the trailing separator macOS `$TMPDIR` carries, so
        // compare in the same normal form the frozen keys hold.
        let normal = |p: &str| ral_core::path::FrozenPath::from_surface(p).into_string();
        let cwd_resolved = normal(&cwd.to_string_lossy());
        let tempdir_resolved = normal(&std::env::temp_dir().to_string_lossy());
        for (name, text) in [("minimal", MINIMAL_RAL), ("reasonable", REASONABLE_RAL)] {
            let caps = load(name, text, &ctx);
            let exec = caps
                .exec
                .as_ref()
                .unwrap_or_else(|| panic!("{name} should declare exec"));
            let allows = |dir: &str| dirs(exec).any(|(p, v)| p == dir && v);
            assert!(allows(&cwd_resolved), "{name} exec missing resolved cwd");
            assert!(
                allows(&tempdir_resolved),
                "{name} exec missing resolved tempdir"
            );
            let fs = caps
                .fs
                .as_ref()
                .unwrap_or_else(|| panic!("{name} should declare fs"));
            for token in [&cwd_resolved, &tempdir_resolved] {
                assert!(
                    fs.read_prefixes.iter().any(|p| p == token),
                    "{name} fs.read_prefixes missing {token}"
                );
                assert!(
                    fs.write_prefixes.iter().any(|p| p == token),
                    "{name} fs.write_prefixes missing {token}"
                );
            }
        }
    }

    /// End-to-end: once freeze rewrites `cwd` into the working directory, a
    /// path-style exec beneath it is admitted.
    #[cfg(unix)]
    #[test]
    fn freeze_admits_relative_exec_under_cwd_sigil() {
        let work = std::path::Path::new("/work/proj");
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: work.to_path_buf(),
        };
        let caps = load("minimal", MINIMAL_RAL, &ctx);

        assert!(
            admits(caps, "/work/proj/configure"),
            "./configure under `cwd` must be admitted"
        );
    }

    /// `system`'s Homebrew root admits cmake, though cmake is no per-name
    /// entry.  That root exists only where Homebrew does, so on a brew-less
    /// host this passes vacuously rather than failing falsely; CI's macOS
    /// runners ship Homebrew, so it still bites.
    #[cfg(unix)]
    #[test]
    fn reasonable_admits_cmake_under_homebrew_when_present() {
        if !ral_core::path::exists("/opt/homebrew") {
            return;
        }
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: "/".into(),
        };
        let caps = load("reasonable", REASONABLE_RAL, &ctx);
        assert!(
            caps.exec
                .as_ref()
                .is_some_and(|m| dirs(m).any(|(p, v)| p == "/opt/homebrew" && v)),
            "reasonable should list /opt/homebrew in exec dirs when it exists on this host"
        );
        assert!(
            admits(caps, "/opt/homebrew/bin/cmake"),
            "cmake under /opt/homebrew/bin must be admitted"
        );
    }

    /// The selector names `text`'s `exec` allow list targets as `` `dir `NAME ``,
    /// read from the evaluated profile before it is decoded and frozen.
    fn dir_selectors(text: &str) -> std::collections::BTreeSet<String> {
        use ral_core::types::Value;
        let mut shell = ral_core::test_helper::core_shell();
        let profile = ral_core::load::evaluate_source(
            &ral_core::types::Mooring::adrift(),
            &mut shell,
            text,
            "<test-base:selectors>",
            None,
        )
        .unwrap_or_else(|e| panic!("profile failed to evaluate: {e:?}"));
        let field = |v: &Value, k: &str| match v {
            Value::Map(m) => m.get(k).map(std::borrow::Cow::into_owned),
            _ => None,
        };
        let allow = field(&profile, "exec")
            .and_then(|exec| field(&exec, "allow"))
            .expect("the profile has an exec allow list");
        let Value::List(targets) = allow else {
            panic!("exec allow is a list");
        };
        targets
            .iter()
            .filter_map(|t| match &*t {
                Value::Variant { label, payload } if &**label == "dir" => {
                    payload.as_deref().cloned()
                }
                _ => None,
            })
            .filter_map(|inner| match inner {
                Value::Variant {
                    label,
                    payload: None,
                } => Some(label.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Every selector, `path` included, is listed under `dir`: read off the
    /// profile's own value, since what a selector expands to depends on the
    /// host, and a manager absent here would pass vacuously.
    #[test]
    fn reasonable_lists_every_selector() {
        use ral_core::guard::freeze::Selector;
        let want = Selector::names().map(String::from).collect();
        assert_eq!(dir_selectors(REASONABLE_RAL), want);
    }

    /// rustup takes its lock for writing on every proxy run, and stages the
    /// updater it later runs beside it: the lock is writable, its directory
    /// excluded from `exec`.  `RUSTUP_HOME` is emptied, which the profile
    /// reads as unset.
    #[cfg(unix)]
    #[test]
    fn reasonable_writes_the_rustup_lock_and_never_runs_self_update() {
        let ctx = FreezeCtx {
            home: Some(host_home()),
            cwd: "/".into(),
        };
        let caps = load_with_env("reasonable", REASONABLE_RAL, &ctx, &[("RUSTUP_HOME", "")]);
        let home = |sub: &str| frozen(&Place::Home(sub.into()), &ctx);
        let lock = home(".rustup/self-update/self-update.lock");
        let fs = caps.fs.as_ref().expect("reasonable declares fs");
        assert!(
            fs.write_prefixes.contains(&lock),
            "reasonable should write {}",
            lock.as_str()
        );
        let self_update = home(".rustup/self-update");
        let exec = caps.exec.as_ref().expect("reasonable declares exec");
        assert!(
            dirs(exec).any(|(p, v)| p == self_update.as_str() && !v),
            "reasonable should exclude {} from exec",
            self_update.as_str()
        );
    }

    /// A session writes no cache the user's own tools read.
    #[cfg(unix)]
    #[test]
    fn reasonable_and_minimal_write_no_user_cache() {
        let ctx = FreezeCtx {
            home: Some(host_home()),
            cwd: "/".into(),
        };
        let cache = frozen(&xdg(XdgKind::Cache, "."), &ctx);
        for (name, text) in [("minimal", MINIMAL_RAL), ("reasonable", REASONABLE_RAL)] {
            let fs = load(name, text, &ctx).fs.expect("declares fs");
            assert!(
                !fs.write_prefixes.contains(&cache),
                "{name} must not write {}",
                cache.as_str()
            );
        }
    }

    /// Both admit `git` and read `~/.gitconfig`, so a commit works without
    /// `--extend`.  Push and signed commits still don't: their credential
    /// and signing helpers need `~/.ssh` and `~/.gnupg`, which the fs grant
    /// leaves unreadable.
    #[cfg(unix)]
    #[test]
    fn reasonable_and_read_only_admit_git() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: "/".into(),
        };
        let gitconfig = frozen(&Place::Home(".gitconfig".into()), &ctx);
        for (name, text) in [("reasonable", REASONABLE_RAL), ("read-only", READ_ONLY_RAL)] {
            let caps = load(name, text, &ctx);
            let exec = caps.exec.as_ref().expect("base declares exec");
            assert_eq!(
                exec.get(&bare("git")),
                Some(&Evidence::ALLOW),
                "{name} should admit git"
            );
            let fs = caps.fs.as_ref().expect("base declares fs");
            assert!(
                fs.read_prefixes.iter().any(|p| p == &gitconfig),
                "{name} should add resolved ~/.gitconfig ({}) to read prefixes",
                gitconfig.as_str()
            );
        }
    }

    /// `minimal` lists no Homebrew tree and `system` carries none, so brew
    /// tools stay opt-in, and the git extension is the opt-in — its bare
    /// `git` is whichever git the host `PATH` finds.  The cwd must not be `/`,
    /// or the ``dir `cwd .`` allow would admit everything.
    #[cfg(unix)]
    #[test]
    fn minimal_admits_system_git_not_homebrew() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: "/work".into(),
        };
        assert!(
            admits(load("minimal", MINIMAL_RAL, &ctx), "/usr/bin/git"),
            "minimal admits the system git via the /usr/bin/ subpath"
        );
        assert!(
            !admits(load("minimal", MINIMAL_RAL, &ctx), "/opt/homebrew/bin/git"),
            "minimal does not admit a Homebrew git: brew trees are opt-in"
        );
        let extended =
            load("minimal", MINIMAL_RAL, &ctx).extend(fragment("git-ext", GIT_EXTENSION_RAL, &ctx));
        assert_eq!(
            extended
                .exec
                .as_ref()
                .and_then(|exec| exec.get(&bare("git"))),
            Some(&Evidence::ALLOW)
        );
        if let Some(git) = host_real("git") {
            assert!(
                admits(extended, &git),
                "the git extension's `name git` carries the host's git, wherever it lives"
            );
        }
    }

    /// The extension extends `minimal` — gitconfig readable, `git` admitted —
    /// while `reasonable`'s credential denies survive: extending an `FsPolicy`
    /// unions deny sets, so a veto sticks without the overlay re-stating it, which is
    /// what lets an overlay stay purely additive.  Both sides freeze against
    /// the same home, so their `xdg-config` places coincide.
    #[cfg(unix)]
    #[test]
    fn git_extension_extends_into_git_capable_profile() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: "/".into(),
        };
        let gitconfig = frozen(&Place::Home(".gitconfig".into()), &ctx);
        let xdg_config_git = frozen(&xdg(XdgKind::Config, "git"), &ctx);

        let extended_minimal = load("minimal", MINIMAL_RAL, &ctx).extend(fragment(
            "git-extension",
            GIT_EXTENSION_RAL,
            &ctx,
        ));
        let exec = extended_minimal
            .exec
            .as_ref()
            .expect("extension should keep exec map");
        assert_eq!(exec.get(&bare("git")), Some(&Evidence::ALLOW));
        let fs = extended_minimal
            .fs
            .as_ref()
            .expect("extension should keep fs map");
        assert!(
            fs.read_prefixes.iter().any(|p| p == &gitconfig),
            "extending minimal should add ~/.gitconfig"
        );
        assert!(
            fs.read_prefixes.iter().any(|p| p == &xdg_config_git),
            "extending minimal should add xdg-config git"
        );

        let extended_reasonable = load("reasonable", REASONABLE_RAL, &ctx).extend(fragment(
            "git-extension",
            GIT_EXTENSION_RAL,
            &ctx,
        ));
        let fs = extended_reasonable
            .fs
            .as_ref()
            .expect("extension should keep fs map");
        for denied in ["gh", "op", "gcloud"] {
            let resolved = frozen(&xdg(XdgKind::Config, denied), &ctx);
            assert!(
                fs.deny_paths.iter().any(|p| p == &resolved),
                "extending reasonable should preserve xdg-config {denied} deny ({})",
                resolved.as_str()
            );
        }
    }

    /// A positive extension reaches a target the base never listed, and an
    /// area it does not name stays as the base left it.
    #[cfg(unix)]
    #[test]
    fn a_positive_extension_of_an_unlisted_target_is_admitted() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home.clone()),
            cwd: "/work".into(),
        };
        let tool = object(&format!("{home}/unlisted-tool"));
        let minimal = load("minimal", MINIMAL_RAL, &ctx);
        assert!(!admits(minimal.clone(), &tool), "{tool} is not minimal's");
        let extended = minimal.extend(fragment(
            "home-dir",
            "return [exec: [allow: [`dir `home .]]]",
            &ctx,
        ));
        assert!(
            admits(extended, &tool),
            "the extension names {tool}'s folder"
        );
    }

    /// An exclusion wins over every allowance: an exact file or a deeper
    /// directory in an extension does not reopen `minimal`'s `bash`.
    #[cfg(unix)]
    #[test]
    fn neither_an_exact_file_nor_a_deeper_directory_reopens_an_exclusion() {
        let ctx = FreezeCtx {
            home: Some(host_home()),
            cwd: "/work".into(),
        };
        for source in [
            "return [exec: [allow: [`absolute /usr/bin/bash]]]",
            "return [exec: [allow: [`dir `absolute /usr/bin]]]",
        ] {
            let extended =
                load("minimal", MINIMAL_RAL, &ctx).extend(fragment("reopen", source, &ctx));
            assert!(
                !admits(extended, "/usr/bin/bash"),
                "`{source}` must not reopen the `name bash` exclusion"
            );
        }
    }

    /// Verb lists join by union: a base cannot withhold a verb against an
    /// extension.
    #[cfg(unix)]
    #[test]
    fn verbs_join_by_union() {
        use ral_core::capability::ShellVerb;
        let ctx = FreezeCtx {
            home: Some(host_home()),
            cwd: "/work".into(),
        };
        let base = Capabilities {
            shell: Some([ShellVerb::Chdir].into()),
            ..Capabilities::default()
        };
        let extended = base.extend(fragment("verbs", "return [shell: [`detach]]", &ctx));
        assert_eq!(
            extended.shell,
            Some([ShellVerb::Chdir, ShellVerb::Detach].into())
        );
    }

    /// A base that omits exec but restricts fs, extended with a run allowance
    /// beside `every`: the ceiling keeps the allowance and reads what it
    /// names.  The program sits in a scratch dir, outside the read floor.
    #[cfg(unix)]
    #[test]
    #[allow(clippy::disallowed_methods, reason = "[test] scratch executable")]
    fn a_run_allowance_beside_every_is_readable_under_an_fs_only_base() {
        let ctx = FreezeCtx {
            home: Some(host_home()),
            cwd: "/work".into(),
        };
        let base = Capabilities {
            fs: Some(ral_core::capability::FsPolicy {
                read_prefixes: vec![frozen(&Place::Absolute("/work".into()), &ctx)],
                ..Default::default()
            }),
            ..Capabilities::default()
        };
        let dir = tempfile::tempdir().expect("scratch dir");
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").expect("write the tool");
        let tool = object(&tool.to_string_lossy());
        assert!(!admits(base.clone(), &tool), "{tool} is outside the reads");
        let extended = base.extend(fragment(
            "tool",
            &format!("return [exec: [allow: [`absolute {tool}]]]"),
            &ctx,
        ));
        assert!(admits(extended, &tool), "the allowance reads what it names");
    }

    /// `system` must expand to an allowance for every live tool root —
    /// merely finding the key would pass on a deny too, which is the whole
    /// point.  Ungated, since the expansion is the host's own and the
    /// assertion holds either way.
    #[test]
    fn every_exec_base_admits_live_system_tool_roots() {
        let home = host_home();
        // Absolute on every platform, unlike the `/work` literal the
        // `cfg(unix)`-gated tests use — Windows reads that as rootless.
        let cwd = std::env::temp_dir();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd,
        };
        let roots = ral_core::guard::toolchains::expand(
            ral_core::guard::toolchains::Toolchain::System,
            ctx.home.as_deref(),
        )
        .expect("the system roots expand");
        for (name, text) in [
            ("reasonable", REASONABLE_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("read-only", READ_ONLY_RAL),
            ("minimal", MINIMAL_RAL),
            ("confined", CONFINED_RAL),
        ] {
            let caps = load(name, text, &ctx);
            let exec = caps
                .exec
                .as_ref()
                .unwrap_or_else(|| panic!("{name} should declare exec"));
            for root in &roots {
                let normalized = ral_core::path::FrozenPath::from_surface(root).into_string();
                assert!(
                    dirs(exec).any(|(p, v)| p == normalized && v),
                    "{name} should allow the live system tool root {normalized}"
                );
            }
        }
    }

    /// A bundled tool (`ral_core::uutils`) is spawned by bare name and never
    /// resolved on disk, so a directory prefix can never admit one: a profile
    /// that omits one leaves an agent unable to run it. Every base a *spawn*
    /// may name must therefore settle each bundled tool by literal — `allow`
    /// or `deny`, but never silence, which denies it with no decision recorded.
    /// The list is [`super::SPAWN_BASES`], the bases a spawn may name, which
    /// no base; `dangerous` is absent because it declares no exec map to
    /// restrict.
    #[test]
    fn every_grantable_base_settles_the_bundled_tools() {
        let home = host_home();
        let cwd = std::env::temp_dir();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd,
        };
        for (name, text) in [
            ("reasonable", REASONABLE_RAL),
            ("edit-only", EDIT_ONLY_RAL),
            ("read-only", READ_ONLY_RAL),
            ("confined", CONFINED_RAL),
        ] {
            let caps = load(name, text, &ctx);
            let exec = caps
                .exec
                .as_ref()
                .unwrap_or_else(|| panic!("{name} should declare exec"));
            let unsettled: Vec<&str> = ral_core::uutils::bundled_tools()
                .into_iter()
                .filter(|tool| exec.get(&bare(tool)).is_none())
                .collect();
            assert!(
                unsettled.is_empty(),
                "{name} leaves these bundled tools unnamed: a bare name matches no prefix, \
                 so silence makes them unrunnable: {unsettled:?}"
            );
        }
    }

    /// Passing [`drop_dead_exec_grants`] `false` simulates a non-Unix host:
    /// the `coreutils-unix-only` grants go, ordinary and cross-platform names
    /// stay.  Unix-gated because it first checks the host really bundles them.
    #[cfg(unix)]
    #[test]
    fn reasonable_drops_unix_only_bundled_tool_grants_off_unix() {
        let home = host_home();
        let ctx = FreezeCtx {
            home: Some(home),
            cwd: "/".into(),
        };
        let mut caps = load("reasonable", REASONABLE_RAL, &ctx);
        {
            let exec = caps.exec.as_ref().expect("reasonable declares exec");
            assert!(
                exec.get(&bare("tac")).is_some(),
                "host build should still bundle tac"
            );
            assert!(
                exec.get(&bare("test")).is_some(),
                "host build should still bundle test"
            );
        }

        drop_dead_exec_grants(&mut caps, false);

        let exec = caps.exec.as_ref().expect("reasonable declares exec");
        for dead in ral_core::uutils::COREUTILS_UNIX_ONLY_TOOLS {
            assert!(
                exec.get(&bare(dead)).is_none(),
                "'{dead}' should be dropped when the platform can't bundle it"
            );
        }
        assert!(
            exec.get(&bare("git")).is_some(),
            "an ordinary named binary must survive the drop"
        );
        assert!(
            exec.get(&bare("cat")).is_some(),
            "a cross-platform bundled tool must survive the drop"
        );
    }

    #[test]
    fn dropping_dead_grants_keeps_exclusions() {
        use ral_core::capability::Evidence;
        let [allowed, denied, both, ..] = *ral_core::uutils::COREUTILS_UNIX_ONLY_TOOLS else {
            unreachable!("at least three unix-only tools")
        };
        let mut caps = Capabilities::deny_all();
        caps.exec = Some(
            [
                (bare(allowed), Evidence::ALLOW),
                (bare(denied), Evidence::DENY),
                (bare(both), Evidence::BOTH),
                (bare("git"), Evidence::ALLOW),
            ]
            .into_iter()
            .collect(),
        );

        drop_dead_exec_grants(&mut caps, false);

        let exec = caps.exec.as_ref().unwrap();
        assert_eq!(exec.get(&bare(allowed)), None);
        assert_eq!(exec.get(&bare(denied)), Some(&Evidence::DENY));
        assert_eq!(exec.get(&bare(both)), Some(&Evidence::DENY));
        assert_eq!(exec.get(&bare("git")), Some(&Evidence::ALLOW));
    }
}
