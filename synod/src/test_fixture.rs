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
