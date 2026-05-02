use crate::types::*;
#[cfg(feature = "grep")]
use grep::regex::RegexMatcherBuilder;
#[cfg(feature = "grep")]
use grep::searcher::{SearcherBuilder, sinks::UTF8};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "grep")]
use super::util::{as_list, regex_err};
use super::util::{arg0_str, check_arity, sig};

struct DirEntryInfo {
    name: String,
    file_type: &'static str,
    size: i64,
    mtime: i64,
}

impl DirEntryInfo {
    fn into_value(self) -> Value {
        Value::Map(vec![
            ("name".into(), Value::String(self.name)),
            ("type".into(), Value::String(self.file_type.into())),
            ("size".into(), Value::Int(self.size)),
            ("mtime".into(), Value::Int(self.mtime)),
        ])
    }
}

pub(super) fn builtin_fs(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "_fs")?;
    let op = args[0].to_string();
    // Most ops need a path; require it explicitly so a missing arg yields a
    // clean diagnostic rather than a misleading "" path error.
    let need_path = |label: &str| -> Result<String, EvalSignal> {
        match args.get(1) {
            Some(v) => Ok(v.to_string()),
            None => Err(sig(format!("_fs '{label}' requires a path argument"))),
        }
    };
    match op.as_str() {
        "lines" => {
            let path = checked_read_path(shell, &need_path("lines")?)?;
            Ok(Value::Int(
                fs::read_to_string(&path)
                    .map_err(|e| io_err("line-count", &path, e))?
                    .lines()
                    .count() as i64,
            ))
        }
        "empty" => {
            let path = checked_read_path(shell, &need_path("empty")?)?;
            let meta = fs::metadata(&path).map_err(|e| io_err("file-empty", &path, e))?;
            let empty = if meta.is_dir() {
                fs::read_dir(&path)
                    .map(|mut d| d.next().is_none())
                    .map_err(|e| io_err("file-empty", &path, e))?
            } else {
                meta.len() == 0
            };
            Ok(Value::Bool(empty))
        }
        "size" => {
            let path = checked_read_path(shell, &need_path("size")?)?;
            Ok(Value::Int(
                fs::metadata(&path)
                    .map_err(|e| io_err("file-size", &path, e))?
                    .len() as i64,
            ))
        }
        "mtime" => {
            let path = checked_read_path(shell, &need_path("mtime")?)?;
            let m = fs::metadata(&path)
                .map_err(|e| io_err("file-mtime", &path, e))?
                .modified()
                .map_err(|e| io_err("file-mtime", &path, e))?;
            Ok(Value::Int(
                m.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
            ))
        }
        "list" => {
            let path = checked_read_path(shell, &need_path("list")?)?;
            let mut entries = Vec::new();
            for entry in fs::read_dir(&path).map_err(|e| io_err("list-dir", &path, e))? {
                let entry = entry.map_err(|e| io_err("list-dir", &path, e))?;
                entries.push(dir_entry_info(entry)?);
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(Value::List(
                entries.into_iter().map(DirEntryInfo::into_value).collect(),
            ))
        }
        "tempdir" => {
            let parent = std::env::temp_dir();
            shell.check_fs_write(&parent.to_string_lossy())?;
            let path = tempfile::Builder::new()
                .prefix("ral-tmp-")
                .tempdir_in(&parent)
                .map_err(|e| sig(format!("temp-dir: {e}")))?
                .keep();
            Ok(Value::String(path.to_string_lossy().into_owned()))
        }
        "tempfile" => {
            let parent = std::env::temp_dir();
            shell.check_fs_write(&parent.to_string_lossy())?;
            let named = tempfile::Builder::new()
                .prefix("ral-tmp-")
                .tempfile_in(&parent)
                .map_err(|e| sig(format!("temp-file: {e}")))?;
            let (_file, path) = named.keep().map_err(|e| sig(format!("temp-file: {e}")))?;
            Ok(Value::String(path.to_string_lossy().into_owned()))
        }
        _ => Err(sig(format!("_fs: unknown operation '{op}'"))),
    }
}

