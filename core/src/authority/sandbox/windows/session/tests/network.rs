use super::{SessionGuard, Tree, projection, with_scoped_state};

#[test]
fn only_online_projections_attach_network_capability_sids() {
    with_scoped_state(|| {
        let _session = SessionGuard;
        let tree = Tree::new();
        let mut grant = projection(&[&tree.real("A")], &[]);
        for (online, expected) in [(false, 0), (true, 2), (false, 0), (true, 2)] {
            grant.net = (!online).then_some(crate::authority::policy::Net::Offline);
            let caps = super::super::confine(
                &grant,
                None,
                &crate::process::DurableRoot::default().child(),
            )
            .unwrap();
            assert_eq!(
                caps.network_capability_count(),
                expected,
                "online={online}: the token must carry exactly its network SIDs"
            );
        }
    });
}
