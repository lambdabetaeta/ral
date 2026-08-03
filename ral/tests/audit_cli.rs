#![allow(clippy::disallowed_methods)]

// Integration tests for `ral --audit`: the JSON dumped to stderr must be
// parseable, and its root must be the same envelope the `audit { … }`
// builtin returns (status / value / error / children) rather than a
// synthetic command node.  `--pretty` may change the bytes, never the value.

mod common;

use common::{Output, ral_bin};
use serde_json::Value;
use std::process::Command;

fn run_c(args: &[&str], code: &str) -> Output {
    let out = Command::new(ral_bin())
        .args(args)
        .arg("-c")
        .arg(code)
        .output()
        .expect("spawn ral");
    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

/// The JSON dump, isolated from the debug traces that share stderr.
fn dump(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.starts_with('{'))
        .unwrap_or_else(|| panic!("no audit JSON on stderr:\n{stderr}"));
    lines[start..].join("\n")
}

/// `start`/`end` are wall-clock nanos, so two runs of the same script differ
/// there and nowhere else; drop them to compare dumps by value.
fn drop_timings(v: &mut Value) {
    match v {
        Value::Object(obj) => {
            obj.remove("start");
            obj.remove("end");
            obj.values_mut().for_each(drop_timings);
        }
        Value::Array(items) => items.iter_mut().for_each(drop_timings),
        _ => {}
    }
}

const SCRIPT: &str = "echo one; echo two";

#[test]
fn audit_cli_root_is_the_plain_envelope() {
    let o = run_c(&["--audit"], SCRIPT);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let root: Value = serde_json::from_str(&dump(&o.stderr)).expect("audit dump must be JSON");

    let obj = root.as_object().expect("root must be an object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["children", "error", "status", "value"]);
    assert_eq!(obj["status"], 0);

    let children = obj["children"].as_array().expect("children array");
    assert_eq!(children.len(), 2, "root: {root}");
    assert_eq!(children[0]["cmd"], "echo");
}

#[test]
fn audit_pretty_changes_the_bytes_not_the_value() {
    let compact = run_c(&["--audit"], SCRIPT);
    let pretty = run_c(&["--audit", "--pretty"], SCRIPT);
    assert_eq!(pretty.status, 0, "stderr: {}", pretty.stderr);

    let (compact, pretty) = (dump(&compact.stderr), dump(&pretty.stderr));
    assert!(
        !compact.contains('\n'),
        "compact dump is one line: {compact}"
    );
    assert!(pretty.contains('\n'), "pretty dump spans lines: {pretty}");

    let mut a: Value = serde_json::from_str(&compact).expect("compact dump must be JSON");
    let mut b: Value = serde_json::from_str(&pretty).expect("pretty dump must be JSON");
    drop_timings(&mut a);
    drop_timings(&mut b);
    assert_eq!(a, b);
}
