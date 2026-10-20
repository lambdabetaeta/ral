#![allow(clippy::disallowed_methods, reason = "[test] filesystem fixtures")]

mod common;

use ral_core::protocol::Run;
use ral_core::run::RunReport;
use ral_core::source::SourceOrigin;

#[test]
fn a_same_map_deny_holds_inside_a_write_prefix_at_the_shell_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_string_lossy();
    let secret = tmp.path().join("secret");
    let allowed = tmp.path().join("allowed");
    std::fs::write(&secret, "original").unwrap();
    let mut shell = common::fresh_shell();
    for (path, succeeds) in [(&allowed, true), (&secret, false)] {
        let source = format!(
            "grant [fs: [read: [`absolute '{root}'], write: [`absolute '{root}'], deny: [`absolute '{}']]] {{ echo changed > '{}' }}",
            secret.display(),
            path.display()
        );
        let RunReport::Ran { ending, .. } =
            shell.run(Run::captured(source, SourceOrigin::named("<test>")))
        else {
            panic!("the grant must compile");
        };
        assert_eq!(ending.into_result().is_ok(), succeeds);
    }
    assert_eq!(std::fs::read_to_string(allowed).unwrap(), "changed\n");
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "original");
}
