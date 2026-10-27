#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(any(target_os = "linux", target_os = "macos"))]

//! An absent deny follows names in Seatbelt and the launch snapshot in bwrap.

mod common;

use ral_core::test_helper::confinement::{Output, admitted, denied};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Stdio};

const MARKER: &str = "host-created-secret";

fn grant(root: &Path, body: &str) -> String {
    format!(
        "grant [fs: [read: [`absolute '{}'], write: [], deny: [`absolute '{}']]] {{ {body} }}",
        root.display(),
        root.join(".env").display(),
    )
}

fn waiting_reader(root: &Path, path: &Path) -> Child {
    let source = grant(
        root,
        &format!(
            r#"/bin/sh -c 'printf ready; IFS= read -r release && /bin/cat "$1"' probe '{}'"#,
            path.display(),
        ),
    );
    let mut child = common::ral_command()
        .args(["-c", &source])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the confined reader");
    let mut ready = [0; 5];
    if let Err(error) = child.stdout.as_mut().unwrap().read_exact(&mut ready) {
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        panic!("the confined reader did not reach its pipe: {error}; {out:?}");
    }
    assert_eq!(&ready, b"ready", "unexpected reader handshake");
    child
}

fn release(mut child: Child) -> Output {
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"go\n")
        .expect("release the confined reader");
    child.wait_with_output().unwrap().into()
}

fn own_read(root: &Path, path: &Path) -> Output {
    common::ral_command()
        .args([
            "-c",
            &grant(
                root,
                &format!("echo !{{from-string < '{}'}}", path.display()),
            ),
        ])
        .current_dir(root)
        .output()
        .expect("run ral's own read")
        .into()
}

#[test]
fn a_host_created_absent_deny_is_visible_only_to_the_running_linux_child() {
    let proof = ral_core::require_confinement!();
    let tree = common::Scratch::new("deny_absent");
    let allowed = tree.join("allowed");
    let secret = tree.join(".env");
    std::fs::write(&allowed, MARKER).unwrap();

    let control = release(waiting_reader(&tree.0, &allowed));
    admitted(&proof, &control, MARKER);
    let own_control = own_read(&tree.0, &allowed);
    admitted(&proof, &own_control, MARKER);

    let child = waiting_reader(&tree.0, &secret);
    let own_absent = own_read(&tree.0, &secret);
    std::fs::write(&secret, MARKER).unwrap();
    let own_present = own_read(&tree.0, &secret);
    let running = release(child);

    #[cfg(target_os = "linux")]
    admitted(&proof, &running, MARKER);
    #[cfg(target_os = "macos")]
    denied(&proof, &control, &running, MARKER);

    let next_control = release(waiting_reader(&tree.0, &allowed));
    let next = release(waiting_reader(&tree.0, &secret));
    denied(&proof, &next_control, &next, MARKER);
    for out in [own_absent, own_present] {
        denied(&proof, &own_control, &out, MARKER);
        assert!(
            out.stderr.contains("denied by grant"),
            "ral's own read must be refused by the grant: {out:?}"
        );
    }
}
