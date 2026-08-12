//! A human-driven proof that synod's fleet delegates inside a booted guest.
//!
//! Boots a real machine, seats a scripted-provider trunk on its control wire
//! exactly as [`synod::session::Conversation::begin`] does, and drives one
//! [`converse_settled`] exchange whose script has the model delegate: spawn
//! a helper, have the helper write a file and reply, wait for the whole
//! fleet to quiesce, then checkpoint and report — the same bracket
//! [`synod::session::Conversation::exchange`] runs, assembled by hand here
//! so the provider can be a canned script instead of a live account.
//!
//! It is the same program on both platforms, and that is the interesting
//! part: nothing below is `#[cfg]`-ed except which backend is constructed —
//! the control plane is an `AF_VSOCK` descriptor under Virtualization.framework
//! and an `AF_HYPERV` socket under Hyper-V, and
//! [`WireTransport::adopt`](ral_core::transport::WireTransport::adopt) takes
//! either, so the protocol never learns which hypervisor it is talking
//! through.
//!
//! # What each platform asks of you first
//!
//! - **macOS** — a signed binary. Run `dev/scripts/sign-virtualization.sh
//!   target/debug/examples/boot-run` after every rebuild.
//! - **Windows** — an account the compute service serves: an administrator,
//!   or a member of the *Hyper-V Administrators* group. (The Windows guest
//!   has not yet witnessed a completed boot at all — see
//!   `docs/ral-wiki/map/synod.md`'s "What is not here" — so this example
//!   only reaches the point that fact allows.)
//!
//! # A known race, accepted rather than hidden
//!
//! A hatched helper's provider is a *snapshot of the trunk's own live one*
//! (`agent-start`'s wire arm, `exarch::fleet::desk`), taken the instant its
//! wire is adopted — the same queue the trunk's own next turn also pulls
//! from, and the helper's detached thread starts before the trunk's tool
//! call has finished unwinding back to its own next turn. A scripted queue
//! cannot be handed to one side and not the other from outside, so this
//! script is written to tolerate either pull order: the one entry whose
//! content matters (write the file, then reply) is a single combined turn,
//! and every other entry is inert filler text a stray pull can safely land
//! on. A live account has no such race, because nothing is queued at all.
//!
//! Usage: `boot-run <kernel> <initramfs> <rootfs> <folder>`

use exarch::agent::{Agent, RootConfig, RootSeat, SPAWN_FUEL};
use exarch::bus::{Event, Kind, Sink};
use exarch::egress::Egress;
use exarch::headless::converse_settled;
use exarch::provider::scripted::{Reply, Script};
use exarch::provider::{Engine, Provider, ProviderKind, ToolCall};
use ral_core::transport::{Liveness, WireTransport};
use ral_core::types::Capabilities;
use std::path::PathBuf;
use std::sync::Arc;
use synod::hatchery::MachineHatchery;
use synod::workspace::{self, HistoryStore};
use vm_manager::{BootArtifact, Hypervisor, MachineSpec};

#[cfg(unix)]
type NetStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type NetStream = std::net::TcpStream;

