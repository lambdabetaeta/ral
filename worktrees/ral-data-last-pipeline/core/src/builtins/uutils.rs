#[cfg(feature = "coreutils")]
macro_rules! declare_coreutils {
    ($($name:literal => $module:ident),+ $(,)?) => {
        $(use $module;)+

        /// Names of every coreutils tool the helper subprocess can dispatch.
        /// The macro keeps this list in lockstep with the dispatch arms below —
        /// adding a tool in one place adds it in both.  Consulted by
        /// [`is_uutils_tool`] together with [`DIFFUTILS_TOOLS`].
        pub(crate) const COREUTILS_TOOLS: &[&str] = &[$($name),+];

        fn coreutils_invoke(tool: &str, args: Vec<std::ffi::OsString>) -> i32 {
            match tool {
                $($name => $module::uumain(args.into_iter()),)+
                _ => 1,
            }
        }
    };
}

/// Bundled diffutils tools — currently `cmp` and `diff`, both gated on the
/// `diffutils` Cargo feature.  Each ships an argv-style shim that runs in
/// the helper subprocess; the parent process never executes them in-process.
#[cfg(feature = "diffutils")]
pub(crate) const DIFFUTILS_TOOLS: &[&str] = &["cmp", "diff"];

/// True when `name` is one of the bundled uutils tools — coreutils or
/// diffutils.  The resolver substitutes a re-exec of ourselves with
/// `--ral-uutils-helper` for these, so they ride through the same
/// external-command boundary as `/usr/bin/cat`.
pub(crate) fn is_uutils_tool(_name: &str) -> bool {
    #[cfg(feature = "coreutils")]
    if COREUTILS_TOOLS.iter().any(|t| *t == _name) {
        return true;
    }
    #[cfg(feature = "diffutils")]
    if DIFFUTILS_TOOLS.iter().any(|t| *t == _name) {
        return true;
    }
    false
}

/// Hidden multicall sentinel.  The parent process passes this as the
/// first argument when spawning `current_exe()` to act as a bundled
/// coreutils helper.  Not user-facing; not in `--help`.
pub const HELPER_FLAG: &str = "--ral-uutils-helper";

/// One-line check at the very top of a binary's `main`: when the first
/// argument is the helper sentinel, dispatch to the bundled coreutils
/// runtime and return its exit code; otherwise return `None` and let
/// normal `main` run.  ral and exarch both call this so the multicall
/// dispatch isn't duplicated.
///
/// Each invocation runs in a fresh OS process, so process-global state
/// inside `uucore` (locale init, the `EXIT_CODE` atomic) starts clean
/// every time.  Returned values are clamped to `0..=255` for `ExitCode`.
pub fn try_run_uutils_helper() -> Option<u8> {
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    if args.next().as_deref().map(std::ffi::OsStr::to_string_lossy)? != HELPER_FLAG {
        return None;
    }
    #[cfg(any(feature = "coreutils", feature = "diffutils"))]
    {
        // Rust's runtime sets SIGPIPE=IGN before main; uucore writes
        // therefore see EPIPE and return 1 instead of the helper dying
        // from SIGPIPE.  A non-final pipeline stage that exits 1 is
        // indistinguishable from a real error — `yes | head` would
        // mis-report failure.  Restore the default disposition so the
        // helper dies with signal 13 → exit 141, which `is_broken_pipe_exit`
        // forgives.
        #[cfg(unix)]
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL); }
        let tool_args: Vec<std::ffi::OsString> = args.collect();
        if tool_args.is_empty() {
            eprintln!("ral: {HELPER_FLAG} requires a tool name");
            return Some(2);
        }
        let tool = tool_args[0].to_string_lossy().into_owned();
        let code = uutils_invoke(&tool, tool_args);
        Some(code.clamp(0, 255) as u8)
    }
    #[cfg(not(any(feature = "coreutils", feature = "diffutils")))]
    {
        eprintln!(
            "ral: built without 'coreutils' or 'diffutils' feature; cannot run as uutils helper"
        );
        Some(2)
    }
}

