//! Founding: what a fleet refuses to start on, and the prompt each agent
//! resolves to as it is built.

use super::*;
use crate::prompt::{self, GRANT_PLACEHOLDER};

const PLACEHOLDER_PREFIX: &str = "@@EXARCH_";

/// A baked prompt as `assemble` leaves it: every per-agent slot unresolved.
fn template() -> String {
    prompt::render(&[
        (None, "persona".into()),
        (
            Some("Builtins"),
            prompt::BUILTIN_INDEX_PLACEHOLDER.to_string(),
        ),
        (Some("Host"), prompt::host_section(crate::app::EXARCH)),
    ])
}

fn system_of(agent: &Agent) -> String {
    agent.body.log.context().system().to_string()
}

/// The grant-taught sections `system` carries, in the order it carries them.
fn taught(system: &str) -> Vec<&'static str> {
    const TAUGHT: [&str; 4] = ["Agents", "Roles", "Spawned", "Reply"];
    system
        .lines()
        .filter_map(|line| {
            let heading = line.strip_prefix("# ")?;
            TAUGHT.into_iter().find(|taught| *taught == heading)
        })
        .collect()
}

fn launched(tools: Toolset, returns: bool) -> Agent {
    Agent::for_test_with(TestLaunch {
        returns,
        tools,
        ..TestLaunch::new(&template())
    })
    .unwrap()
}

#[test]
fn a_founded_agents_prompt_carries_its_own_name_grant_and_scratch() {
    let dir = tmp("found-prompt");
    let agent = Agent::born_for_test(
        LaunchConfig {
            system: template(),
            ..launch_config(&dir, 0)
        },
        identity_engine("found-prompt"),
        scripted("test-model", Script::new()),
    )
    .unwrap();
    let system = system_of(&agent);
    assert!(!system.contains(PLACEHOLDER_PREFIX), "{system}");
    assert!(system.contains("'main'"), "{system}");
    let scratch = agent
        .engine()
        .read(
            ral_core::fact::ProbeSubject::EnvVar(crate::app::SCRATCH_VAR.into()),
            |t| t.env_var(crate::app::SCRATCH_VAR),
        )
        .unwrap()
        .expect("an identity engine names its scratch");
    assert!(system.contains(&scratch), "{system}");
    let grants = agent
        .engine()
        .read(ral_core::fact::ProbeSubject::Grants, |t| t.grants())
        .unwrap();
    assert!(system.contains(&prompt::with_grant(GRANT_PLACEHOLDER, &grants)));
}

#[test]
fn each_agent_resolves_the_template_under_its_own_name_and_grants() {
    let parent = launched(Toolset::offered(false), false);
    let child = parent.fresh_child_named("helper").unwrap();
    let (parent_system, child_system) = (system_of(&parent), system_of(&child));
    for system in [&parent_system, &child_system] {
        assert!(!system.contains(PLACEHOLDER_PREFIX), "{system}");
    }
    assert!(parent_system.contains("'main'") && !parent_system.contains("'helper'"));
    assert!(child_system.contains("'helper'") && !child_system.contains("'main'"));
    // The fuelled root without a return teaches spawning only; its child is
    // spawned and returns, and the return contract comes last.
    assert_eq!(taught(&parent_system), ["Agents"]);
    assert_eq!(taught(&child_system), ["Agents", "Spawned", "Reply"]);
}

/// Chat keeps the engine's grant and nothing else: no index, no family section.
#[test]
fn a_chat_prompt_resolves_the_grant_but_teaches_no_family() {
    let chat = launched(Toolset::default(), true);
    let system = system_of(&chat);
    assert!(!system.contains(GRANT_PLACEHOLDER), "{system}");
    assert!(taught(&system).is_empty(), "{system}");
    assert_eq!(
        taught(&system_of(&launched(Toolset::offered(false), true))),
        ["Agents", "Reply"],
        "the same grants teach sections once tools are offered"
    );
}

/// A guest root carries its ceiling unresolved in `Root::config`, beside no
/// grants resolved on this host: the only wire root there is.
#[cfg(unix)]
#[test]
fn a_guest_root_carries_its_ceiling_unresolved() {
    use std::os::unix::net::UnixStream;
    let (host, _guest) = UnixStream::pair().unwrap();
    let ceiling = ral_core::authority::grant::value::GrantValue::deny_all();
    let engine = RootEngine::guest(
        host,
        GuestPath::from_static("/work"),
        GuestPath::from_static("/tmp"),
        ceiling.clone(),
    )
    .unwrap();
    let RootEngine::Wire(WireRoot { attach, .. }) = engine else {
        panic!("a guest engine is a wire engine");
    };
    let ral_core::protocol::Origin::Root(root) = attach.origin else {
        panic!("a guest attach is a root's");
    };
    assert!(root.grants.is_empty(), "{:?}", root.grants);
    assert_eq!(root.config, ral_core::data::FOValue::from(ceiling));
}

/// The refusal precedes every effect: no run directory content is made.
#[cfg(unix)]
#[test]
fn a_wire_root_with_fuel_needs_an_engine_factory() {
    use std::os::unix::net::UnixStream;
    let dir = tmp("found-no-factory");
    let (host, _guest) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(host, Liveness::default()).unwrap();
    let (spec, root) = crate::boot::test_attach();
    let Err(error) = Fleet::found(
        launch_config(&dir, 1),
        RootEngine::Wire(WireRoot {
            transport: Box::new(transport),
            attach: Attach::root(spec, root),
        }),
        &crate::INSTALLERS,
    ) else {
        panic!("a fuelled wire root without a factory must be refused");
    };
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(!dir.path().join("sessions").exists());
}
