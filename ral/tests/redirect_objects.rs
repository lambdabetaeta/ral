#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(unix)]

//! The fs guard authorises the *object* a redirect lands on, not the name
//! written: a symlink is judged at its target, dangling or not, and a
//! before-image is a read the grant must admit.

mod common;

use common::denied_by_grant;
use ral_core::test_helper::confinement::admitted;
use std::path::{Path, PathBuf};

struct Fixture {
    root: PathBuf,
    profile: PathBuf,
}

impl Fixture {
    /// `root/work` writable, `root` readable, `deny` denied.
    fn new(name: &str, read: &[&str], deny: &[&str]) -> Self {
        let root = common::fresh_tmp_path(&format!("redirect_objects_{name}"), "d");
        std::fs::create_dir_all(root.join("work/inner")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        let list = |items: &[String]| {
            items
                .iter()
                .map(|p| format!("`absolute '{p}'"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let abs = |rel: &str| root.join(rel).display().to_string();
        let read: Vec<String> = read.iter().map(|r| abs(r)).collect();
        let deny: Vec<String> = deny.iter().map(|d| abs(d)).collect();
        let body = format!(
            "return [fs: [read: [{}], write: [{}], deny: [{}]]]\n",
            list(&read),
            list(&[abs("work")]),
            list(&deny),
        );
        let profile = common::fresh_tmp_path(&format!("redirect_objects_{name}"), "ral");
        std::fs::write(&profile, body).unwrap();
        Self { root, profile }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn link(&self, at: &str, target: &str) {
        std::os::unix::fs::symlink(target, self.path(at)).unwrap();
    }

    fn run(&self, args: &[&str], script: &str) -> common::Output {
        let script = script.replace("ROOT", &self.root.display().to_string());
        let out = common::ral_command()
            .arg("--capabilities")
            .arg(&self.profile)
            .args(args)
            .arg("-c")
            .arg(script)
            .output()
            .expect("spawn ral");
        common::Output {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            status: out.status.code().unwrap_or(1),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_file(&self.profile);
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

#[test]
fn streaming_writes_through_a_dangling_link_outside_the_grant_are_refused() {
    let proof = ral_core::require_confinement!();
    let fx = Fixture::new("dangling_out", &["."], &[]);
    fx.link("work/dangling", "../outside/marker");
    for op in [">>", ">~"] {
        let control = fx.run(
            &[],
            &format!("echo BENIGN {op} 'ROOT/work/control'; echo ok"),
        );
        let out = fx.run(
            &[],
            &format!("echo BENIGN {op} 'ROOT/work/dangling'; echo ok"),
        );
        denied_by_grant(&proof, &control, &out, "ok");
    }
    assert!(
        !fx.path("outside/marker").exists(),
        "nothing may be created outside the grant"
    );
}

#[test]
fn a_dangling_link_inside_the_grant_creates_its_target() {
    let proof = ral_core::require_confinement!();
    let fx = Fixture::new("dangling_in", &["."], &[]);
    fx.link("work/link", "inner/x");
    let out = fx.run(&[], "echo hi >> 'ROOT/work/link'; echo ok");
    admitted(&proof, &out, "ok");
    assert_eq!(read(&fx.path("work/inner/x")), "hi\n");
    assert!(
        fx.path("work/link")
            .symlink_metadata()
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn an_atomic_write_over_a_link_replaces_the_target_it_names() {
    let proof = ral_core::require_confinement!();
    let fx = Fixture::new("atomic_link", &["."], &[]);
    std::fs::write(fx.path("work/a"), "old\n").unwrap();
    fx.link("work/link", "a");
    let out = fx.run(&[], "echo new > 'ROOT/work/link'; echo ok");
    admitted(&proof, &out, "ok");
    assert_eq!(read(&fx.path("work/a")), "new\n");
    assert!(
        fx.path("work/link")
            .symlink_metadata()
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn a_link_to_a_denied_file_is_judged_at_the_file() {
    let proof = ral_core::require_confinement!();
    let fx = Fixture::new("denied_link", &["."], &["work/secret"]);
    std::fs::write(fx.path("work/secret"), "keep\n").unwrap();
    fx.link("work/link", "secret");
    let control = fx.run(&[], "echo x > 'ROOT/work/control'; echo ok");
    let out = fx.run(&[], "echo x > 'ROOT/work/link'; echo ok");
    denied_by_grant(&proof, &control, &out, "ok");
    assert_eq!(read(&fx.path("work/secret")), "keep\n");
}

/// The before-image is a read, and writing reads: a grant that writes `work`
/// and lists no read of it carries the old bytes, as one that lists it does.
#[test]
fn a_write_only_grant_carries_the_before_image_because_writing_reads() {
    let proof = ral_core::require_confinement!();
    for (name, listed) in [("diff_wo", &[][..]), ("diff_rw", &["."][..])] {
        let fx = Fixture::new(name, listed, &[]);
        std::fs::write(fx.path("work/f"), "OLD-BEFORE-IMAGE\n").unwrap();
        let out = fx.run(&["--audit"], "echo replaced > 'ROOT/work/f'; echo ok");
        admitted(&proof, &out, "ok");
        assert_eq!(read(&fx.path("work/f")), "replaced\n");
        assert!(
            out.stderr.contains("OLD-BEFORE-IMAGE"),
            "read list {listed:?}; audit dump:\n{}",
            out.stderr
        );
    }
}
