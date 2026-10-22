//! Carriers: the files `execve` forwards to before a program's own code runs —
//! a `#!` script's interpreter and, on macOS, the system's root-owned shims.
//! They are support (see `projection`): live where no exclusion covers them,
//! admitted and readable beside the grant, or an admitted script could not start.
//!
//! The system vouches for its own scripts and shims; a script the user can
//! author must have its interpreter named in the grant, and so must the
//! program an `env` interpreter runs.  Kernel-only: the in-process guard
//! never consults them.

use crate::path::walk::{Kind, Vetted, walk_with};
use crate::path::{
    CwdPath, Judged, LeafMode, LexicalPath, Located, ObjectId, Reading, RealPath, Resolver, Trace,
    is_absolute,
};
use crate::types::EnvVars;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Interpreter hops followed, as the kernel bounds them.
const DEPTH: usize = 4;

/// The carriers of `programs`: every hop, up to [`DEPTH`] deep, that only the
/// system can have written, reached from a program only the system can have
/// written.
pub(crate) fn carriers<'a>(programs: impl IntoIterator<Item = &'a RealPath>) -> BTreeSet<RealPath> {
    let mut found = BTreeSet::new();
    let mut frontier: Vec<Trace> = programs
        .into_iter()
        .filter_map(|program| trusted_real(program.as_path()))
        .collect();
    for _ in 0..DEPTH {
        frontier = frontier
            .iter()
            .flat_map(|carrier| hops(carrier.real()))
            .filter_map(|hop| trusted_real(&hop))
            .filter(|hop| found.insert(hop.real().clone()))
            .collect();
    }
    found
}

/// No kernel linker candidates to admit off Linux: macOS's loader is Rosetta's
/// directory, which the profile names itself.
#[cfg(not(target_os = "linux"))]
pub(crate) fn linkers() -> Vec<RealPath> {
    Vec::new()
}

/// The dynamic linkers, as regular files: `execve` of a dynamic binary needs
/// the binary's `PT_INTERP` file.  Support for a launch under restricted exec,
/// admitted unless excluded.  Never a directory, which under `/usr/bin` would
/// admit a command directory.
#[cfg(target_os = "linux")]
pub(crate) fn linkers() -> Vec<RealPath> {
    const PATTERNS: &[&str] = &[
        "/lib/ld*.so*",
        "/lib64/ld*.so*",
        "/lib32/ld*.so*",
        "/usr/lib/ld*.so*",
        "/usr/lib64/ld*.so*",
        "/usr/lib32/ld*.so*",
        // Debian multiarch, e.g. /usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1.
        "/lib/*/ld*.so*",
        "/usr/lib/*/ld*.so*",
    ];
    let scope = crate::process::Scope::clone(&crate::process::DurableRoot::default());
    let found: BTreeSet<RealPath> = PATTERNS
        .iter()
        .filter_map(|p| crate::path::tree::glob::Glob::parse(&PathBuf::from(*p)).ok())
        .map(std::sync::Arc::new)
        .flat_map(|g| {
            g.walk()
                .sightings(scope.clone())
                .filter_map(Result::ok)
                .map(|s| s.path)
                .filter(|p| g.matches(p))
                .collect::<Vec<_>>()
        })
        // Merged-/usr makes /lib/x and /usr/lib/x one inode: one real path.
        .filter_map(|p| file(&p))
        .collect();
    found.into_iter().collect()
}

/// Where `execve` of `program` goes next: the system's shims, then its
/// interpreter.  Never what an `env` interpreter runs: `env` is the carrier.
fn hops(program: &RealPath) -> Vec<PathBuf> {
    let mut hops = Vec::new();
    #[cfg(target_os = "macos")]
    hops.extend(shims(program));
    hops.extend(shebang(program.as_path()));
    hops
}