/// Helper-subprocess dispatch.  Diffutils tools (`cmp`, `diff`) are
/// matched first since they're a tiny set; anything else falls through
/// to coreutils.  Each branch is feature-gated, so a build with only
/// `diffutils` (or only `coreutils`) compiles down to a single arm.
#[cfg(any(feature = "coreutils", feature = "diffutils"))]
fn uutils_invoke(tool: &str, args: Vec<std::ffi::OsString>) -> i32 {
    #[cfg(feature = "diffutils")]
    {
        match tool {
            "cmp" => return cmp_main(args.into_iter()),
            "diff" => return diff_main(args.into_iter()),
            _ => {}
        }
    }
    #[cfg(feature = "coreutils")]
    {
        return coreutils_invoke(tool, args);
    }
    #[cfg(not(feature = "coreutils"))]
    {
        let _ = (tool, args);
        1
    }
}

#[cfg(feature = "coreutils")]
declare_coreutils! {
    "ls" => uu_ls,
    "cat" => uu_cat,
    "wc" => uu_wc,
    "head" => uu_head,
    "tail" => uu_tail,
    "cp" => uu_cp,
    "cut" => uu_cut,
    "mkdir" => uu_mkdir,
    "mv" => uu_mv,
    "rm" => uu_rm,
    "sort" => uu_sort,
    "tee" => uu_tee,
    "touch" => uu_touch,
    "tr" => uu_tr,
    "uniq" => uu_uniq,
    "yes" => uu_yes,
    "basename" => uu_basename,
    "comm" => uu_comm,
    "date" => uu_date,
    "df" => uu_df,
    "dirname" => uu_dirname,
    "du" => uu_du,
    "env" => uu_env,
    "join" => uu_join,
    "ln" => uu_ln,
    "paste" => uu_paste,
    "printf" => uu_printf,
    "sleep" => uu_sleep,
    "arch" => uu_arch,
    "b2sum" => uu_b2sum,
    "base32" => uu_base32,
    "base64" => uu_base64,
    "basenc" => uu_basenc,
    "cksum" => uu_cksum,
    "csplit" => uu_csplit,
    "dd" => uu_dd,
    "dir" => uu_dir,
    "dircolors" => uu_dircolors,
    "expand" => uu_expand,
    "expr" => uu_expr,
    "factor" => uu_factor,
    "fmt" => uu_fmt,
    "fold" => uu_fold,
    "hostname" => uu_hostname,
    "link" => uu_link,
    "md5sum" => uu_md5sum,
    "mktemp" => uu_mktemp,
    "nl" => uu_nl,
    "nproc" => uu_nproc,
    "numfmt" => uu_numfmt,
    "od" => uu_od,
    "pr" => uu_pr,
    "printenv" => uu_printenv,
    "ptx" => uu_ptx,
    "pwd" => uu_pwd,
    "readlink" => uu_readlink,
    "realpath" => uu_realpath,
    "rmdir" => uu_rmdir,
    "sha1sum" => uu_sha1sum,
    "sha224sum" => uu_sha224sum,
    "sha256sum" => uu_sha256sum,
    "sha384sum" => uu_sha384sum,
    "sha512sum" => uu_sha512sum,
    "shred" => uu_shred,
    "shuf" => uu_shuf,
    "sum" => uu_sum,
    "sync" => uu_sync,
    "tac" => uu_tac,
    "test" => uu_test,
    "truncate" => uu_truncate,
    "tsort" => uu_tsort,
    "uname" => uu_uname,
    "unexpand" => uu_unexpand,
    "unlink" => uu_unlink,
    "vdir" => uu_vdir,
    "whoami" => uu_whoami
}

