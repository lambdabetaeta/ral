use super::*;

#[test]
fn hresult_from_win32_matches_already_exists_constant() {
    // 0x800700B7 is the published wrapping, checkable against `winerror.h`.
    assert_eq!(
        hresult_from_win32(ERROR_ALREADY_EXISTS).cast_unsigned(),
        0x8007_00B7
    );
}

#[test]
fn capability_sids_empty_when_network_denied() {
    let caps = CapabilitySids::build(false).expect("build never touches the OS when empty");
    assert_eq!(caps.sids().count(), 0);
}

#[test]
fn capability_sids_derives_both_network_capabilities_when_allowed() {
    let caps = CapabilitySids::build(true).expect(
        "internetClient/privateNetworkClientServer are well-known capabilities on any \
         supported Windows version",
    );
    assert_eq!(caps.sids().count(), NETWORK_CAPABILITIES.len());
    for sid in caps.sids() {
        assert!(!sid.is_null());
    }
}

#[test]
fn profile_create_reuse_sid_string_and_delete_round_trip() {
    // Keyed on pid so a concurrent test process cannot collide.
    let name = format!("ral.test.appcontainer.{}", std::process::id());

    let profile = AppContainerProfile::create_or_reuse(&name).expect("first create succeeds");
    assert_eq!(profile.name, name);
    assert!(!profile.sid().sid().is_null());
    let sid_string = profile.sid_string().expect("SID converts to string form");
    assert!(
        sid_string.starts_with("S-1-15-"),
        "unexpected AppContainer SID: {sid_string}"
    );

    // Dropping frees the in-memory SID but leaves the OS profile
    // registered, so the next create takes the already-exists path.
    drop(profile);
    let reused = AppContainerProfile::create_or_reuse(&name).expect("reuse succeeds");
    assert_eq!(
        reused.sid_string().expect("SID converts to string form"),
        sid_string
    );
    reused.delete().expect("delete succeeds");

    // The SID is a deterministic function of the name, so it survives a
    // delete and re-create.
    let recreated = AppContainerProfile::create_or_reuse(&name).expect("re-create succeeds");
    assert_eq!(
        recreated.sid_string().expect("SID converts to string form"),
        sid_string
    );
    recreated.delete().expect("final delete succeeds");
}
