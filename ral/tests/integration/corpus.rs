//! The golden corpus: every program under `tests/` and `examples/`, judged by
//! the directory it sits in, one test per judgment.
//!
//! - `tests/reject/**` must be refused by `ral --check` (status 1 for a type
//!   error, 2 for a syntax error — never a crash), in words a first-year reader
//!   can follow.  A type or parse error aborts a script, so it cannot be
//!   goldened in a run.
//! - `tests/accept/**` and `examples/**` must pass `ral --check`: programs
//!   that cannot or should not run, yet must be well typed.  They take no
//!   directives.
//! - `tests/run/**` is run as `ral <program>` from a fresh empty directory,
//!   with stdin from the sibling `<name>.in` if it exists, else null, and must
//!   reproduce the sibling `<name>.out` on stdout byte for byte.  Programs
//!   under a `unix` directory run only on Unix.  `RAL_BLESS=1` rewrites each
//!   `.out` from stdout (never a header, never an `.in`); the directives are
//!   still checked, against stderr with the program's path removed.
//!
//! Directives are the program's leading `#` lines.  A line `# key: value` with
//! a key of one word is a directive; any other `#` line is prose, so a prose
//! line must have a space before its first colon.
//!
//! ```text
//! # codes: T0050 T0020      reject: exact ordered diagnostic codes
//! # at: 2:5                 reject: LINE:COL of the first diagnostic, header included
//! # status: 1               exact exit status (default: reject 1 or 2, run 0)
//! # contains: <fragment>    must appear in the report (reject) or stderr (run); repeatable
//! # absent: <fragment>      must not appear there; repeatable
//! ```
//!
//! `# contains: Help:` asks for a diagnostic that carries guidance.  Helper
//! modules sit in a `lib/` directory beside the programs, which discovery does
//! not enter.  `reject` and `accept` run each program from its own directory.

use crate::common::{self, Output};
use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// A script that outlives this is hung, not slow.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// Internal vocabulary a rendered report must not contain: Rust `Debug`
/// give-aways, and the implementation's words for itself.  Error codes and the
/// Greek letters that name type variables are deliberately allowed.
const JARGON_FRAGMENTS: &[&str] = &[
    "ParseError {",
    "LexError {",
    "LexErrorKind::",
    "Token::",
    "Word::",
    "StringPart::",
    "Ast::",
    "called `Option::unwrap",
    "panicked at",
    "deref form",
    "deref in expression",
    "IDENT)",
    " IDENT ",
    "an IDENT",
    "CompTy",
    "TyVar",
    "RowVar",
    "Box<",
    "unifier",
    "union-find",
    "occurs check",
    "Variant(",
    "Ty::",
    "Row::",
    "Cmd ",
];

#[derive(Default)]
struct Expect {
    codes: Option<Vec<String>>,
    status: Option<i32>,
    at: Option<String>,
    contains: Vec<String>,
    absent: Vec<String>,
}

/// The directives in the leading `#` lines of `source`.
fn expectations(source: &str) -> Result<Expect, String> {
    let mut expect = Expect::default();
    for line in source.lines().take_while(|l| l.starts_with('#')) {
        let line = line.trim_start_matches('#').trim();
        if let Some(codes) = line.strip_prefix("codes:") {
            expect.codes = Some(codes.split_whitespace().map(str::to_owned).collect());
        } else if let Some(status) = line.strip_prefix("status:") {
            let status = status.trim();
            expect.status = Some(
                status
                    .parse()
                    .map_err(|_| format!("`# status:` takes an integer, got {status:?}"))?,
            );
        } else if let Some(at) = line.strip_prefix("at:") {
            expect.at = Some(at.trim().to_owned());
        } else if let Some(fragment) = line.strip_prefix("contains:") {
            expect.contains.push(fragment.trim().to_owned());
        } else if let Some(fragment) = line.strip_prefix("absent:") {
            expect.absent.push(fragment.trim().to_owned());
        } else if let Some((key, _)) = line.split_once(':')
            && !key.contains(char::is_whitespace)
        {
            return Err(format!(
                "unknown directive `# {key}:` (known: codes, status, at, contains, absent; \
                 a prose line needs a space before its first colon)"
            ));
        }
    }
    Ok(expect)
}

