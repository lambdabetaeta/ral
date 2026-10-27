use super::{SessionGuard, Tree, confined, projection, with_scoped_state};

#[test]
fn only_online_projections_attach_network_capability_sids() {
    with_scoped_state(|| {
        let _session = SessionGuard;
        let tree = Tree::new();
        let cwd = tree.at("A");
        let mut grant = projection(&[&tree.real("A")], &[]);
        for (online, expected) in [(false, 0), (true, 2), (false, 0), (true, 2)] {
            grant.net = online;
            let launch = confined(&grant, &cwd, "echo ok").unwrap();
            assert_eq!(
                launch.network_capability_count(),
                expected,
                "online={online}: the staged launch must carry exactly its network SIDs"
            );
        }
    });
}
