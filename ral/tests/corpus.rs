#![allow(clippy::disallowed_methods)]

//! The static corpora.  A type or parse error aborts a script, so it cannot be
//! goldened in one; each program here is instead handed whole to `ral --check`.
//!
//! - `tests/reject/*.ral` must be refused (status 1 for a type error, 2 for a
//!   syntax error — never a crash), in words a first-year reader can follow.
//!   Leading `#` lines say what the diagnostic must be:
//!
//!   ```text
//!   # codes: T0050 T0020      exact ordered diagnostic codes
//!   # contains: <fragment>    must appear in the rendered report (repeatable)
//!   # absent: <fragment>      must not appear (repeatable)
//!   ```
//!
//!   `# contains: Help:` asks for a diagnostic that carries guidance.
//! - `tests/accept/*.ral` must pass `--check`: programs that cannot or should
//!   not run, yet must be well typed.
//!
//! Helper modules sit in a `lib/` directory beside the programs, which
//! discovery does not enter.  Each program runs with its own directory as
//! working directory.

mod common;

use std::path::Path;
use std::process::Stdio;

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
    contains: Vec<String>,
    absent: Vec<String>,
}

/// The assertions in the leading `#` lines of `source`.
fn expectations(source: &str) -> Expect {
    let mut expect = Expect::default();
    for line in source.lines().take_while(|l| l.starts_with('#')) {
        let line = line.trim_start_matches('#').trim();
        if let Some(codes) = line.strip_prefix("codes:") {
            expect.codes = Some(codes.split_whitespace().map(str::to_owned).collect());
        } else if let Some(fragment) = line.strip_prefix("contains:") {
            expect.contains.push(fragment.trim().to_owned());
        } else if let Some(fragment) = line.strip_prefix("absent:") {
            expect.absent.push(fragment.trim().to_owned());
        }
    }
    expect
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

/// What is wrong with how `program` was refused, if anything.
fn rejection_faults(path: &Path) -> Vec<String> {
    let source = std::fs::read_to_string(path).unwrap();
    let expect = expectations(&source);
    let Checked { status, report } = check(path);
    let mut faults = Vec::new();

    if !matches!(status, 1 | 2) {
        faults.push(format!(
            "expected status 1 (type error) or 2 (syntax error), got {status}"
        ));
    }
    if let Some(want) = &expect.codes {
        let got = codes(&report);
        if got != *want {
            faults.push(format!("expected codes {want:?}, got {got:?}"));
        }
    }
    for fragment in &expect.contains {
        if !report.contains(fragment.as_str()) {
            faults.push(format!("report lacks {fragment:?}"));
        }
    }
    for fragment in &expect.absent {
        if report.contains(fragment.as_str()) {
            faults.push(format!("report contains {fragment:?}"));
        }
    }
    for jargon in JARGON_FRAGMENTS {
        if report.contains(jargon) {
            faults.push(format!("report contains internal jargon {jargon:?}"));
        }
    }
    if faults.is_empty() {
        return faults;
    }
    faults.push(format!("report was:\n{report}"));
    faults
}

fn acceptance_faults(path: &Path) -> Vec<String> {
    let Checked { status, report } = check(path);
    if status == 0 {
        return Vec::new();
    }
    vec![format!(
        "expected status 0, got {status}; report was:\n{report}"
    )]
}

fn run_corpus(corpus: &str, faults: fn(&Path) -> Vec<String>) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests")
        .join(corpus);
    let programs = common::discover(&base, &["lib"]);
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
    run_corpus("reject", rejection_faults);
}

#[test]
fn accept() {
    run_corpus("accept", acceptance_faults);
}
