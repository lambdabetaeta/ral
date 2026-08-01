//! Coreutils, diffutils and ripgrep linked into the ral binary, and the
//! exit-code protocol the two bundled-tool placements share.

/// Declare the bundled coreutils tools once — `cross`, plus a `unix` list
/// whose `uu_*` crates pull in Unix-only uucore modules and so cannot link
/// on Windows — and emit the name slices and the dispatch arms from it, so
/// a tool added to one is added to all.
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

        /// Every coreutils tool this build can dispatch on this platform.
        pub(crate) const COREUTILS_TOOLS: &[&str] = &[
            $($cname,)+
            $($(
                #[cfg(all(unix, feature = "coreutils-unix-only"))]
                $uname,
            )+)?
        ];

        /// The `unix:` names, listed on every platform — unlike their gated
        /// [`COREUTILS_TOOLS`] entries — so exarch's `drop_dead_exec_grants`
        /// can strip the grants Windows cannot honour while compiling here.
        pub const COREUTILS_UNIX_ONLY_TOOLS: &[&str] = &[
            $($( $uname, )+)?
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

/// Bundled diffutils tools, shimmed by `cmp_main` and `diff_main` below.
#[cfg(feature = "diffutils")]
pub(crate) const DIFFUTILS_TOOLS: &[&str] = &["cmp", "diff"];

/// The bundled ripgrep tool, shimmed by `rg_main` below.
#[cfg(feature = "ripgrep")]
pub(crate) const RIPGREP_TOOLS: &[&str] = &["rg"];

/// True when `name` is a bundled tool.  `command::vet` routes these to an
/// `ExecImage::BundledTool` and skips the PATH probe entirely, so the
/// in-binary implementation wins over any same-named system binary.
#[cfg_attr(
    not(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")),
    allow(
        unused_variables,
        reason = "no bundled-tool feature is active, so `name` is not consulted and the fn is a constant `false`"
    )
)]
pub(crate) fn is_uutils_tool(name: &str) -> bool {
    #[cfg(feature = "coreutils")]
    if COREUTILS_TOOLS.contains(&name) {
        return true;
    }
    #[cfg(feature = "diffutils")]
    if DIFFUTILS_TOOLS.contains(&name) {
        return true;
    }
    #[cfg(feature = "ripgrep")]
    if RIPGREP_TOOLS.contains(&name) {
        return true;
    }
    false
}

/// Restore SIGPIPE to `SIG_DFL`, once per binary at startup.
///
/// Rust's runtime ignores it before `main`, so a uucore write to a closed
/// pipe returns EPIPE and exits 1 — indistinguishable from real failure in
/// `yes | head`.
#[cfg(unix)]
pub fn init_signal_dispositions() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Clear uucore's process-global `EXIT_CODE`, which each `uumain` must be
/// entered with: the previous call may have left a non-zero code behind.
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
pub fn reset_exit_code() {
    #[cfg(feature = "coreutils")]
    uucore::error::set_exit_code(0);
}

/// Read uucore's process-global `EXIT_CODE`: a utility's error machinery
/// reports through this cell, not through `uumain`'s return value.
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

/// Bare dispatch to a bundled tool's shim.  Callers own fd redirection,
/// `EXIT_CODE` reset, panic isolation and cwd save/restore.
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

/// Run bundled `tool` here, combining `uumain`'s return with the exit-code
/// cell.  Argv slot 0 carries the tool name for every tool; [`uutils_invoke`]'s
/// `rg` arm drops it again.  The sole caller is the `ral --ral-bundled-tool`
/// child entrypoint (`try_run_bundled_tool`), which inherits its execution
/// context from the exec, so the process-global cell is this process's own.
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
pub(crate) fn invoke_bundled(tool: &str, args: &[String]) -> i32 {
    let os_args: Vec<std::ffi::OsString> = std::iter::once(std::ffi::OsString::from(tool))
        .chain(args.iter().map(std::ffi::OsString::from))
        .collect();
    reset_exit_code();
    let code = uutils_invoke(tool, os_args);
    let global = get_exit_code();
    if global == 0 { code } else { global }
}

/// `rg` shim.  `ral_ripgrep_core::run_cli` wants argv without argv[0], so
/// drop the tool-name slot [`invoke_bundled`] puts there.
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

#[cfg(all(test, feature = "coreutils"))]
mod hygiene_tests {
    use super::*;

    /// A `unix:` name is advertised when the platform and the feature both
    /// allow it, never merely because the feature is on.  Keyed on the real
    /// `cfg!` rather than a fixed platform, so it is honest on any host.
    #[test]
    fn unix_only_tools_are_advertised_iff_unix_and_feature_on() {
        for name in COREUTILS_UNIX_ONLY_TOOLS {
            assert_eq!(
                COREUTILS_TOOLS.contains(name),
                cfg!(all(unix, feature = "coreutils-unix-only")),
                "{name} advertised-set mismatch"
            );
        }
    }
}

/// `cmp` shim, translating upstream `diffutilslib::cmp::main` minus two
/// things its private `Params` fields put out of reach: the same-file /
/// both-stdin shortcut (we re-read and report `Equal` — same exit code,
/// more I/O) and `--quiet` suppression of the error line.  Re-audit
/// against `cmp::main` when diffutils is bumped.
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

/// `diff` shim.  Upstream's `diff::main` lives in the diffutils binary
/// crate, out of a library's reach, so it is transcribed here; `Params` is
/// fully public, so unlike `cmp_main` this loses nothing — it only returns
/// `i32` rather than `ExitCode`, and a `Format::Ed` error returns 2 instead
/// of killing the process.  Re-audit against `diff::main` on a bump.
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
        reason = "[io-door:silent:diff-read] Internal byte-read of the bundled `diff` tool's compared file; the bundled exec is surfaced once at the exec door (command::run) and this internal shuffling rides that visible call, raising no separate read card."
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
