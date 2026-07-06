/// Declare two parallel lists of bundled coreutils tools — `cross`
/// (always available when the `coreutils` feature is on) and `unix`
/// (additionally available when `coreutils-unix-only` is on, gated to
/// `cfg(unix)` because the underlying uucore modules they pull in are
/// Unix-only).  The macro emits one merged `COREUTILS_TOOLS` slice and
/// one `coreutils_invoke` whose Unix-only arms are themselves
/// `cfg(unix)`-gated, so a Windows build with only `coreutils` active
/// links exclusively the cross-platform set.
#[cfg(feature = "coreutils")]
macro_rules! declare_coreutils {
    (
        cross: { $($cname:literal => $cmodule:ident),+ $(,)? }
        $(, unix: { $($uname:literal => $umodule:ident),+ $(,)? } )? $(,)?
    ) => {
        $(use $cmodule;)+
        $($(
            #[cfg(all(unix, feature = "coreutils-unix-only"))]
            use $umodule;
        )+)?

        /// Names of every coreutils tool the helper subprocess can dispatch.
        /// The macro derives this list and the dispatch arms below from a
        /// single declaration, so a tool added in one is added in both.
        /// Consulted by [`is_uutils_tool`] together with [`DIFFUTILS_TOOLS`].
        pub(crate) const COREUTILS_TOOLS: &[&str] = &[
            $($cname,)+
            $($(
                #[cfg(all(unix, feature = "coreutils-unix-only"))]
                $uname,
            )+)?
        ];

        pub(crate) fn coreutils_invoke(tool: &str, args: Vec<std::ffi::OsString>) -> i32 {
            match tool {
                $($cname => $cmodule::uumain(args.into_iter()),)+
                $($(
                    #[cfg(all(unix, feature = "coreutils-unix-only"))]
                    $uname => $umodule::uumain(args.into_iter()),
                )+)?
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

/// Bundled ripgrep tool — `rg`, gated on the `ripgrep` Cargo feature.
#[cfg(feature = "ripgrep")]
pub(crate) const RIPGREP_TOOLS: &[&str] = &["rg"];

/// True when `name` is one of the bundled tools — coreutils,
/// diffutils, or ripgrep.  Two callers consult this predicate:
/// `command::run` dispatches `uutils_invoke` in-process when the sinks
/// are plain Terminal/Stderr, and otherwise spawns the
/// `ral --ral-bundled-tool <tool>` exec image as an ordinary child; and
/// a bundled byte pipeline stage launches the same `--ral-bundled-tool`
/// child placement.  Both paths converge on `uutils_invoke` running in a
/// context where stdout/stderr are plain Terminal/Stderr sinks, so the
/// in-binary implementation is authoritative on every platform
/// regardless of what PATH would have turned up.
pub(crate) fn is_uutils_tool(_name: &str) -> bool {
    #[cfg(feature = "coreutils")]
    if COREUTILS_TOOLS.contains(&_name) {
        return true;
    }
    #[cfg(feature = "diffutils")]
    if DIFFUTILS_TOOLS.contains(&_name) {
        return true;
    }
    #[cfg(feature = "ripgrep")]
    if RIPGREP_TOOLS.contains(&_name) {
        return true;
    }
    false
}

/// Restore SIGPIPE to its default disposition.  Rust's runtime sets
/// SIGPIPE=IGN before main; uucore writes therefore see EPIPE and return 1
/// instead of dying from SIGPIPE.  A non-final pipeline stage that exits 1
/// is indistinguishable from a real error — `yes | head` would mis-report
/// failure.  Call this once at startup so every in-process uutils call and
/// pipeline helper benefits.
#[cfg(unix)]
pub fn init_signal_dispositions() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Reset uucore's process-global EXIT_CODE to 0.  Must be called before
/// each in-process `uumain` invocation since the previous call may have
/// left a non-zero code.  When `coreutils` is not active (only diffutils
/// or ripgrep are bundled), this is a no-op.
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
pub fn reset_exit_code() {
    #[cfg(feature = "coreutils")]
    uucore::error::set_exit_code(0);
}

/// Read uucore's process-global EXIT_CODE.  The return value of `uumain`
/// is not the exit code seen by the utility's own error machinery — the
/// true exit code is tracked in this atomic.  When `coreutils` is not
/// active, returns 0 (diffutils / ripgrep don't use the uucore exit code).
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
pub fn get_exit_code() -> i32 {
    #[cfg(feature = "coreutils")]
    {
        uucore::error::get_exit_code()
    }
    #[cfg(not(feature = "coreutils"))]
    {
        0
    }
}

/// Dispatch a bundled tool in-process.  Diffutils tools (`cmp`, `diff`)
/// are matched first since they're a tiny set; anything else falls through
/// to coreutils.  Each branch is feature-gated, so a build with only
/// `diffutils` (or only `coreutils`, or only `ripgrep`) compiles down to a
/// single arm.
///
/// Callers must handle fd redirection, EXIT_CODE reset, panic isolation,
/// and CWD save/restore.  This function is the bare dispatch.
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
pub(crate) fn uutils_invoke(tool: &str, args: Vec<std::ffi::OsString>) -> i32 {
    #[cfg(feature = "diffutils")]
    {
        match tool {
            "cmp" => return cmp_main(args.into_iter()),
            "diff" => return diff_main(args.into_iter()),
            _ => {}
        }
    }
    #[cfg(feature = "ripgrep")]
    if tool == "rg" {
        return rg_main(args.into_iter());
    }
    #[cfg(feature = "coreutils")]
    {
        coreutils_invoke(tool, args)
    }
    #[cfg(not(feature = "coreutils"))]
    {
        let _ = (tool, args);
        1
    }
}

/// `rg` shim, dispatched via [`uutils_invoke`] — from the in-process
/// fast path or from the `ral --ral-bundled-tool rg` child placement.
/// `ral-ripgrep-core` expects argv without argv[0], so we drop the
/// tool-name slot and pass through the original user arguments unchanged.
#[cfg(feature = "ripgrep")]
fn rg_main<I: Iterator<Item = std::ffi::OsString>>(mut args: I) -> i32 {
    let _argv0 = args.next();
    i32::from(ral_ripgrep_core::run_cli(args))
}

#[cfg(feature = "coreutils")]
declare_coreutils! {
    cross: {
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
        "base64" => uu_base64,
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
        "hostname" => uu_hostname,
        "mktemp" => uu_mktemp,
        "nproc" => uu_nproc,
        "printenv" => uu_printenv,
        "pwd" => uu_pwd,
        "readlink" => uu_readlink,
        "realpath" => uu_realpath,
        "rmdir" => uu_rmdir,
        "seq" => uu_seq,
        "sha256sum" => uu_sha256sum,
        "sha512sum" => uu_sha512sum,
        "shuf" => uu_shuf,
        "split" => uu_split,
        "truncate" => uu_truncate,
        "uname" => uu_uname,
        "whoami" => uu_whoami,
    },
    unix: {
        "id" => uu_id,
        "kill" => uu_kill,
        "stat" => uu_stat,
        "tac" => uu_tac,
        "test" => uu_test,
        "timeout" => uu_timeout,
    }
}

/// `cmp` shim, dispatched via [`uutils_invoke`] — from the in-process
/// fast path or from the `ral --ral-bundled-tool cmp` child placement.
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
/// directly instead of killing the process.
///
/// Bump diffutils → re-audit this function against the new `diff::main`.
#[cfg(feature = "diffutils")]
fn diff_main<I: Iterator<Item = std::ffi::OsString>>(args: I) -> i32 {
    use diffutilslib::params::{Format, parse_params};
    use diffutilslib::utils::report_failure_to_read_input_file;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Read, Write, stdout};
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

    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:diff-read] Internal byte-read of the bundled `diff` tool's compared file; the bundled exec is surfaced once at the exec door (run_uutils_in_process / command::run) and this internal shuffling rides that visible call, raising no separate read card."
    )]
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
        Format::Ed => match diffutilslib::ed_diff(&from_content, &to_content, &params) {
            Ok(v) => v,
            Err(error) => {
                eprintln!("{error}");
                return 2;
            }
        },
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
