use super::Pinned;

fn pinned_file(dir: &std::path::Path) -> (std::path::PathBuf, Pinned) {
    let path = std::fs::canonicalize(dir)
        .expect("the dir resolves")
        .join("ral");
    std::fs::write(&path, "pinned").expect("write the file");
    let pin = Pinned::open(path.clone()).expect("the file pins");
    (path, pin)
}

#[test]
fn an_unchanged_pin_verifies() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (_, pin) = pinned_file(dir.path());
    assert!(pin.verify().is_ok());
}

#[test]
fn a_pin_swapped_by_rename_no_longer_verifies() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (path, pin) = pinned_file(dir.path());
    let impostor = dir.path().join("impostor");
    std::fs::write(&impostor, "impostor").unwrap();
    std::fs::rename(&impostor, &path).unwrap();
    assert!(pin.verify().is_err());
}

#[test]
fn a_pin_that_lost_its_name_no_longer_verifies() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (path, pin) = pinned_file(dir.path());
    std::fs::remove_file(&path).unwrap();
    assert!(pin.verify().is_err());
}

#[test]
fn a_pin_reads_its_identity_from_the_trace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (path, pin) = pinned_file(dir.path());
    assert_eq!(pin.trace().leaf_id(), Some(pin.object_id()));
    assert_eq!(pin.arg0(), path);
}

#[test]
fn an_absent_name_pins_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let absent = std::fs::canonicalize(dir.path()).unwrap().join("absent");
    assert!(Pinned::open(absent).is_none());
}