/// What `path` reaches if only the system can have written it: walked from
/// `/`, every directory and the file itself neither owned nor writable by
/// this euid, and every symlink not owned by it.  Root owns everything, so it
/// trusts nothing: that fails closed.  The final object must be a regular
/// file: a carrier is bound read-only whole, so a directory would expose its
/// tree.
pub(crate) fn trusted_real(path: &Path) -> Option<Trace> {
    let me = rustix::process::geteuid().as_raw();
    let located = walk_with(&spelled(path)?, LeafMode::Follow, |v| system_only(v, me)).ok()?;
    (located.trace().kind() == Some(Kind::File)).then(|| located.trace().clone())
}

/// The real path of `path` where it reaches a regular file.
#[cfg(target_os = "linux")]
fn file(path: &Path) -> Option<RealPath> {
    use crate::path::walk;
    let located = walk(&spelled(path)?, LeafMode::Follow).ok()?;
    (located.trace().kind() == Some(Kind::File)).then(|| located.real().clone())
}

/// `path`, which is absolute, as the spelling a walk takes: anchored on `/`,
/// which an absolute path never reads.
fn spelled(path: &Path) -> Option<LexicalPath> {
    let root = CwdPath::new(PathBuf::from("/")).ok()?;
    Some(Resolver::at(&EnvVars::new(), &root).anchor(path))
}

/// Whether only the system can have made the object `v` offers what it is:
/// the entry is still the object the walk holds, which is not owned by `me`
/// and — unless a symlink, whose own mode the kernel ignores — not writable
/// by it either.  An object the walk could not hold is refused.
fn system_only(v: &Vetted<'_>, me: u32) -> bool {
    use rustix::fs::{Access, AtFlags, accessat, fstat, statat};
    let Some(child) = v.child else {
        return false;
    };
    let (Ok(held), Ok(named)) = (
        fstat(child),
        statat(v.dir, v.name, AtFlags::SYMLINK_NOFOLLOW),
    ) else {
        return false;
    };
    let is_link = v.step.is_some_and(|step| step.kind() == Kind::Symlink);
    ObjectId::of(child).is_ok_and(|id| id == ObjectId::from(&named))
        && held.st_uid != me
        && (is_link || accessat(v.dir, v.name, Access::WRITE_OK, AtFlags::EACCESS).is_err())
}

/// The interpreter on `path`'s `#!` line: the kernels' grammar narrowed to the
/// shapes it accepts — a line ending within the first `LIMIT` bytes (on macOS
/// at the first `\n` or `#`), blanks skipped, the interpreter running to the
/// next blank, and absolute.  Any other shape yields no carrier, and the
/// trampoline's hint names the interpreter.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:carrier-shebang] reads the head of a seed, a hop or a refused exec's target by name to learn its interpreter, as the kernel will; sandbox rendering, not the model's data I/O"
)]
pub(crate) fn shebang(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    // `O_NONBLOCK` and the regular-file check: a FIFO swapped in must not stall the reader.
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    file.metadata()
        .ok()?
        .is_file()
        .then(|| head_shebang(file))?
}

/// The interpreter on the `#!` line of the program that was judged, read
/// through its own handle: the object the launch is admitted for, whatever the
/// name holds by now.
pub(crate) fn launched_shebang(program: &Located<Judged<Reading>>) -> Option<PathBuf> {
    head_shebang(program.read().ok()?)
}

fn head_shebang(file: File) -> Option<PathBuf> {
    const LIMIT: u64 = if cfg!(target_os = "macos") { 512 } else { 256 };
    let mut head = Vec::new();
    file.take(LIMIT).read_to_end(&mut head).ok()?;
    let line = head.strip_prefix(b"#!")?;
    let end = line
        .iter()
        .position(|&b| b == b'\n' || (cfg!(target_os = "macos") && b == b'#'))?;
    let blank = |b: &u8| *b == b' ' || *b == b'\t';
    let line = &line[..end];
    let start = line.iter().position(|b| !blank(b)).unwrap_or(line.len());
    let interpreter = line[start..].split(blank).next()?;
    let interpreter = PathBuf::from(OsStr::from_bytes(interpreter));
    is_absolute(&interpreter).then_some(interpreter)
}