pub(super) fn builtin_fs_pred(
    args: &[Value],
    pred: impl Fn(&std::path::Path) -> bool,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    let path = checked_read_path(shell, &arg0_str(args))?;
    let result = pred(&path);
    shell.set_status_from_bool(result);
    Ok(Value::Bool(result))
}

#[cfg(feature = "grep")]
pub(super) fn builtin_grep_files(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "grep-files")?;
    let pattern = args[0].to_string();
    let matcher = RegexMatcherBuilder::new()
        .build(&pattern)
        .map_err(|e| sig(regex_err("grep-files", &pattern, &e.to_string())))?;
    let paths = as_list(&args[1], "grep-files")?;
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut results = Vec::new();
    for arg in &paths {
        let path = arg.to_string();
        let resolved = checked_read_path(shell, &path)?;
        let file = fs::File::open(&resolved).map_err(|e| io_err("grep-files", &resolved, e))?;
        searcher
            .search_reader(
                &matcher,
                file,
                UTF8(|line_num, line| {
                    results.push(Value::Map(vec![
                        ("file".into(), Value::String(path.clone())),
                        ("line".into(), Value::Int(line_num as i64)),
                        (
                            "text".into(),
                            Value::String(line.trim_end_matches('\n').to_string()),
                        ),
                    ]));
                    Ok(true)
                }),
            )
            .map_err(|e| sig(format!("grep-files: {path}: {e}")))?;
    }
    Ok(Value::List(results))
}

/// Dispatch wrapper used by the registry: presents a uniform signature
/// regardless of whether the `grep` feature compiled in, so the macro
/// table need not carry a `cfg` gate.
pub(super) fn builtin_grep_files_dispatch(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    #[cfg(feature = "grep")]
    {
        builtin_grep_files(args, shell)
    }
    #[cfg(not(feature = "grep"))]
    {
        let _ = (args, shell);
        Err(sig(
            "grep-files: grep feature not compiled in — rebuild with --features grep",
        ))
    }
}

pub(super) fn builtin_glob(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let pattern = checked_read_path(shell, &arg0_str(args))?
        .to_string_lossy()
        .into_owned();
    let mut results = Vec::new();
    match glob::glob(&pattern) {
        Ok(paths) => {
            for entry in paths {
                match entry {
                    Ok(path) => results.push(Value::String(path.to_string_lossy().into_owned())),
                    Err(e) => return Err(sig(format!("glob: {e}"))),
                }
            }
        }
        Err(e) => return Err(sig(format!("glob: {e}"))),
    }
    results.sort_by_key(|a| a.to_string());
    Ok(Value::List(results))
}

fn checked_read_path(shell: &mut Shell, path: &str) -> Result<PathBuf, EvalSignal> {
    shell.check_fs_read(path)?;
    Ok(shell.resolve_path(path))
}

/// Wrap a `std::io::Error` with the operation label and the path that
/// triggered it.  Stand-in for `fs-err`: every fs call here knows the
/// path it was acting on, so we attach it explicitly rather than relying
/// on a wrapper type.
fn io_err(ctx: &str, path: &Path, e: std::io::Error) -> EvalSignal {
    sig(format!("{ctx}: {}: {e}", path.display()))
}

fn dir_entry_info(entry: fs::DirEntry) -> Result<DirEntryInfo, EvalSignal> {
    let name = entry.file_name().to_string_lossy().into_owned();
    let path = entry.path();
    let file_type = entry.file_type().map_err(|e| io_err("list-dir", &path, e))?;
    let meta = entry.metadata().map_err(|e| io_err("list-dir", &path, e))?;
    let file_type = if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "dir"
    } else if file_type.is_file() {
        "file"
    } else {
        "other"
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(DirEntryInfo {
        name,
        file_type,
        size: meta.len() as i64,
        mtime,
    })
}
