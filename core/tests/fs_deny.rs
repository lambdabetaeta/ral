#![allow(clippy::disallowed_methods, reason = "test code")]

mod common;

use ral_core::test_helper::confinement::{denied, run};
use ral_core::test_helper::stack;

#[test]
fn a_same_map_deny_holds_inside_a_write_prefix_at_the_shell_boundary() {
    let proof = ral_core::require_confinement!();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_string_lossy();
    let secret = tmp.path().join("secret");
    let allowed = tmp.path().join("allowed");
    std::fs::write(&secret, "original").unwrap();
    let write = |path: &std::path::Path| {
        run(
            stack([]),
            &format!(
                "grant [fs: [read: [`absolute '{root}'], write: [`absolute '{root}'], deny: [`absolute '{}']]] {{ echo changed > '{}'; echo ok }}",
                secret.display(),
                path.display(),
            ),
        )
    };
    let control = write(&allowed);
    let refused = write(&secret);
    denied(&proof, &control, &refused, "ok");
    assert_eq!(std::fs::read_to_string(allowed).unwrap(), "changed\n");
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "original");
}