/// What the system's own shims forward to: `/bin/sh` to the shell
/// `/private/var/select/sh` selects, and a stub under `/usr/bin` to the real
/// tool in the developer directory.  Never `DEVELOPER_DIR`: the confined code
/// controls its environment.
#[cfg(target_os = "macos")]
pub(super) fn shims(program: &RealPath) -> Vec<PathBuf> {
    if program.as_path().as_os_str() == "/bin/sh" {
        return vec![PathBuf::from("/private/var/select/sh")];
    }
    let real = |name: &str| spelled(&PathBuf::from(name)).and_then(|at| RealPath::of(&at).ok());
    let (Some(usr_bin), Some(name)) = (real("/usr/bin"), program.as_path().file_name()) else {
        return Vec::new();
    };
    if program.parent().as_ref() != Some(&usr_bin) {
        return Vec::new();
    }
    let dev = real("/private/var/db/xcode_select_link").map_or_else(
        || PathBuf::from("/Library/Developer/CommandLineTools"),
        |link| link.as_path().to_path_buf(),
    );
    vec![
        crate::path::join_slash(&dev, "usr/bin").join(name),
        crate::path::join_slash(&dev, "Toolchains/XcodeDefault.xctoolchain/usr/bin").join(name),
    ]
}

/// A script this host's system wrote and the interpreter it names, for the
/// tests that prove a system script's interpreter is carried; `None` where
/// the host has no such script among a few known ones, or runs us as root,
/// which trusts nothing.
#[cfg(test)]
pub(super) fn system_script() -> Option<(RealPath, RealPath)> {
    [
        "/usr/bin/ldd",
        "/usr/bin/lsb_release",
        "/usr/sbin/update-ca-certificates",
    ]
    .into_iter()
    .find_map(|script| {
        let script = trusted_real(&PathBuf::from(script))?;
        let interp = trusted_real(&shebang(script.real().as_path())?)?;
        Some((script.real().clone(), interp.real().clone()))
    })
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: tempdir programs and their modes"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// An executable `name` under `dir` holding `body`.
    fn file(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = crate::path::canon::canonicalise_strict(dir.path()).expect("temp dir resolves");
        (dir, root)
    }

    fn real(path: &Path) -> RealPath {
        RealPath::of_path(path).expect("it exists")
    }

    #[test]
    fn a_script_hops_to_its_interpreter() {
        let (_dir, root) = temp_root();
        let interp = file(&root, "interp", "\x7fELF");
        let script = file(
            &root,
            "run",
            &format!("#!  {}\t-x\nbody\n", interp.display()),
        );
        assert_eq!(hops(&real(&script)), [interp]);
        assert_eq!(
            hops(&real(&file(&root, "plain", "no shebang\n"))),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn env_is_the_carrier_and_what_it_runs_is_not_followed() {
        let (_dir, root) = temp_root();
        let env = file(&root.join("bin"), "env", "\x7fELF");
        let tool = file(&root.join("bin"), "tool", "\x7fELF");
        let script = file(
            &root,
            "a",
            &format!("#!{} {}\n", env.display(), tool.display()),
        );
        assert_eq!(hops(&real(&script)), [env]);
    }

    #[test]
    fn a_relative_interpreter_yields_nothing() {
        let (_dir, root) = temp_root();
        assert_eq!(shebang(&file(&root, "a", "#!sh\n")), None);
        assert_eq!(shebang(&file(&root, "b", "#!\n")), None);
    }

    #[test]
    fn a_line_with_no_end_in_the_buffer_yields_nothing() {
        let (_dir, root) = temp_root();
        assert_eq!(shebang(&file(&root, "a", "#!/bin/sh")), None);
    }

    /// The launched program's `#!` line is the admitted object's, though its
    /// name holds another script by the time it is read.
    #[test]
    fn a_launched_programs_interpreter_is_read_through_the_judged_handle() {
        use crate::capability::Program;
        let (_dir, root) = temp_root();
        let script = file(&root, "run", "#!/bin/sh\nbody\n");
        let admitted =
            super::super::launch::admitted(Program::file(script.clone()).expect("a script"), &[]);
        std::fs::rename(file(&root, "other", "#!/bin/other\n"), &script).expect("swap");

        assert_eq!(shebang(&script), Some(PathBuf::from("/bin/other")));
        let Program::File { located, .. } = admitted.program() else {
            panic!("a script is a file")
        };
        assert_eq!(launched_shebang(located), Some(PathBuf::from("/bin/sh")));
    }

    /// macOS ends the `#!` line at a `#`; Linux reads it as part of the name.
    #[test]
    fn a_hash_ends_the_interpreter_only_on_macos() {
        let (_dir, root) = temp_root();
        let expected = if cfg!(target_os = "macos") {
            "/bin/sh"
        } else {
            "/bin/sh#x"
        };
        assert_eq!(
            shebang(&file(&root, "a", "#!/bin/sh#x\n")),
            Some(PathBuf::from(expected))
        );
    }

    /// Files in a temp dir are this uid's, who could write any interpreter
    /// into them, whatever grant the launch runs under.
    #[test]
    fn what_the_user_can_author_vouches_for_nothing() {
        let (_dir, root) = temp_root();
        let interp = file(&root, "interp", "\x7fELF");
        let script = file(&root, "run", &format!("#!{}\n", interp.display()));
        assert!(carriers([&real(&script)]).is_empty());
        for path in [&script, &interp, &root] {
            assert!(
                trusted_real(path).is_none(),
                "{} is the user's",
                path.display()
            );
        }
        assert!(
            trusted_real(Path::new("/tmp")).is_none(),
            "a directory anyone may write is on no trusted chain"
        );
    }

    /// The link is the user's to repoint, whatever it points at now.
    #[test]
    fn a_user_symlink_to_a_system_binary_is_not_trusted() {
        let sh = Path::new("/bin/sh");
        if trusted_real(sh).is_none() {
            return;
        }
        let (_dir, root) = temp_root();
        let link = root.join("sh");
        std::os::unix::fs::symlink(sh, &link).expect("symlink");
        assert_eq!(trusted_real(&link), None);
    }

    #[test]
    fn a_directory_is_no_carrier() {
        for dir in ["/", "/usr", "/usr/bin", "/bin"] {
            assert_eq!(trusted_real(Path::new(dir)), None, "{dir}");
        }
    }

    #[test]
    fn an_empty_link_is_no_carrier() {
        let (_dir, root) = temp_root();
        let link = root.join("empty");
        if std::os::unix::fs::symlink("", &link).is_ok() {
            assert_eq!(trusted_real(&link), None);
        }
    }

    #[test]
    fn a_system_script_carries_its_interpreter() {
        if let Some((script, interp)) = system_script() {
            assert!(carriers([&script]).contains(&interp), "{interp}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_shims_forward_to_what_they_select() {
        if let (Some(sh), Some(select)) = (
            trusted_real(Path::new("/bin/sh")),
            trusted_real(Path::new("/private/var/select/sh")),
        ) {
            let got = carriers([sh.real()]);
            assert!(got.contains(select.real()), "{got:?}");
        }
        if let (Some(git), Some(real), false) = (
            trusted_real(Path::new("/usr/bin/git")),
            trusted_real(Path::new("/Library/Developer/CommandLineTools/usr/bin/git")),
            crate::path::exists("/private/var/db/xcode_select_link"),
        ) {
            let got = carriers([git.real()]);
            assert!(got.contains(real.real()), "{got:?}");
        }
    }
}
