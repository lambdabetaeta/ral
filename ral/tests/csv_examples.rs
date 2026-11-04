#![allow(clippy::disallowed_methods, reason = "test code")]

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

fn run_example(name: &str, paths: &[&Path], args: &[&str]) -> common::Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../examples")
        .join(name)
        .join(format!("{name}.ral"));
    let child = common::ral_command()
        .arg("--norc")
        .arg(script)
        .args(paths)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CSV example");
    common::wait_with_timeout(child, Duration::from_secs(10)).expect("CSV example hung")
}

fn write_csv(path: &Path, records: &[&[&str]]) {
    let mut writer = csv::Writer::from_path(path).unwrap();
    for record in records {
        writer.write_record(*record).unwrap();
    }
    writer.flush().unwrap();
}

// These are rows, not maps: repeated output headers are part of the contract.
fn read_rows(output: &common::Output) -> Vec<Vec<String>> {
    assert_eq!(output.status, 0, "stderr: {}", output.stderr);
    csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(output.stdout.as_bytes())
        .records()
        .map(|row| row.unwrap().iter().map(str::to_owned).collect())
        .collect()
}

#[test]
fn csv_select_encodes_cells_in_requested_order_with_repeats() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.csv");
    let header = [
        "plain",
        "comma,header",
        "quote\"\"header",
        "LF\nheader",
        "CRLF\r\nheader",
        "empty",
        "unicode",
        "CR\rheader",
    ];
    let first = [
        "ordinary",
        "a,b",
        "\"one\" and \"two\"",
        "line\none\n",
        "line\r\ntwo\r\n",
        "",
        "雪🙂",
        "bare\rreturn\r",
    ];
    let second = ["", ",", "\"\"", "\n", "\r\n", "", "é", "\r"];
    write_csv(&input, &[&header, &first, &second]);
    let cols = header
        .iter()
        .rev()
        .copied()
        .chain([header[2]])
        .collect::<Vec<_>>();
    let out = run_example("csv-select", &[&input], &cols);
    let expected = [&header, &first, &second]
        .iter()
        .map(|row| {
            row.iter()
                .rev()
                .copied()
                .chain([row[2]])
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(read_rows(&out), expected);
}

#[test]
fn csv_select_header_only_keeps_requested_header_without_data_row_validation() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("header.csv");
    write_csv(&input, &[&["a", "b"]]);
    let cols = ["b", "a", "b", "missing,\"\n\r\n"];
    let out = run_example("csv-select", &[&input], &cols);
    assert_eq!(read_rows(&out), vec![cols.to_vec()]);
}

#[test]
fn csv_select_empty_selection_writes_blank_header_and_data_rows() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.csv");
    for (source, expected) in [("a,b\n1,2\n3,4\n", "\n\n\n"), ("a,b\n", "\n")] {
        std::fs::write(&input, source).unwrap();
        let out = run_example("csv-select", &[&input], &[]);
        assert_eq!(out.status, 0, "stderr: {}", out.stderr);
        // A CSV reader ignores blank lines, so only bytes can observe this case.
        assert_eq!(out.stdout.as_bytes(), expected.as_bytes());
    }
}

#[test]
fn csv_select_refuses_unknown_columns_on_the_first_data_row() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.csv");
    write_csv(&input, &[&["a", "b"], &["1", "2"]]);
    let out = run_example("csv-select", &[&input], &["missing"]);
    assert_ne!(out.status, 0);
    assert!(out.stderr.contains("no such column: missing"), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn join_csv_decodes_both_unsorted_inputs_and_encodes_only_matches() {
    let dir = tempfile::tempdir().unwrap();
    let left = dir.path().join("left.csv");
    let right = dir.path().join("right.csv");
    let key_a = "a,\n\"key\"";
    let key_b = "b\r\nkey";
    write_csv(
        &left,
        &[
            &["id", "left"],
            &[key_b, "left,\"B\"\r\n雪"],
            &["left-only", "not joined"],
            &[key_a, "left\nA\r"],
        ],
    );
    write_csv(
        &right,
        &[
            &["id", "right"],
            &[key_a, "right,\"A\"\n🙂"],
            &["right-only", "not joined"],
            &[key_b, "right\r\nB\r"],
        ],
    );
    let out = run_example("join-csv", &[&left, &right], &["id"]);
    assert_eq!(
        read_rows(&out),
        vec![
            vec!["id", "left", "right"],
            vec![key_b, "left,\"B\"\r\n雪", "right\r\nB\r"],
            vec![key_a, "left\nA\r", "right,\"A\"\n🙂"],
        ]
    );
}

#[test]
fn join_csv_refuses_invalid_csv_on_either_side() {
    let dir = tempfile::tempdir().unwrap();
    let left = dir.path().join("left.csv");
    let right = dir.path().join("right.csv");
    let invalid: &[(&[u8], &str)] = &[
        (b"id,id\na,a\n", "duplicate header"),
        (b"id,value\na\n", "header has 2"),
        (b"id,value\na,b,c\n", "header has 2"),
        (b"id,value\na,\xff\n", "UTF-8"),
    ];
    for (bytes, diagnostic) in invalid {
        for bad_side in [&left, &right] {
            write_csv(&left, &[&["id", "left"], &["a", "L"]]);
            write_csv(&right, &[&["id", "right"], &["a", "R"]]);
            std::fs::write(bad_side, bytes).unwrap();
            let out = run_example("join-csv", &[&left, &right], &["id"]);
            assert_ne!(out.status, 0, "accepted {}: {bytes:?}", bad_side.display());
            assert!(out.stderr.contains("from-csv:"), "{out:?}");
            assert!(out.stderr.contains(diagnostic), "{out:?}");
            assert!(out.stdout.is_empty(), "{out:?}");
        }
    }
}
