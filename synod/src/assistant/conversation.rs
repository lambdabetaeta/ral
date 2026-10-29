//! One folder and one guest; each released job follows the preceding folder walk.

use super::accounts::{Selected, select_account};
use super::baseline::Baseline;
use super::gate::Seat;
use super::guest::Guest;
use crate::folder::grant::Grant;
use crate::folder::{
    changes::{JobReport, job_report},
    manifest::{self, Manifest},
};
use crate::wire::{Choice, Opening, WindowReport};
use exarch::agent::fleet::HELPER_IDLE;
use exarch::agent::{Admission, EngineLost, Trunk};
use exarch::app::SYNOD;
use exarch::bus::BusReceiver;
use exarch::provider::{self, Bureau, Holdings, Pool};
use exarch::record::AgentId;
use ral_core::carrier::Severed;
use ral_core::path::{XdgKind, shown};
use std::path::Path;
use std::sync::Arc;
use vm_manager::media;

pub(crate) struct Conversation {
    grant: Grant,
    guest: Guest,
    _run_lock: exarch::app::RunLock,
    baseline: Baseline,
    /// Cumulative against the opening baseline, remade after each job.
    report: Option<JobReport>,
}

impl Conversation {
    /// Networking must start successfully; the host then settles the baseline.
    pub(crate) fn begin(
        folder: &Path,
        holdings: &Holdings,
        choice: Choice,
        baseline_stop: &manifest::Stop,
        baseline_progress: Box<dyn FnMut(u64) + Send>,
    ) -> Result<(Self, Opening), String> {
        let grant = Grant::open(folder)?;
        let hypervisor = vm_manager::detect(boot_media()?)
            .map_err(|e| e.worded(|path| shown(path)).to_string())?;

        let disk_warn_bytes = exarch::config::disk_warn_bytes()?;
        let policy = exarch::net_policy::load()?;
        let audit = SYNOD
            .own(XdgKind::State)
            .and_then(netgate::AuditLog::open)
            .map_err(|e| format!("network audit ledger: {e}"))?;

        let Selected {
            account,
            label,
            model,
            effort,
        } = select_account(&holdings.store, choice)?;
        let announced_model = model.clone();
        let tuning = provider::Tuning::for_model(effort.as_deref(), &model)?;
        let announced_effort = provider::effort_label(&tuning.effort)
            .expect("Tuning::for_model yields only efforts the ladder names")
            .to_string();

        let run_dir = SYNOD
            .new_run(grant.root())
            .map_err(|e| format!("could not make a log folder: {e}"))?;
        let run_lock = exarch::app::RunLock::try_acquire(&run_dir)
            .map_err(|e| format!("could not lock the log folder {run_dir}: {e}"))?;

        // Boot and baseline run concurrently; no guest act precedes baseline settlement.
        let baseline =
            Baseline::spawn(grant.root().to_path_buf(), baseline_stop, baseline_progress);

        let booted = hypervisor.boot(&grant.machine_spec()).map_err(|e| {
            format!(
                "could not start a machine for {}: {}",
                shown(grant.root()),
                e.worded(|path| shown(path))
            )
        })?;

        // Authority and prompt paths use only the guest namespace.
        let caps = grant.capabilities();
        let system =
            crate::assistant::prompt::assemble(guest_abi::layout::WORK.as_ref(), &grant.name())?;

        // The provider pool is per-conversation; holdings are shared.
        let bureau = Arc::new(Bureau::Live {
            engine: Pool::new(),
            holdings: holdings.clone(),
        });
        let provider = bureau.build(
            provider::Choice {
                account,
                model,
                via: None,
            },
            &tuning,
            None,
        )?;

        let config = exarch::agent::RootConfig {
            system,
            run_dir,
            allow_schedule: false,
            resume_on_reset: false,
            trunk: Trunk::Converses,
            admission: Admission::Gated,
            helper_idle: HELPER_IDLE,
            tools: exarch::provider::Toolset::offered(false),
            disk_warn_bytes,
            fuel: exarch::agent::SPAWN_FUEL,
            search: policy.search,
            // Guest::start installs the factory once its machine exists.
            factory: None,
            bureau,
            roles: exarch::provider::roles::Roles::default(),
        };
        let guest = Guest::start(
            booted,
            config,
            caps,
            provider,
            netgate::Config {
                policy: Arc::new(policy),
                audit,
                dialer: Arc::new(netgate::System),
            },
        )?;

        Ok((
            Self {
                grant,
                guest,
                _run_lock: run_lock,
                baseline,
                report: None,
            },
            Opening {
                label,
                model: announced_model,
                effort: announced_effort,
            },
        ))
    }

    pub(crate) fn handle(&self) -> exarch::agent::AgentHandle {
        self.guest.handle()
    }

    pub(crate) fn settle_baseline(&mut self) -> Result<(), String> {
        self.baseline.manifest().map(drop)
    }

    pub(crate) fn end(self) -> Result<(), String> {
        let Self {
            guest, baseline, ..
        } = self;
        let shutdown = guest.end();
        // The machine shuts down before an abandoned baseline is joined.
        drop(baseline);
        shutdown
    }
}

impl Seat for Conversation {
    fn root(&self) -> AgentId {
        self.handle().id()
    }

    fn feed(&self) -> &BusReceiver {
        self.guest.feed()
    }

    /// Captured while the machine exists.
    fn engine_lost(&self, cause: &Severed) -> String {
        let lost = EngineLost::running(cause, self.guest.run_dir());
        self.guest.capture(&lost);
        lost.to_string()
    }

    /// After the baseline, and after any job's walk.
    fn release(&self) -> Result<(), String> {
        self.baseline.settled_manifest()?;
        if self.handle().release() {
            Ok(())
        } else {
            Err("The assistant ended before it could begin the next job.".to_string())
        }
    }

    fn settled(&mut self) -> Result<(), String> {
        self.report = None;
        let before = self.baseline.settled_manifest()?;
        let after = Manifest::of_folder(self.grant.root())?;
        self.report = Some(job_report(before, &after));
        Ok(())
    }

    fn report(&self) -> Option<WindowReport> {
        self.report.as_ref().map(WindowReport::of)
    }

    fn invalidate(&mut self) {
        self.report = None;
        self.handle().end();
    }
}

pub(crate) fn boot_media() -> Result<vm_manager::BootMedia, String> {
    let exe = std::env::current_exe().map_err(|cause| {
        format!("synod could not locate its executable to find boot media: {cause}")
    })?;
    let cache = SYNOD
        .own(XdgKind::Cache)
        .map_err(|cause| format!("synod could not locate its boot cache: {cause}"))?
        .path()
        .join("boot");
    Ok(Box::new(move || media::locate(&exe)?.ready(&cache)))
}

impl Grant {
    pub(crate) fn machine_spec(&self) -> vm_manager::MachineSpec {
        vm_manager::MachineSpec {
            folder: self.root().into(),
            read_only: false,
        }
    }
}
