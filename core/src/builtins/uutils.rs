#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "grep"))]
use crate::diagnostic;
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "grep"))]
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
    let code = shell.io.stdout.with_child_stdout(|| invoke(uargs));
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

#[cfg(feature = "grep")]
pub(crate) fn uu_grep(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    use grep::regex::RegexMatcherBuilder;
    use grep::searcher::{SearcherBuilder, sinks::UTF8};
    let strs: Vec<String> = args.iter().map(|v| v.to_string()).collect();
    let mut invert = false;
    let mut ignore_case = false;
    let mut fixed = false;
    let mut count_only = false;
    let mut line_numbers = false;
    let mut quiet = false;
    let mut pattern: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    let mut i = 0;
    while i < strs.len() {
        if strs[i].starts_with('-') && strs[i].len() > 1 && !strs[i].starts_with("--") {
            // Walk the cluster; `-e` consumes the next argv and ends the cluster.
            let mut consumed_e = false;
            for ch in strs[i][1..].chars() {
                match ch {
                    'v' => invert = true,
                    'i' => ignore_case = true,
                    'F' => fixed = true,
                    'c' => count_only = true,
                    'n' => line_numbers = true,
                    'q' => quiet = true,
                    'E' | 'G' | 'P' => {}
                    'e' => {
                        consumed_e = true;
                        break;
                    }
                    _ => {}
                }
            }
            if consumed_e {
                i += 1;
                if i < strs.len() {
                    pattern = Some(strs[i].clone());
                }
            }
        } else {
            match strs[i].as_str() {
                "--invert-match" => invert = true,
                "--ignore-case" => ignore_case = true,
                "--fixed-strings" => fixed = true,
                "--count" => count_only = true,
                "--line-number" => line_numbers = true,
                "--quiet" | "--silent" => quiet = true,
                "--extended-regexp" | "--basic-regexp" | "--perl-regexp" => {}
                "--regexp" => {
                    i += 1;
                    if i < strs.len() {
                        pattern = Some(strs[i].clone());
                    }
                }
                _ if pattern.is_none() => pattern = Some(strs[i].clone()),
                other => files.push(other.to_string()),
            }
        }
        i += 1;
    }
    let Some(pattern) = pattern else {
        diagnostic::cmd_error("grep", "no pattern given");
        shell.control.last_status = 2;
        return Err(EvalSignal::Error(Error::new("grep: no pattern given", 2)));
    };
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(ignore_case)
        .fixed_strings(fixed)
        .build(&pattern)
    {
        Ok(m) => m,
        Err(e) => {
            diagnostic::cmd_error("grep", &e.to_string());
            shell.control.last_status = 2;
            return Err(EvalSignal::Error(Error::new(format!("grep: {e}"), 2)));
        }
    };
    // line_number(true) is mandatory: the UTF8 sink always invokes its
    // closure with a line number, and `.search_reader(...)` returns
    // `Err("line numbers not enabled")` otherwise.  The display of lnum
    // is gated on the `line_numbers` flag below.
    let mut searcher = SearcherBuilder::new()
        .invert_match(invert)
        .line_number(true)
        .build();
    let multi = files.len() > 1;
    let mut matched = false;
    macro_rules! search_one {
        ($reader:expr, $name:expr) => {{
            let prefix: String = if multi {
                format!("{}:", $name)
            } else {
                String::new()
            };
            let mut count = 0u64;
            let res = searcher.search_reader(
                &matcher,
                $reader,
                UTF8(|lnum, line| {
                    count += 1;
                    matched = true;
                    if quiet {
                        return Ok(false);
                    }
                    if !count_only {
                        let l = line.trim_end_matches('\n');
                        let s = if line_numbers {
                            format!("{prefix}{lnum}:{l}")
                        } else if prefix.is_empty() {
                            l.to_string()
                        } else {
                            format!("{prefix}{l}")
                        };
                        let _ = shell.write_stdout(format!("{s}\n").as_bytes());
                    }
                    Ok(true)
                }),
            );
            crate::dbg_trace!("uu_grep", "search_reader result={:?} count={count} matched={matched}", res);
            if count_only {
                let s = format!("{prefix}{count}");
                let _ = shell.write_stdout(format!("{s}\n").as_bytes());
            }
        }};
    }
    if files.is_empty() {
        if let Some(r) = shell.io.stdin.take_pipe() {
            crate::dbg_trace!("uu_grep", "stdin=Pipe");
            search_one!(r, "<stdin>");
        } else {
            crate::dbg_trace!("uu_grep", "stdin=Terminal");
            search_one!(std::io::stdin(), "<stdin>");
        }
    } else {
        for path in &files {
            if let Err(e) = shell.check_fs_read(path) {
                diagnostic::cmd_error("grep", &e.to_string());
                continue;
            }
            match std::fs::File::open(path) {
                Ok(f) => search_one!(f, path.as_str()),
                Err(e) => diagnostic::cmd_error("grep", &format!("{path}: {e}")),
            }
        }
    }
    let code = if matched { 0 } else { 1 };
    shell.control.last_status = code;
    Ok(Value::Unit)
}
