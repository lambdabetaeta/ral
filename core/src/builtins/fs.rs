use crate::types::*;
use fs_err as fs;
#[cfg(feature = "grep")]
use grep::regex::RegexMatcherBuilder;
#[cfg(feature = "grep")]
use grep::searcher::{SearcherBuilder, sinks::UTF8};
use std::path::PathBuf;

#[cfg(feature = "grep")]
use super::util::{as_list, regex_err};
use super::util::{arg0_str, check_arity, sig, value_to_json};

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

pub(super) fn builtin_write_json(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "write-json")?;
    let path = args[0].to_string();
    let resolved = checked_write_path(shell, &path)?;
    let json = value_to_json(&args[1]);
    let s = serde_json::to_string_pretty(&json).map_err(|e| sig(format!("write-json: {e}")))?;
    atomic_write(&resolved, s.as_bytes()).map_err(|e| sig(format!("write-json: {e}")))?;
    Ok(Value::Unit)
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
        "read" => {
            let path = checked_read_path(shell, &need_path("read")?)?;
            Ok(Value::String(
                fs::read_to_string(&path).map_err(|e| sig(format!("read-file: {e}")))?,
            ))
        }
        "lines" => {
            let path = checked_read_path(shell, &need_path("lines")?)?;
            Ok(Value::Int(
                fs::read_to_string(&path)
                    .map_err(|e| sig(format!("line-count: {e}")))?
                    .lines()
                    .count() as i64,
            ))
        }
        "empty" => {
            let path = checked_read_path(shell, &need_path("empty")?)?;
            let meta = fs::metadata(&path).map_err(|e| sig(format!("file-empty: {e}")))?;
            let empty = if meta.is_dir() {
                fs::read_dir(&path)
                    .map(|mut d| d.next().is_none())
                    .map_err(|e| sig(format!("file-empty: {e}")))?
            } else {
                meta.len() == 0
            };
            Ok(Value::Bool(empty))
        }
        "size" => {
            let path = checked_read_path(shell, &need_path("size")?)?;
            Ok(Value::Int(
                fs::metadata(&path)
                    .map_err(|e| sig(format!("file-size: {e}")))?
                    .len() as i64,
            ))
        }
        "mtime" => {
            let path = checked_read_path(shell, &need_path("mtime")?)?;
            let m = fs::metadata(&path)
                .map_err(|e| sig(format!("file-mtime: {e}")))?
                .modified()
                .map_err(|e| sig(format!("file-mtime: {e}")))?;
            Ok(Value::Int(
                m.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
            ))
        }
        "write" => {
            check_arity(args, 3, "_fs 'write'")?;
            let path = checked_write_path(shell, &need_path("write")?)?;
            atomic_write(&path, args[2].to_string().as_bytes())
                .map_err(|e| sig(format!("write-file: {e}")))?;
            Ok(Value::Unit)
        }
        "copy" => {
            check_arity(args, 3, "_fs 'copy'")?;
            let src_path = checked_read_path(shell, &need_path("copy")?)?;
            let dest_path = checked_write_path(shell, &args[2].to_string())?;
            fs::copy(&src_path, &dest_path).map_err(|e| sig(format!("copy-file: {e}")))?;
            Ok(Value::Unit)
        }
        "rename" => {
            check_arity(args, 3, "_fs 'rename'")?;
            let src_path = checked_write_path(shell, &need_path("rename")?)?;
            let dest_path = checked_write_path(shell, &args[2].to_string())?;
            fs::rename(&src_path, &dest_path).map_err(|e| sig(format!("move-file: {e}")))?;
            Ok(Value::Unit)
        }
        "remove" => {
            let p = need_path("remove")?;
            let path = checked_write_path(shell, &p)?;
            if path.is_dir() {
                fs::remove_dir_all(&path).map_err(|e| sig(format!("remove: {e}")))?;
            } else {
                fs::remove_file(&path).map_err(|e| sig(format!("remove: {e}")))?;
            }
            Ok(Value::Unit)
        }
        "mkdir" => {
            let path = checked_write_path(shell, &need_path("mkdir")?)?;
            fs::create_dir_all(&path).map_err(|e| sig(format!("make-dir: {e}")))?;
            Ok(Value::Unit)
        }
        "list" => {
            let p = need_path("list")?;
            let path = checked_read_path(shell, &p)?;
            let mut entries = Vec::new();
            for entry in fs::read_dir(&path).map_err(|e| sig(format!("list-dir: {e}")))? {
                let entry = entry.map_err(|e| sig(format!("list-dir: {p}: {e}")))?;
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
        let file = fs::File::open(&resolved).map_err(|e| sig(format!("grep-files: {e}")))?;
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

pub(super) fn builtin_glob(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let pattern = checked_read_path(shell, &arg0_str(args))?
        .to_string_lossy()
        .into_owned();
    let mut results = Vec::new();
    match glob::glob(&pattern) {
        Ok(paths) => {
            for entry in paths.flatten() {
                results.push(Value::String(entry.to_string_lossy().into_owned()));
            }
        }
        Err(e) => return Err(sig(format!("glob: {e}"))),
    }
    results.sort_by_key(|a| a.to_string());
    Ok(Value::List(results))
}

/// Atomically replace `target`'s contents with `contents` using the
/// standard "same-directory tmp + flush + rename" recipe.  Either
/// readers see the old contents or the new contents — never a
/// half-written file, even across power loss or kernel panic.
///
/// # The recipe, and what each step buys you
///
/// 1. **Resolve symlinks via `canonicalize`.**  Without this, writing
///    to a symlink would replace the link itself with a regular file
///    — silently breaking anyone else who held the link as a path.
///    For new files (no canonical form yet) we fall through to the
///    literal path.
///
/// 2. **Place the tmp file in the same directory as the target.**
///    `rename(2)` is only atomic within a single filesystem; cross-fs
///    rename returns `EXDEV` and the recipe fails outright.  Using
///    `/tmp` for the staging file would break on every machine where
///    `/tmp` is a separate mount (which is most of them, post-tmpfs).
///
/// 3. **Use a cryptographically random tmp name.**  A predictable
///    name (e.g. `target.tmp`) races with concurrent writers and
///    invites symlink-attack-shaped surprises in shared dirs.
///    `tempfile::Builder` picks the name with `O_EXCL` semantics and
///    RAII-deletes the file if we error out before persisting, so we
///    never leak `.tmp` detritus.
///
/// 4. **Write the contents, then flush them to disk before rename.**
///    Skip this and a power loss in the window between rename and
///    background data flush leaves a renamed-but-zero-length file:
///    the directory entry was committed, the data blocks weren't.
///    This is the ext4 `data=ordered` bug from 2009 that ate config
///    files all over the Linux desktop (see "Further reading").
///
/// 5. **Copy the target's existing mode onto the tmp before rename
///    (or default to 0644 for new files).**  Skip this and a
///    previously-0600 sensitive file silently becomes 0600 with the
///    *new* owner-only contents — or, on platforms where the default
///    differs, becomes world-readable.  Either way the resulting
///    permissions don't match what the user had before.
///
/// 6. **Atomic `rename(tmp, target)`.**  Skip this in favour of
///    "truncate target, write" and you reintroduce exactly the bug
///    we're fixing: a crash or `^C` mid-write leaves a half-written
///    file with no recovery path.  Skip it for "copy then unlink"
///    and there's a window where the target doesn't exist; readers
///    racing the swap see `ENOENT`.
///
/// 7. **Best-effort flush the parent directory to disk.**  Skip this and
///    a kernel panic right after the rename can roll back the
///    directory entry: the data is on disk under the tmp name, but
///    on next boot the rename appears never to have happened.
///    Errors here don't roll back the rename, so we ignore them;
///    on platforms that don't support directory-level flush (Windows)
///    the open itself fails and we fall through silently.
///
/// # Things this deliberately does *not* handle
///
/// - **Hardlink fan-out breaks.**  Atomic rename creates a fresh
///   inode; any other names that pointed at the old inode keep its
///   old contents.  Every editor accepts this tradeoff — preserving
///   the inode would mean truncate-and-write, which is non-atomic.
///   Pick one.
/// - **Owner/group, xattrs, ACLs, SELinux contexts** are not copied
///   over.  Kernel default-inheritance handles the common case;
///   explicit copying belongs in a separate "preserve" path if it's
///   ever needed.
/// - **Concurrent writers** race normally — atomic rename gives
///   crash safety, not mutual exclusion.  Last writer wins.
///
/// # New failure mode vs. the old `fs::write`
///
/// Atomic rename requires write permission on the *parent
/// directory*, not just the target file.  Callers that previously
/// relied on file-only write perms will now see `EACCES` here.
/// This is the standard tradeoff for crash safety.
///
/// # Platform notes
///
/// On Linux/macOS the recipe is exact: `rename(2)` swaps the inode
/// even if other processes have the target open, and the open
/// handles keep reading the old inode until close.
///
/// On Windows, `tempfile::persist` calls
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which is atomic on NTFS
/// in the no-observable-intermediate-state sense — but it *fails*
/// (rather than silently swapping) if another process has the target
/// open with a sharing mode that excludes deletion.  Live readers
/// (editors, tail-followers, AV scanners) therefore turn what would
/// be a silent success on Linux into a hard error on Windows.
/// Mode preservation (step 5) and directory-level flush (step 7) are
/// no-ops on Windows: the former because the POSIX permission
/// bitfield doesn't apply to NTFS DACLs, the latter because NTFS
/// commits directory-entry changes via its own journal and there's
/// no API to force it from a regular file handle.  If those
/// divergences ever matter, switch the Windows path to `ReplaceFileW`
/// (preserves target DACLs and offers an optional backup) or, on
/// Windows 10 v1607+, `FileRenameInfoEx` with
/// `FILE_RENAME_FLAG_POSIX_SEMANTICS` (Linux-equivalent rename
/// over live readers).
///
/// # Further reading
///
/// - [`rename(2)`](https://man7.org/linux/man-pages/man2/rename.2.html)
///   — atomicity guarantees and the same-filesystem constraint.
/// - [`fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html)
///   — what gets flushed; why the directory-level flush matters.
/// - [Theodore Ts'o, "Don't fear the fsync!"](https://lwn.net/Articles/322823/)
///   — the ext4 `data=ordered` zero-length-after-rename saga that
///   forced flush-before-rename into common practice.
/// - [Pillai et al., "All File Systems Are Not Created Equal"](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/pillai)
///   — academic survey showing how often application code gets
///   crash-safe updates wrong.  This recipe passes their checks.
/// - [Dan Luu, "Files are hard"](https://danluu.com/file-consistency/)
///   — accessible overview of the surprising failure modes.
fn atomic_write(target: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    // (1) Symlink resolution.  `canonicalize` errors on non-existent
    // paths — for a fresh file the literal target is the right thing.
    let target = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());

    // (2) Same-directory tmp.  `Path::parent` returns `Some("")` for
    // a bare filename; treat empty as "current directory" so the
    // tempfile call doesn't choke.
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));

    // (3) Random tmp name with RAII-cleanup on early return.  Dot
    // prefix hides it from `ls`; `O_EXCL` semantics inside.
    let mut tmp = tempfile::Builder::new()
        .prefix(".")
        .suffix(".ral-write.tmp")
        .tempfile_in(parent)?;

    // (4) Write data and durably commit it before any directory
    // entry change can be observed.
    tmp.write_all(contents)?;
    tmp.as_file().sync_all()?;

    // (5) Mode preservation — Unix only; Windows has no analogue and
    // tempfile's default is already reasonable there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o644);
        let mut perms = tmp.as_file().metadata()?.permissions();
        perms.set_mode(mode);
        tmp.as_file().set_permissions(perms)?;
    }

    // (6) Atomic rename.  `tempfile::persist` calls `rename(2)`; on
    // success the tmp's RAII cleanup is disarmed automatically.
    tmp.persist(&target).map_err(|e| e.error)?;

    // (7) Best-effort directory-level flush.  Errors don't unwind
    // the rename, so we eat them; platforms without directory-level
    // flush (Windows) just fail to open the dir as a regular file
    // here.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

fn checked_read_path(shell: &mut Shell, path: &str) -> Result<PathBuf, EvalSignal> {
    shell.check_fs_read(path)?;
    Ok(shell.resolve_path(path))
}

fn checked_write_path(shell: &mut Shell, path: &str) -> Result<PathBuf, EvalSignal> {
    shell.check_fs_write(path)?;
    Ok(shell.resolve_path(path))
}

fn dir_entry_info(entry: fs::DirEntry) -> Result<DirEntryInfo, EvalSignal> {
    let name = entry.file_name().to_string_lossy().into_owned();
    let file_type = entry
        .file_type()
        .map_err(|e| sig(format!("list-dir: {name}: {e}")))?;
    let meta = entry
        .metadata()
        .map_err(|e| sig(format!("list-dir: {name}: {e}")))?;
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
