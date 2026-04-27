#[cfg(any(feature = "coreutils", feature = "diffutils"))]
use crate::diagnostic;
#[cfg(any(feature = "coreutils", feature = "diffutils"))]
use crate::types::*;

#[cfg(feature = "diffutils")]
use diffutilslib;

#[cfg(feature = "coreutils")]
macro_rules! declare_uutils {
    ($($name:literal => $module:ident),+ $(,)?) => {
        $(use $module;)+

        fn uutils_invoke(tool: &str, args: Vec<std::ffi::OsString>) -> i32 {
            match tool {
                $($name => $module::uumain(args.into_iter()),)+
                _ => 1,
            }
        }
    };
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
    "seq" => uu_seq,
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

#[cfg(feature = "coreutils")]
pub(crate) fn uutils(tool: &str, args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    use std::ffi::OsString;
    let mut uargs: Vec<OsString> = vec![OsString::from(tool)];
    uargs.extend(args.iter().map(|v| OsString::from(v.to_string())));
    let pipe_stdin = shell.io.stdin.take_pipe();
    let invoke = |uargs: Vec<OsString>| {
        if let Some(ref reader) = pipe_stdin {
            crate::compat::with_stdin_redirected(reader, || uutils_invoke(tool, uargs))
        } else {
            uutils_invoke(tool, uargs)
        }
    };
    // dup2 of fd 0/1 mutates global state; serialize against any other thread
    // doing the same.  Lock is held across the entire redirect/run/restore
    // window (both stdout and the inner stdin redirect).
    let _fd_lock = crate::compat::lock_stdio_redirect();
    let code = shell.io.stdout.with_child_stdout(|| invoke(uargs));
    drop(_fd_lock);
    shell.control.last_status = code;
    if code == 0 {
        Ok(Value::Unit)
    } else {
        Err(EvalSignal::Error(Error::new(
            format!("{tool}: exited with status {code}"),
            code,
        )))
    }
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
    let mut pipe = shell.io.stdin.take_pipe();
    let mut read_input =
        |path: &OsString, pipe: &mut Option<os_pipe::PipeReader>| -> io::Result<Vec<u8>> {
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
    if let Some(ref reader) = shell.io.stdin.take_pipe() {
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
