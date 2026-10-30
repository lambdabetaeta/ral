/// Canonical target avoids symlink ambiguity and overlap with the guest's granted /tmp.
pub(crate) fn workshop(tag: &str) -> tempfile::TempDir {
    // CARGO_TARGET_TMPDIR is only set for integration tests.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/synod-test");
    std::fs::create_dir_all(&root).expect("test workshop root");
    let root = std::fs::canonicalize(root).expect("canonical test workshop root");
    tempfile::Builder::new()
        .prefix(&format!("synod-{tag}-"))
        .tempdir_in(root)
        .expect("temp workshop")
}

/// A gated root over the identity engine; the factory is the machine's, when it has one.
pub(crate) fn scripted_host(
    run_dir: &ral_core::own::OwnDir,
    factory: Option<std::sync::Arc<dyn exarch::host::Factory>>,
    script: exarch::provider::scripted::Script,
) -> exarch::host::Host {
    use exarch::app::{SYNOD, Scratch};
    use exarch::host::{Admission, HELPER_IDLE, RootConfig, RootEngine, Trunk};
    use exarch::provider::{Bureau, Provider, Toolset};
    use std::sync::Arc;

    let config = RootConfig {
        system: "system".into(),
        run_dir: ral_core::own::OwnDir::clone(run_dir),
        trunk: Trunk::Converses,
        admission: Admission::Gated,
        helper_idle: HELPER_IDLE,
        tools: Toolset::default(),
        allow_schedule: false,
        resume_on_reset: false,
        disk_warn_bytes: None,
        fuel: 0,
        search: false,
        factory,
        bureau: Arc::new(Bureau::scripted([])),
        max_tokens: None,
        roles: exarch::provider::roles::Roles::default(),
    };
    let engine = RootEngine::Identity {
        scratch: Arc::new(Scratch::for_test(SYNOD, "synod-host").expect("engine scratch")),
        cwd: ral_core::host::cwd().expect("test cwd"),
        grants: ral_core::authority::policy::GrantStack::default(),
        terminal: ral_core::io::terminal::TerminalState::default(),
    };
    let provider = Arc::new(Provider::scripted("test-model", script));
    exarch::host::Host::found(config, engine, provider).expect("host launches its root")
}
