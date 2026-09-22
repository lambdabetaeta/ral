//! A human-driven proof that synod's fleet delegates inside a booted guest.
//!
//! Boots a real machine, seats a scripted-provider trunk on its control wire
//! through [`synod::session::seat_machine`] — the same constructor
//! [`synod::session::Conversation::begin`] calls — and drives one
//! [`converse_settled`] exchange whose script has the model delegate: spawn
//! a helper, have the helper write a file and reply, wait for the whole
//! fleet to quiesce, then walk the folder again and report.
//!
//! It is the same program on both platforms, and that is the interesting
//! part: nothing below is `#[cfg]`-ed except which backend is constructed —
//! the control plane is an `AF_VSOCK` descriptor under Virtualization.framework
//! and an `AF_HYPERV` socket under Hyper-V, and
//! [`seat_machine`](synod::session::seat_machine) adopts either the same
//! way, so the protocol never learns which hypervisor it is talking
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
//! (`` agents `start ``'s wire arm, `exarch::fleet::desk`), taken the instant its
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

#![allow(
    clippy::disallowed_methods,
    reason = "example binary: its `main` owns the process, so an `exit` unwinds nothing another owner needs"
)]

use exarch::agent::{RecordedAccount, RootConfig, SPAWN_FUEL};
use exarch::bus::{AgentId, Sink};
use exarch::egress::Egress;
use exarch::headless::converse_settled;
use exarch::provider::scripted::{Reply, Script};
use exarch::provider::{Bureau, Provider, ToolCall};
use exarch::record::{Display, Record, Recorded, Transient};
use ral_core::types::GrantStack;
use std::path::PathBuf;
use std::sync::Arc;
use synod::session::{seat_machine, unseat_machine};
use synod::workspace::{self, Manifest};
use vm_manager::{BootArtifact, Hypervisor, MachineSpec};

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

    // The baseline, exactly as `Conversation::begin` takes it: the folder's
    // shape before anything touches it, so the report at the end is judged
    // against what the guest actually changed.
    let before = Manifest::of_folder(&folder_path).expect("stat-walk the folder as it stands");

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
    let machine = match hypervisor.boot(&MachineSpec::for_folder(&folder)) {
        Ok(machine) => machine,
        Err(err) => {
            eprintln!("boot failed: {err}");
            std::process::exit(1);
        }
    };
    println!("booted: the agent can reach the granted folder and nothing else on this computer");

    let spawn_cmd = format!(
        "exarch-agents `start [prompt: #'write the text {HELPER_TEXT:?} into {HELPER_FILE}, \
         then reply confirming the write'#, name: 'helper', type: `amnemon, grant: `edit-only, \
         search: false, provider: `inherit, model: `inherit]"
    );
    let write_and_reply = vec![
        // `/bin/echo`, never the `echo` builtin: this example is the only
        // thing that boots a guest, so it is the only thing that can catch the
        // guest's spawn jail breaking, and a builtin spawns nothing.
        ral_call(
            "write",
            &format!("/bin/echo {HELPER_TEXT:?} > {HELPER_FILE}"),
        ),
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

    let provider = Arc::new(Provider::scripted("test-model", script));
    // Made here, not by `Avatar::root`: it takes the directory as given so that
    // a `--resume` naming one that does not exist fails instead of quietly
    // starting an empty session.
    let run_dir = std::env::temp_dir().join(format!("boot-run-{}", std::process::id()));
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:boot-run-run-dir] Scratch setup before the trunk exists, in an example that is its own only caller — not model I/O."
    )]
    std::fs::create_dir_all(&run_dir).expect("make the run directory");
    let config = RootConfig {
        system: "you are a helpful office assistant".to_string(),
        caps: GrantStack::root(),
        run_dir,
        resume: None,
        run_lock: None,
        model: "test-model".to_string(),
        account: RecordedAccount {
            label: "test".to_string(),
            service: "scripted".to_string(),
            id: "test".to_string(),
        },
        allow_schedule: false,
        interactive: true,
        chat: false,
        thinking_tool: false,
        disk_warn_bytes: None,
        fuel: SPAWN_FUEL,
        egress: Egress::for_test(),
        // `seat_machine` overwrites this: the dialler cannot exist before
        // the machine it wraps.
        dial: None,
        // The provider above is scripted, so nothing here ever mints one.
        bureau: Arc::new(Bureau::Scripted),
    };
    // `_net` is held open only so its EOF ends the session; this example
    // wants no guest network of its own.
    let (dial, mut agent, _net) = seat_machine(machine, config, provider).expect("start the trunk");

    let mut sink = PrintSink;
    let exchange = converse_settled(
        &mut agent,
        "please have the helper write the file".to_string(),
        &mut sink,
    );

    // `unseat_machine` is `Conversation::end`'s own machine-recovery step —
    // see its doc for why the agent must drop before the machine comes
    // back.
    let machine = unseat_machine(agent, dial);

    match &exchange {
        Ok(()) => println!("the exchange settled: the trunk parked and every helper finished"),
        Err(err) => eprintln!("the exchange failed: {err}"),
    }

    let after = Manifest::of_folder(&folder_path);
    let report_ok = if let (Ok(()), Ok(after)) = (&exchange, &after) {
        let report = workspace::job_report(&before, after);
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
        eprintln!("FAIL: the exchange or its closing walk did not succeed");
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

/// Prints the exchange's shape as it streams by — enough to follow it along
/// in a terminal without rendering a real UI.
struct PrintSink;

impl Sink for PrintSink {
    fn fact(&mut self, _id: AgentId, rec: &Recorded<Record>) {
        if let Record::Display(Display::SubagentDone { name, error, .. }) = rec.value() {
            match error {
                Some(e) => println!("\n[{name} failed: {e}]"),
                None => println!("\n[{name} finished]"),
            }
        }
    }

    fn transient(&mut self, _id: AgentId, t: &Transient) {
        match t {
            Transient::Token(text) => print!("{text}"),
            Transient::State(state) => println!("\n[state: {state:?}]"),
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
