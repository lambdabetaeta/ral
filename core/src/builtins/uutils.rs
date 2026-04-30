#[cfg(feature = "diffutils")]
use crate::diagnostic;
#[cfg(feature = "diffutils")]
use crate::types::*;

#[cfg(feature = "diffutils")]
use diffutilslib;

#[cfg(feature = "coreutils")]
macro_rules! declare_uutils {
    ($($name:literal => $module:ident),+ $(,)?) => {
        $(use $module;)+

        /// Names of every uutils tool the helper subprocess can dispatch.
        /// Read by [`is_uutils_tool`] so command resolution can route a bare
        /// `cat`/`yes`/`wc`/... through `current_exe() --ral-uutils-helper`
        /// instead of PATH's system binary.  The macro keeps this list in
        /// lockstep with the dispatch arms below — adding a tool in one
        /// place adds it in both.
        pub(crate) const UUTILS_TOOLS: &[&str] = &[$($name),+];

        fn uutils_invoke(tool: &str, args: Vec<std::ffi::OsString>) -> i32 {
            match tool {
                $($name => $module::uumain(args.into_iter()),)+
                _ => 1,
            }
        }
    };
}

/// True when `name` is one of the bundled uutils tools — the resolver
/// substitutes a re-exec of ourselves with `--ral-uutils-helper` for these,
/// so they ride through the same external-command boundary as `/usr/bin/cat`.
#[cfg(feature = "coreutils")]
pub(crate) fn is_uutils_tool(name: &str) -> bool {
    UUTILS_TOOLS.iter().any(|t| *t == name)
}

/// `coreutils` feature off → no bundled tools, so resolution falls through
/// to PATH for every name.
#[cfg(not(feature = "coreutils"))]
pub(crate) fn is_uutils_tool(_name: &str) -> bool {
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
    #[cfg(feature = "coreutils")]
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
    #[cfg(not(feature = "coreutils"))]
    {
        eprintln!("ral: built without 'coreutils' feature; cannot run as uutils helper");
        Some(2)
    }
}

#[cfg(feature = "coreutils")]
declare_uutils! {
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

#[cfg(feature = "diffutils")]
pub(crate) fn uu_diff(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    use diffutilslib::params::{self, Format};
    use std::ffi::OsString;
    use std::io::{self, Read};
    let mut uargs: Vec<OsString> = vec![OsString::from("diff")];
    uargs.extend(args.iter().map(|v| OsString::from(v.to_string())));
    let params = match params::parse_params(uargs.into_iter().peekable()) {
        Ok(p) => p,
        Err(e) => {
            diagnostic::cmd_error("diff", &e.to_string());
            shell.control.last_status = 2;
            return Err(EvalSignal::Error(Error::new(format!("diff: {e}"), 2)));
        }
    };
    let mut pipe = shell.io.stdin.take_reader();
    let mut read_input =
        |path: &OsString,
         pipe: &mut Option<crate::io::SourceReader>|
         -> io::Result<Vec<u8>> {
            if path == "-" {
                let mut buf = Vec::new();
                if let Some(r) = pipe.take() {
                    io::BufReader::new(r).read_to_end(&mut buf)?;
                } else {
                    io::stdin().read_to_end(&mut buf)?;
                }
                Ok(buf)
            } else {
                shell.check_fs_read(&path.to_string_lossy())
                    .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?;
                std::fs::read(path)
            }
        };
    let from = match read_input(&params.from, &mut pipe) {
        Ok(c) => c,
        Err(e) => {
            diagnostic::cmd_error("diff", &format!("{}: {e}", params.from.to_string_lossy()));
            shell.control.last_status = 2;
            return Err(EvalSignal::Error(Error::new(
                format!("diff: {}: {e}", params.from.to_string_lossy()),
                2,
            )));
        }
    };
    let to = match read_input(&params.to, &mut pipe) {
        Ok(c) => c,
        Err(e) => {
            diagnostic::cmd_error("diff", &format!("{}: {e}", params.to.to_string_lossy()));
            shell.control.last_status = 2;
            return Err(EvalSignal::Error(Error::new(
                format!("diff: {}: {e}", params.to.to_string_lossy()),
                2,
            )));
        }
    };
    if from == to {
        if params.report_identical_files {
            let _ = shell.write_stdout(
                format!(
                    "Files {} and {} are identical",
                    params.from.to_string_lossy(),
                    params.to.to_string_lossy()
                )
                .as_bytes(),
            );
            let _ = shell.write_stdout(b"\n");
        }
        shell.control.last_status = 0;
        return Ok(Value::Unit);
    }
    if params.brief {
        let _ = shell.write_stdout(
            format!(
                "Files {} and {} differ",
                params.from.to_string_lossy(),
                params.to.to_string_lossy()
            )
            .as_bytes(),
        );
        let _ = shell.write_stdout(b"\n");
        shell.control.last_status = 1;
        return Err(EvalSignal::Error(Error::new("diff: files differ", 1)));
    }
    let output = match params.format {
        Format::Unified => diffutilslib::unified_diff(&from, &to, &params),
        Format::Context => diffutilslib::context_diff(&from, &to, &params),
        Format::Ed => diffutilslib::ed_diff(&from, &to, &params).unwrap_or_default(),
        Format::SideBySide => {
            let mut buf: Vec<u8> = Vec::new();
            diffutilslib::side_by_side_diff(&from, &to, &mut buf, &params);
            buf
        }
        Format::Normal => diffutilslib::normal_diff(&from, &to, &params),
    };
    let _ = shell.write_stdout(&output);
    shell.control.last_status = 1;
    Err(EvalSignal::Error(Error::new("diff: files differ", 1)))
}

#[cfg(feature = "diffutils")]
pub(crate) fn uu_cmp(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    use diffutilslib::cmp::{self, Cmp};
    use std::cell::RefCell;
    use std::ffi::OsString;
    let mut uargs: Vec<OsString> = vec![OsString::from("cmp")];
    uargs.extend(args.iter().map(|v| OsString::from(v.to_string())));
    let params = match cmp::parse_params(uargs.into_iter().peekable()) {
        Ok(p) => p,
        Err(e) => {
            diagnostic::cmd_error("cmp", &e.to_string());
            shell.control.last_status = 2;
            return Err(EvalSignal::Error(Error::new(format!("cmp: {e}"), 2)));
        }
    };
    let result: RefCell<Result<Cmp, String>> = RefCell::new(Ok(Cmp::Equal));
    if let Some(ref reader) = shell.io.stdin.take_reader() {
        let _fd_lock = crate::compat::lock_stdio_redirect();
        crate::compat::with_stdin_redirected(reader, || {
            *result.borrow_mut() = cmp::cmp(&params);
            0
        });
    } else {
        *result.borrow_mut() = cmp::cmp(&params);
    }
    let code = match result.into_inner() {
        Ok(Cmp::Equal) => 0,
        Ok(Cmp::Different) => 1,
        Err(e) => {
            diagnostic::cmd_error("cmp", &e);
            2
        }
    };
    shell.control.last_status = code;
    if code == 0 {
        Ok(Value::Unit)
    } else {
        Err(EvalSignal::Error(Error::new(
            format!("cmp: exited with status {code}"),
            code,
        )))
    }
}