/// The file the helper is asked to write, checked against the after-job
/// report at the end.
const HELPER_FILE: &str = "helper-output.txt";
const HELPER_TEXT: &str = "hello from the helper";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok([kernel, initramfs, rootfs, folder]) = <[String; 4]>::try_from(args) else {
        eprintln!("usage: boot-run <kernel> <initramfs> <rootfs> <folder>");
        std::process::exit(2);
    };
    let folder_path = PathBuf::from(&folder);

    // The safety net, exactly as `Conversation::begin` takes it: a baseline
    // before anything touches the folder, so the report at the end is
    // judged against what the guest actually changed.
    let history = HistoryStore::open_for(&folder_path).expect("open the folder's safety-net store");
    history
        .capture(&folder_path, workspace::Moment::Before)
        .expect("take the before checkpoint");

    let artifact = BootArtifact {
        kernel: kernel.into(),
        initramfs: initramfs.into(),
        rootfs: rootfs.into(),
    };
    let Some(hypervisor) = backend(artifact) else {
        eprintln!("this platform has no virtual-machine backend in vm-manager");
        std::process::exit(1);
    };

    println!("booting via {}...", hypervisor.name());
    let mut machine = match hypervisor.boot(&MachineSpec::for_folder(&folder)) {
        Ok(machine) => machine,
        Err(err) => {
            eprintln!("boot failed: {err}");
            std::process::exit(1);
        }
    };
    println!("booted: the agent can reach the granted folder and nothing else on this computer");

    let workspace_path = machine.workspace_path().to_path_buf();
    let wires = machine.take_wires();
    // Held open only so its EOF ends the session; this example wants no
    // guest network of its own.
    let _net = NetStream::from(wires.net);

    // The machine goes to the accept pump now — the same handoff
    // `Conversation::begin` makes, and the reason `end`'s ordering below
    // recovers it from the pump rather than from `machine` directly.
    let hatchery = Arc::new(MachineHatchery::start(machine));

    let transport = WireTransport::adopt(wires.control, Liveness::default())
        .expect("adopt the guest's control plane");
    let root_seat = RootSeat::Wire {
        transport: Box::new(transport),
        cwd: workspace_path,
        home: PathBuf::from("/tmp"),
    };

    let spawn_cmd = format!(
        "agent [prompt: #'write the text {HELPER_TEXT:?} into {HELPER_FILE}, then reply \
         confirming the write'#, name: 'helper', type: `amnemon, grant: `edit-only, search: false]"
    );
    let write_and_reply = vec![
        ral_call("write", &format!("echo {HELPER_TEXT:?} > {HELPER_FILE}")),
        ral_call("reply", &format!("reply \"wrote {HELPER_FILE}\"")),
    ];
    let script = Script::new()
        // The trunk's own first and only necessary turn: delegate.
        .then(Reply::tool_calls(vec![ral_call("spawn", &spawn_cmd)]))
        // The one turn whose content matters, meant for the helper — see
        // this file's top comment on why either side pulling it is fine.
        .then(Reply::tool_calls(write_and_reply))
        // Filler: whichever agent's next turn (the trunk processing the
        // spawn receipt, a marked-note nudge once the helper settles, or a
        // recovery turn if the ordering above landed the other way) gets
        // harmless closing prose rather than an empty queue.
        .then(Reply::text("asked the helper to write the file"))
        .then(Reply::text("the helper's write is done"))
        .then(Reply::text("all set"))
        .then(Reply::text("all set"));

    let engine = Engine::new();
    let provider = Arc::new(Provider::scripted(
        "test-model",
        ProviderKind::Openai,
        script,
    ));
    let mut agent = Agent::root(
        RootConfig {
            system: "you are a helpful office assistant".to_string(),
            caps: Capabilities::root(),
            run_dir: std::env::temp_dir().join(format!("boot-run-{}", std::process::id())),
            model: "test-model".to_string(),
            provider_label: "test".to_string(),
            allow_schedule: false,
            interactive: true,
            chat: false,
            disk_warn_bytes: None,
            fuel: SPAWN_FUEL,
            egress: Egress::for_test(),
            hatchery: Some(hatchery.clone()),
        },
        root_seat,
        provider,
    )
    .expect("start the trunk");

    let mut sink = PrintSink;
    let exchange = converse_settled(
        &mut agent,
        "please have the helper write the file".to_string(),
        engine,
        &mut sink,
    );

    // Ending the agent before recovering the machine, and recovering the
    // machine before shutting it down, is `Conversation::end`'s own order —
    // reproduced here rather than borrowed, since this example has no
    // `Conversation` to call it on.
    drop(agent);
    let machine = Arc::try_unwrap(hatchery)
        .unwrap_or_else(|_| panic!("the pump outlived the agent that was its only other owner"))
        .join();

    match &exchange {
        Ok(()) => println!("the exchange settled: the trunk parked and every helper finished"),
        Err(err) => eprintln!("the exchange failed: {err}"),
    }

    let after = history.capture(&folder_path, workspace::Moment::After);
    let report_ok = if let (Ok(()), Ok(_)) = (&exchange, &after) {
        let report = workspace::job_report(&history, &folder_path)
            .expect("a job just ran; the report must read back");
        let wrote_it = report.changes.changes.iter().any(|c| {
            matches!(c, workspace::changes::Change::Created { path, folder: false } if path == HELPER_FILE)
        });
        println!("report: {:?}", report.changes);
        if wrote_it {
            println!("PASS: the helper's file was created and the report names it");
        } else {
            eprintln!("FAIL: the report does not list {HELPER_FILE} as created");
        }
        wrote_it
    } else {
        eprintln!("FAIL: the exchange or its after-checkpoint did not succeed");
        false
    };

    match machine.shutdown() {
        Ok(()) => println!("stopped cleanly"),
        Err(err) => eprintln!("shutdown failed: {err}"),
    }
    if exchange.is_err() || !report_ok {
        std::process::exit(1);
    }
}

/// One `ral` tool call: `fn_name` is always `"ral"`, `cmd` the ral source the
/// engine evaluates — the shape every real provider integration sends, and
/// the one `exarch::fleet::desk`'s own scripted-provider tests build by
/// hand rather than exposing as a test-only helper across crates.
fn ral_call(id: &str, cmd: &str) -> ToolCall {
    ToolCall {
        call_id: id.to_string(),
        fn_name: "ral".to_string(),
        fn_arguments: serde_json::json!({
            "cmd": cmd,
            "description": "boot-run example command",
        }),
        thought_signatures: None,
    }
}

/// Prints every event's shape as it streams by — enough to follow the
/// exchange along in a terminal without rendering a real UI.
struct PrintSink;

impl Sink for PrintSink {
    fn handle(&mut self, e: Event) {
        match e.kind {
            Kind::Token(text) => print!("{text}"),
            Kind::SubagentDone { name, outcome, .. } => {
                println!("\n[{name} finished: {outcome:?}]");
            }
            Kind::State(state) => println!("\n[state: {state:?}]"),
            _ => {}
        }
    }
}

/// The backend this platform has, constructed directly rather than through
/// `vm_manager::detect` — an example already holds the boot media, and
/// `synod::boot` is what finds it in a shipped build.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the `Option` is the seam's shape, not this arm's: a platform with a backend always \
              answers `Some`, and the one without answers `None` from the third arm below"
)]
#[cfg(target_os = "macos")]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    Some(Box::new(vm_manager::vz::Vz::new(
        artifact,
        std::env::temp_dir(),
    )))
}

/// The backend this platform has — see the macOS twin.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the `Option` is the seam's shape, not this arm's: a platform with a backend always \
              answers `Some`, and the one without answers `None` from the third arm below"
)]
#[cfg(windows)]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    use vm_manager::hcs::Hyperv;
    Some(Box::new(Hyperv::new(artifact, Hyperv::default_cache())))
}

/// No backend here, and the example says so at run time rather than failing
/// to build.
#[cfg(not(any(target_os = "macos", windows)))]
fn backend(artifact: BootArtifact) -> Option<Box<dyn Hypervisor>> {
    drop(artifact);
    None
}
