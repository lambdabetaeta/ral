//! Carriers: the files `execve` forwards to before a program's own code runs —
//! a `#!` script's interpreter and, on macOS, the system's root-owned shims.
//! The kernel exec rules admit them beside the grant, or an admitted script
//! could not start.
//!
//! The system vouches for its own scripts and shims; a script the user can
//! author must have its interpreter named in the grant, and so must the
//! program an `env` interpreter runs.  Kernel-only: the in-process guard
//! never consults them.

use crate::path::RealPath;
use crate::path::walk::MAX_HOPS;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Interpreter hops followed, as the kernel bounds them.
const DEPTH: usize = 4;

/// The carriers of `programs`: every hop, up to [`DEPTH`] deep, that only the
/// system can have written, reached from a program only the system can have
/// written.
pub(crate) fn carriers<'a>(programs: impl IntoIterator<Item = &'a RealPath>) -> BTreeSet<RealPath> {
    let mut found = BTreeSet::new();
    let mut frontier: Vec<RealPath> = programs
        .into_iter()
        .filter_map(|program| trusted_real(program.as_path()))
        .collect();
    for _ in 0..DEPTH {
        frontier = frontier
            .iter()
            .flat_map(hops)
            .filter_map(|hop| trusted_real(&hop))
            .filter(|hop| found.insert(hop.clone()))
            .collect();
    }
    found
}

/// Where `execve` of `program` goes next: the system's shims, then its
/// interpreter.  Never what an `env` interpreter runs: `env` is the carrier.
fn hops(program: &RealPath) -> Vec<PathBuf> {
    let mut hops = Vec::new();
    #[cfg(target_os = "macos")]
    hops.extend(shims(program.as_path()));
    hops.extend(shebang(program.as_path()));
    hops
}

/// The real path of `path` if only the system can have written it: walked
/// from `/` one component at a time, every directory and the file itself
/// neither owned nor writable by this euid, and every symlink not owned by
/// it.  Root owns everything, so it trusts nothing: that fails closed.  The
/// final object must be a regular file: a carrier is bound read-only whole, so
/// a directory would expose its tree.
pub(crate) fn trusted_real(path: &Path) -> Option<RealPath> {
    if !path.is_absolute() {
        return None;
    }
    let me = rustix::process::geteuid().as_raw();
    let system_only = |path: &Path, uid: u32| {
        uid != me
            && rustix::fs::accessat(
                rustix::fs::CWD,
                path,
                rustix::fs::Access::WRITE_OK,
                rustix::fs::AtFlags::EACCESS,
            )
            .is_err()
    };
    let mut walked = PathBuf::from("/");
    if !system_only(&walked, rustix::fs::stat(&walked).ok()?.st_uid) {
        return None;
    }
    let mut pending: Vec<OsString> = Vec::new();
    let push = |pending: &mut Vec<OsString>, path: &Path| {
        pending.extend(path.components().rev().map(|c| c.as_os_str().to_owned()));
    };
    push(&mut pending, path);
    let mut links = 0;
    while let Some(name) = pending.pop() {
        match name.as_bytes() {
            b"/" => walked = PathBuf::from("/"),
            b"." => {}
            // `walked` is link-free, so `..` is lexical on it.
            b".." => {
                walked.pop();
            }
            _ => {
                let next = walked.join(&name);
                let stat = rustix::fs::lstat(&next).ok()?;
                if rustix::fs::FileType::from_raw_mode(stat.st_mode).is_symlink() {
                    links += 1;
                    if stat.st_uid == me || links > MAX_HOPS {
                        return None;
                    }
                    let target = rustix::fs::readlink(&next, Vec::new()).ok()?;
                    if target.to_bytes().is_empty() {
                        return None;
                    }
                    let target = PathBuf::from(OsString::from_vec(target.into_bytes()));
                    push(&mut pending, &target);
                } else if system_only(&next, stat.st_uid) {
                    walked = next;
                } else {
                    return None;
                }
            }
        }
    }
    let stat = rustix::fs::stat(&walked).ok()?;
    rustix::fs::FileType::from_raw_mode(stat.st_mode)
        .is_file()
        .then(|| RealPath::of(&walked).ok())
        .flatten()
}

/// The interpreter on `path`'s `#!` line: the kernels' grammar narrowed to the
/// shapes it accepts — a line ending within the first `LIMIT` bytes (on macOS
/// at the first `\n` or `#`), blanks skipped, the interpreter running to the
/// next blank, and absolute.  Any other shape yields no carrier, and the
/// trampoline's hint names the interpreter.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:carrier-shebang] reads the head of an admitted program to learn its interpreter, as the kernel will; sandbox rendering, not the model's data I/O"
)]
pub(crate) fn shebang(path: &Path) -> Option<PathBuf> {
    const LIMIT: u64 = if cfg!(target_os = "macos") { 512 } else { 256 };
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(LIMIT)
        .read_to_end(&mut head)
        .ok()?;
    let line = head.strip_prefix(b"#!")?;
    let end = line
        .iter()
        .position(|&b| b == b'\n' || (cfg!(target_os = "macos") && b == b'#'))?;
    let blank = |b: &u8| *b == b' ' || *b == b'\t';
    let line = &line[..end];
    let start = line.iter().position(|b| !blank(b)).unwrap_or(line.len());
    let interpreter = line[start..].split(blank).next()?;
    let interpreter = PathBuf::from(OsStr::from_bytes(interpreter));
    interpreter.is_absolute().then_some(interpreter)
}

/// What the system's own shims forward to: `/bin/sh` to the shell
/// `/private/var/select/sh` selects, and a stub under `/usr/bin` to the real
/// tool in the developer directory.  Never `DEVELOPER_DIR`: the confined code
/// controls its environment.
#[cfg(target_os = "macos")]
fn shims(program: &Path) -> Vec<PathBuf> {
    if program.as_os_str() == "/bin/sh" {
        return vec![PathBuf::from("/private/var/select/sh")];
    }
    let (Some(dir), Some(name)) = (program.parent(), program.file_name()) else {
        return Vec::new();
    };
    if dir.as_os_str() != "/usr/bin" {
        return Vec::new();
    }
    let dev = crate::path::canon::canonicalise_strict(&PathBuf::from(
        "/private/var/db/xcode_select_link",
    ))
    .unwrap_or_else(|_| PathBuf::from("/Library/Developer/CommandLineTools"));
    vec![
        dev.join("usr/bin").join(name),
        dev.join("Toolchains/XcodeDefault.xctoolchain/usr/bin")
            .join(name),
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
        let interp = trusted_real(&shebang(script.as_path())?)?;
        Some((script, interp))
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
        RealPath::of(path).expect("it exists")
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
            let got = carriers([&sh]);
            assert!(got.contains(&select), "{got:?}");
        }
        if let (Some(git), Some(real), false) = (
            trusted_real(Path::new("/usr/bin/git")),
            trusted_real(Path::new("/Library/Developer/CommandLineTools/usr/bin/git")),
            crate::path::exists("/private/var/db/xcode_select_link"),
        ) {
            let got = carriers([&git]);
            assert!(got.contains(&real), "{got:?}");
        }
    }
}
