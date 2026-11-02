//! The desk's wire arm without an engine factory.

use super::*;

/// A fuelled wire launch is refused at founding without a factory, so a desk
/// that still lacks one is a bug it reports rather than hides.
#[test]
fn a_desk_without_a_factory_cannot_make_an_engine() {
    with_fleet_desk(
        3,
        crate::agent::fleet::Launch::for_test(),
        |desk, _fleet, _agent| {
            let seal = ral_core::seed::seal(
                &crate::boot::test_shell().bequeath(ral_core::SpawnGrant::Inherit),
            )
            .expect("a bequest seals");
            let made = desk.make_engine(&crate::boot::test_attach().0, seal);
            assert!(made.is_err());
        },
    );
}