/// `cmp` shim, dispatched by the helper subprocess when the parent's
/// `resolve_command` rewrites a bare `cmp` to `--ral-uutils-helper cmp`.
/// Argv layout matches `parse_params`'s expectation: argv[0] is the tool
/// name, argv[1..] are user arguments.  Faithful translation of upstream
/// `diffutilslib::cmp::main` (`src/cmp.rs:476`), with two structural
/// divergences forced by upstream's API:
///
///   * No same-file/both-stdin shortcut.  Upstream's `main` checks
///     `params.from == "-" && params.to == "-"
///      || same_file::is_same_file(&params.from, &params.to)` and returns
///     SUCCESS without re-reading.  `cmp::Params.from` and `params.to`
///     are private, so we cannot replicate the test; `cmp::cmp` re-does
///     the I/O and reports `Equal`, giving the same exit code at higher
///     I/O cost.
///   * No `--quiet` suppression.  Upstream's `main` skips the `eprintln!`
///     under `params.quiet`; that field is also private.  We always
///     emit the error.
///
/// Bump diffutils → re-audit this function against the new `cmp::main`.
#[cfg(feature = "diffutils")]
fn cmp_main<I: Iterator<Item = std::ffi::OsString>>(args: I) -> i32 {
    use diffutilslib::cmp::{self, Cmp};
    let params = match cmp::parse_params(args.peekable()) {
        Ok(param) => param,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    match cmp::cmp(&params) {
        Ok(Cmp::Equal) => 0,
        Ok(Cmp::Different) => 1,
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

/// `diff` shim, line-for-line translation of upstream `diff::main`
/// (`src/diff.rs:21` in `diffutils-0.5.0`).  Upstream's `diff::main`
/// lives in the binary crate (not the library), so it cannot be called
/// directly; this is the closest we can get.
///
/// `params::Params` exposes its fields as `pub`, so unlike `cmp_main`
/// the only divergences are surface ones: the helper subprocess returns
/// `i32` rather than `ExitCode`, and `Format::Ed` errors return 2
/// directly instead of going through `std::process::exit(2)` (the helper
/// caller clamps the value to a `u8` exit code anyway).
///
/// Bump diffutils → re-audit this function against the new `diff::main`.
#[cfg(feature = "diffutils")]
fn diff_main<I: Iterator<Item = std::ffi::OsString>>(args: I) -> i32 {
    use diffutilslib::params::{parse_params, Format};
    use diffutilslib::utils::report_failure_to_read_input_file;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, stdout, Read, Write};
    let params = match parse_params(args.peekable()) {
        Ok(p) => p,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let maybe_report_identical_files = || {
        if params.report_identical_files {
            println!(
                "Files {} and {} are identical",
                params.from.to_string_lossy(),
                params.to.to_string_lossy(),
            );
        }
    };
    if params.from == "-" && params.to == "-"
        || same_file::is_same_file(&params.from, &params.to).unwrap_or(false)
    {
        maybe_report_identical_files();
        return 0;
    }

    fn read_file_contents(filepath: &OsString) -> io::Result<Vec<u8>> {
        if filepath == "-" {
            let mut content = Vec::new();
            io::stdin().read_to_end(&mut content).and(Ok(content))
        } else {
            fs::read(filepath)
        }
    }
    let mut io_error = false;
    let from_content = match read_file_contents(&params.from) {
        Ok(c) => c,
        Err(e) => {
            report_failure_to_read_input_file(&params.executable, &params.from, &e);
            io_error = true;
            vec![]
        }
    };
    let to_content = match read_file_contents(&params.to) {
        Ok(c) => c,
        Err(e) => {
            report_failure_to_read_input_file(&params.executable, &params.to, &e);
            io_error = true;
            vec![]
        }
    };
    if io_error {
        return 2;
    }

    let result: Vec<u8> = match params.format {
        Format::Normal => diffutilslib::normal_diff(&from_content, &to_content, &params),
        Format::Unified => diffutilslib::unified_diff(&from_content, &to_content, &params),
        Format::Context => diffutilslib::context_diff(&from_content, &to_content, &params),
        Format::Ed => diffutilslib::ed_diff(&from_content, &to_content, &params).unwrap_or_else(
            |error| {
                eprintln!("{error}");
                std::process::exit(2);
            },
        ),
        Format::SideBySide => {
            let mut output = stdout().lock();
            diffutilslib::side_by_side_diff(&from_content, &to_content, &mut output, &params)
        }
    };
    if params.brief && !result.is_empty() {
        println!(
            "Files {} and {} differ",
            params.from.to_string_lossy(),
            params.to.to_string_lossy()
        );
    } else {
        io::stdout().write_all(&result).unwrap();
    }
    if result.is_empty() {
        maybe_report_identical_files();
        0
    } else {
        1
    }
}