/// The faults of `text`, named `what`, against the `contains`/`absent` directives.
fn fragment_faults<'a>(
    expect: &'a Expect,
    text: &'a str,
    what: &'a str,
) -> impl Iterator<Item = String> + 'a {
    let lacking = (expect.contains.iter())
        .filter(move |fragment| !text.contains(fragment.as_str()))
        .map(move |fragment| format!("{what} lacks {fragment:?}"));
    let containing = (expect.absent.iter())
        .filter(move |fragment| text.contains(fragment.as_str()))
        .map(move |fragment| format!("{what} contains {fragment:?}"));
    lacking.chain(containing)
}

/// The diagnostic codes a report opens its entries with, in order.
fn codes(report: &str) -> Vec<&str> {
    report
        .lines()
        .filter_map(|line| {
            let code = line.strip_prefix('[')?.split_once("] ")?.0;
            let (kind, digits) = code.split_at_checked(1)?;
            (kind.bytes().all(|b| b.is_ascii_uppercase())
                && digits.len() == 4
                && digits.bytes().all(|b| b.is_ascii_digit()))
            .then_some(code)
        })
        .collect()
}

struct Checked {
    status: i32,
    /// The report, with the program's own file name removed so that a fragment
    /// cannot match the path it was found at.
    report: String,
}

fn check(path: &Path) -> Checked {
    let name = path.file_name().unwrap().to_string_lossy();
    let out = common::ral_command()
        .arg("--check")
        .arg(&*name)
        .current_dir(path.parent().unwrap())
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", path.display()));
    Checked {
        status: out.status.code().unwrap_or(-1),
        report: String::from_utf8_lossy(&out.stderr).replace(&*name, ""),
    }
}

/// What is wrong with how `path` was refused, if anything.
fn reject_faults(path: &Path) -> Vec<String> {
    let expect = match expectations(&std::fs::read_to_string(path).unwrap()) {
        Ok(expect) => expect,
        Err(fault) => return vec![fault],
    };
    let Checked { status, report } = check(path);
    let mut faults = Vec::new();

    let wanted = expect.status.map_or(1..=2, |s| s..=s);
    if !wanted.contains(&status) {
        faults.push(format!(
            "expected status {wanted:?} (1 type error, 2 syntax error), got {status}"
        ));
    }
    // The first locus line ends `file:LINE:COL ]`, the file name being stripped.
    if let Some(at) = &expect.at
        && !report
            .lines()
            .find(|l| l.contains("╭─["))
            .is_some_and(|l| l.ends_with(&format!(":{at} ]")))
    {
        faults.push(format!("first diagnostic is not at {at}"));
    }
    if let Some(want) = &expect.codes {
        let got = codes(&report);
        if got != *want {
            faults.push(format!("expected codes {want:?}, got {got:?}"));
        }
    }
    faults.extend(fragment_faults(&expect, &report, "report"));
    faults.extend(
        JARGON_FRAGMENTS
            .iter()
            .filter(|jargon| report.contains(**jargon))
            .map(|jargon| format!("report contains internal jargon {jargon:?}")),
    );
    if faults.is_empty() {
        return faults;
    }
    faults.push(format!("report was:\n{report}"));
    faults
}

fn accept_faults(path: &Path) -> Vec<String> {
    let Checked { status, report } = check(path);
    if status == 0 {
        return Vec::new();
    }
    vec![format!(
        "expected status 0, got {status}; report was:\n{report}"
    )]
}

/// Run `path` from a fresh empty directory; `None` if it hung and was killed.
fn execute(path: &Path) -> Option<Output> {
    let stdin = match File::open(path.with_extension("in")) {
        Ok(file) => Stdio::from(file),
        Err(e) if e.kind() == ErrorKind::NotFound => Stdio::null(),
        Err(e) => panic!("read {}: {e}", path.with_extension("in").display()),
    };
    let cwd = tempfile::tempdir().unwrap();
    let child = common::ral_command()
        .arg(path)
        .current_dir(cwd.path())
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", path.display()));
    common::wait_with_timeout(child, RUN_TIMEOUT)
}

/// On Windows git checks goldens out with CRLF while `ral` emits LF; compare
/// modulo `\r` there, byte for byte elsewhere.
fn same_bytes(expected: &[u8], actual: &[u8]) -> bool {
    #[cfg(unix)]
    return expected == actual;
    #[cfg(not(unix))]
    return expected
        .iter()
        .filter(|&&b| b != b'\r')
        .eq(actual.iter().filter(|&&b| b != b'\r'));
}

