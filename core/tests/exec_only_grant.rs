#![allow(clippy::disallowed_methods)]

//! Regression: a grant that holds an opinion about `exec` alone still yields
//! an OS-renderable projection, wherever a backend carries the exec allow-list
//! into the kernel.
//!
//! That layer exists for exactly the re-execs the in-process gate cannot see —
//! `sh -c`, `find -exec` — so a grant whose *whole content* is an exec rule is
//! the shape that needs it most.  It was also the shape that skipped it: the
//! trigger named macOS alone and stayed that way when Linux's Landlock ruleset
//! landed, so adding an unrelated `fs:` key to the same grant turned kernel
//! enforcement back on.  The trigger now reads `sandbox::EXEC_ENFORCED`, one
//! fact stated beside the backends, so this test is the law and not a platform
//! list: both Unix backends render exec, so both must project.
//!
//! On a host with no `bwrap` the projection is then refused at launch rather
//! than run unconfined — the fail-closed direction every other grant shape
//! already takes, and not a reason to weaken the trigger.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use ral_core::types::{Capabilities, ExecMap, ExecPolicy, ExecProjection};

#[test]
fn exec_only_grant_still_projects() {
    let mut shell = common::fresh_shell();
    let caps = Capabilities {
        exec: Some(ExecMap {
            literals: std::iter::once(("/bin/sh".to_string(), ExecPolicy::Allow)).collect(),
            ..ExecMap::default()
        }),
        ..Capabilities::root()
    };
    shell.with_capabilities(caps, |shell| {
        let projection = shell
            .sandbox_projection()
            .expect("an exec-only grant asks for kernel exec confinement");
        assert!(matches!(projection.exec, ExecProjection::Restricted { .. }));
    });
}