/// How `stdout` differs from the golden at `golden`; with `bless`, rewrites it.
fn golden_fault(golden: &Path, stdout: &str, bless: bool) -> Option<String> {
    if bless {
        std::fs::write(golden, stdout)
            .unwrap_or_else(|e| panic!("bless write {}: {e}", golden.display()));
        return None;
    }
    match std::fs::read(golden) {
        Ok(expected) if same_bytes(&expected, stdout.as_bytes()) => None,
        Ok(expected) => Some(format!(
            "stdout differs from {}\n--- golden ---\n{}\n--- actual ---\n{stdout}",
            golden.display(),
            String::from_utf8_lossy(&expected),
        )),
        Err(_) => Some(format!(
            "missing golden {} — run `RAL_BLESS=1 cargo test -p ral --test integration corpus::run`",
            golden.display(),
        )),
    }
}

/// What is wrong with how `path` ran, if anything.
fn run_faults(path: &Path) -> Vec<String> {
    let expect = match expectations(&std::fs::read_to_string(path).unwrap()) {
        Ok(expect) => expect,
        Err(fault) => return vec![fault],
    };
    let misplaced = [
        ("codes", expect.codes.is_some()),
        ("at", expect.at.is_some()),
    ];
    let mut faults: Vec<String> = misplaced
        .into_iter()
        .filter(|&(_, present)| present)
        .map(|(key, _)| format!("`# {key}:` is not a run directive"))
        .collect();
    let Some(out) = execute(path) else {
        faults.push(format!("no exit within {RUN_TIMEOUT:?}; killed"));
        return faults;
    };

    let wanted = expect.status.unwrap_or(0);
    if out.status != wanted {
        faults.push(format!("expected status {wanted}, got {}", out.status));
    }
    // As for a report: a fragment cannot match the path the program was found at.
    let stderr = out.stderr.replace(&*path.to_string_lossy(), "");
    faults.extend(fragment_faults(&expect, &stderr, "stderr"));
    let bless = std::env::var_os("RAL_BLESS").is_some();
    faults.extend(golden_fault(
        &path.with_extension("out"),
        &out.stdout,
        bless,
    ));
    if faults.is_empty() {
        return faults;
    }
    faults.push(format!("stderr was:\n{stderr}"));
    faults
}

/// Every `.ral` file under `dir`, sorted, never entering a directory named in
/// `prune`.
fn discover(dir: &Path, prune: &[&str]) -> Vec<PathBuf> {
    let mut scripts = Vec::new();
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("read_dir entry in {}: {e}", dir.display()))
            .path();
        if path.is_dir() {
            if !path
                .file_name()
                .is_some_and(|n| prune.iter().any(|p| n == *p))
            {
                scripts.extend(discover(&path, prune));
            }
        } else if path.extension().is_some_and(|e| e == "ral") {
            scripts.push(path);
        }
    }
    scripts.sort();
    scripts
}

/// Unix-specific programs do not run elsewhere.
fn applies_here(path: &Path) -> bool {
    cfg!(unix) || !path.components().any(|c| c.as_os_str() == "unix")
}

/// Run `faults` over every program under `corpus`, a directory of the workspace.
fn run_corpus(corpus: &str, faults: fn(&Path) -> Vec<String>) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(corpus);
    let programs: Vec<_> = discover(&base, &["lib"])
        .into_iter()
        .filter(|program| applies_here(program))
        .collect();
    assert!(
        !programs.is_empty(),
        "no .ral programs found in {}",
        base.display()
    );

    let failures: Vec<String> = programs
        .iter()
        .filter_map(|program| {
            let faults = faults(program);
            let name = program.strip_prefix(&base).unwrap().display();
            (!faults.is_empty()).then(|| format!("{corpus}/{name}:\n  {}", faults.join("\n  ")))
        })
        .collect();

    eprintln!(
        "{} of {} {corpus} programs ok",
        programs.len() - failures.len(),
        programs.len()
    );
    assert!(
        failures.is_empty(),
        "{} {corpus} program(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn reject() {
    run_corpus("tests/reject", reject_faults);
}

#[test]
fn accept() {
    run_corpus("tests/accept", accept_faults);
}

#[test]
fn examples() {
    run_corpus("examples", accept_faults);
}

#[test]
fn run() {
    run_corpus("tests/run", run_faults);
}
