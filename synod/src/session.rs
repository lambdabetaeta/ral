//! The conversation itself: one folder, one agent, and the exchanges the
//! window drives between them.
//!
//! Synod's session is exarch's session with the developer removed.  The
//! provider transport, the turn driver, and the card bus are exarch's
//! ([`exarch::provider`], [`exarch::agent`], [`exarch::headless`]); what
//! differs is where the work happens (the machine's workspace, not the
//! shell's cwd), what the agent may touch (the grant, not a capability
//! base named on the command line), and what the user is told.
//!
//! There is no wire protocol here any more: synod is a library the window
//! calls in-process.  [`prepare`] resolves the credential store once at
//! startup; [`menu`] turns it into a picker the window can render;
//! [`Conversation::begin`] opens a folder onto a booted machine and an
//! agent, [`Conversation::exchange`] drives one message through it, and
//! [`Conversation::end`] shuts the machine down.  Provider and model are
//! either named by the window (a menu choice) or resolved the old way —
//! whichever one account is set up on this computer, and its default
//! model.

use crate::grant::Grant;
use crate::workspace;
use exarch::agent::Agent;
use exarch::bootstrap;
use exarch::provider::{
    self, Engine, Provider, ProviderId,
    credential::CredentialStore,
    listing::Listing,
    models::{ModelCatalog, ModelSource, resolve_pinned_provider},
    pricing,
};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use vm_manager::Machine;

/// Synod's own directories.
///
/// `$XDG_STATE_HOME/synod/<folder>/` for the run logs.  A synod run must
/// never write into an exarch run's logs, nor read its model selection.
/// The agent's working area is not among these: it is the guest's own
/// scratch tmpfs ([`crate::grant::GUEST_SCRATCH`]), no host directory.
pub const SYNOD: bootstrap::App = bootstrap::App::new("synod");

/// "Protection: {this}." — true of every machine synod boots, since the
/// only backend left is a real virtual machine.  Carried here, verbatim,
/// from what was vm-manager's `Boundary::Hardware` sentence, before that
/// type lost every other inhabitant and stopped being worth keeping.
const HARDWARE_PROTECTION: &str = "a separate virtual machine: the agent can reach the folder \
                                    you granted and nothing else on this computer";

/// Resolve the credential store, once, at startup.
///
/// # Errors
/// Returns `Err` if the user's config cannot be read.
///
/// # Panics
/// This function must be called while the process is still
/// single-threaded: the credential scrub mutates the environment, and
/// that is only safe before any other thread — the transport runtime, a
/// session's worker threads — has been created.
pub fn prepare() -> Result<CredentialStore, String> {
    let custom = exarch::config::load()?;
    Ok(CredentialStore::resolve_and_scrub(custom))
}

/// One model offered for a provider, and whether it takes a reasoning-effort
/// control.
///
/// `reasoning` reads `true` whenever the pricing catalog has not positively
/// said otherwise — before [`pricing::ensure_loaded`] completes, or for a
/// model the catalog's fetch never listed, [`pricing::caps_or_default`]
/// returns an empty capability record and [`pricing::ModelCaps::supports`]
/// treats that as permission rather than refusal. This is the same
/// only-gray-on-positive-absence rule exarch's own `/model` picker applies:
/// absence of information is never evidence that a model lacks the
/// capability, so the effort control stays offered until told otherwise.
#[derive(serde::Serialize, Clone)]
pub struct ModelChoice {
    pub name: String,
    pub reasoning: bool,
}

/// One provider the window can offer, and the models known for it.
#[derive(serde::Serialize, Clone)]
pub struct ProviderChoice {
    /// A stable identifier that round-trips through
    /// [`Conversation::begin`]'s `choice`.
    pub key: String,
    /// [`ProviderId::label`].
    pub label: String,
    pub default_model: Option<String>,
    /// Whatever the catalog honestly knows for this provider — at minimum
    /// the famous default, never blocking on a network fetch to build.
    pub models: Vec<ModelChoice>,
}

/// The provider picker: one entry per available account, plus the shared
/// effort ladder every entry's models offer a rung from.
#[derive(serde::Serialize, Clone)]
pub struct ModelMenu {
    pub providers: Vec<ProviderChoice>,
    /// [`provider::EFFORT_LADDER`]'s labels, ascending — the rungs
    /// [`Choice::effort`] may name.
    pub efforts: Vec<String>,
    /// [`provider::default_effort_label`] — the rung a freshly-opened
    /// control should land on.
    pub default_effort: String,
}

/// The provider picker as it can be shown the instant the window opens.
///
/// No network touched: each provider's models come from whatever `catalog`
/// already has cached — a fresh disk entry carried over from an earlier
/// session, or nothing at all — merged with the famous default so a
/// provider with no cache still offers its one well-known model.
/// [`refresh_menu`] is the complete listing, fetched live; this is the
/// instant one the window shows while that runs.
pub fn menu<S>(store: &CredentialStore, catalog: &mut ModelCatalog<S>) -> ModelMenu
where
    S: ModelSource,
{
    menu_from(&store.available(), catalog)
}

/// The complete provider picker: every available provider's live model
/// list, fetched from the network wherever the catalog has nothing cached.
///
/// Locks `catalog` only twice, and only briefly — once to open the
/// [`Listing`] (seeding from cache, spawning a background fetch per miss),
/// once to fold the fetches' results back in — never while
/// [`Listing::settle`] blocks on the network in between, so a concurrent
/// instant [`menu`] call is never held up behind this one's fetches.
pub fn refresh_menu<S>(store: &CredentialStore, catalog: &Mutex<ModelCatalog<S>>) -> ModelMenu
where
    S: ModelSource + Clone + Send + 'static,
{
    let available = store.available();
    refresh_menu_for(&available, catalog)
}

/// [`refresh_menu`]'s body, over a provider list rather than a store — so
/// the fetch/fold/shape logic is exercised directly, with a fake
/// [`ModelSource`] and no [`CredentialStore`] to stand up.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the guard is deliberately held from the fold-in loop through menu_from's read of the same catalog — one lock for both, not one per use"
)]
fn refresh_menu_for<S>(available: &[ProviderId], catalog: &Mutex<ModelCatalog<S>>) -> ModelMenu
where
    S: ModelSource + Clone + Send + 'static,
{
    let listing = {
        let mut catalog = lock(catalog);
        Listing::open(available.to_owned(), &mut catalog)
    };
    let results = listing.settle();

    // Best effort, and off the lock: the [`ModelChoice::reasoning`] flags
    // [`menu_from`] computes below read this catalog, so it should be
    // loaded before that runs wherever loading it is possible at all.
    ensure_pricing_loaded();

    let mut catalog = lock(catalog);
    for (id, result) in results {
        if let Ok(models) = result {
            catalog.record(&id, models);
        }
    }
    menu_from(available, &mut catalog)
}

/// Shape `available` into a [`ModelMenu`], reading each provider's model
/// list from `catalog` without ever fetching — the part [`menu`] and
/// [`refresh_menu_for`] share once each has decided what belongs in the
/// catalog.
fn menu_from<S>(available: &[ProviderId], catalog: &mut ModelCatalog<S>) -> ModelMenu
where
    S: ModelSource,
{
    let providers = available
        .iter()
        .map(|id| provider_choice(id, catalog))
        .collect();
    ModelMenu {
        providers,
        efforts: provider::EFFORT_LADDER
            .iter()
            .map(|(label, _)| label.to_string())
            .collect(),
        default_effort: provider::default_effort_label().to_string(),
    }
}

/// One provider's entry: its cached models (if any) merged with its famous
/// default, each carrying whether the pricing catalog knows it reasons.
fn provider_choice<S>(id: &ProviderId, catalog: &mut ModelCatalog<S>) -> ProviderChoice
where
    S: ModelSource,
{
    let default_model = id.famous().map(|kind| kind.info().1.to_string());
    let cached = catalog.cached(id).unwrap_or_default();
    let models = merged_models(default_model.as_deref(), cached)
        .into_iter()
        .map(to_model_choice)
        .collect();
    ProviderChoice {
        key: id.label().to_string(),
        label: id.label().to_string(),
        default_model,
        models,
    }
}

/// `default`, first, then `cached` in its own order — deduplicated so the
/// default never appears twice, whether because `cached` already listed it
/// or because it was merged in on an earlier call over the same catalog
/// entry.
fn merged_models(default: Option<&str>, cached: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(cached.len() + 1);
    if let Some(default) = default {
        seen.insert(default.to_string());
        out.push(default.to_string());
    }
    for model in cached {
        if seen.insert(model.clone()) {
            out.push(model);
        }
    }
    out
}

fn to_model_choice(name: String) -> ModelChoice {
    let reasoning = pricing::caps_or_default(&name).supports("reasoning");
    ModelChoice { name, reasoning }
}

/// Lock `catalog`, recovering the guard even if a prior holder panicked —
/// the codebase's established pattern for a lock whose data outlives any
/// one thread's confusion about it.
fn lock<S: ModelSource>(
    catalog: &Mutex<ModelCatalog<S>>,
) -> std::sync::MutexGuard<'_, ModelCatalog<S>> {
    catalog
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Load exarch's `OpenRouter` pricing/capability catalog, if this process
/// has not already — best effort, on a throwaway current-thread runtime,
/// the same shape [`exarch::provider::models::LiveSource`]'s own network
/// calls use rather than holding one open for a picker's whole lifetime.
///
/// Every [`ModelChoice::reasoning`] flag and [`Conversation::begin`]'s
/// effort mask read this catalog through [`pricing::caps_or_default`];
/// before it loads (or if even building a runtime fails) that read comes
/// back empty, which the same function already treats as "unknown", not
/// "unsupported" — so a caller here never blocks a selection, only misses
/// the mask it would otherwise have applied.
fn ensure_pricing_loaded() {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(pricing::ensure_loaded());
}

/// One provider, model, and (optionally) reasoning effort, chosen from
/// [`menu`] or [`refresh_menu`]'s listing and handed to
/// [`Conversation::begin`].
///
/// `effort`'s absence and `effort: Some("auto")` are deliberately distinct:
/// leaving it unset carries [`provider::Tuning::initial`]'s thinking-on
/// default forward untouched, exactly as an unspecified choice always has;
/// naming `"auto"` is a request to send no reasoning option on the wire at
/// all, landing on `effort: None` the same way, but *chosen* rather than
/// defaulted.
#[derive(serde::Deserialize, Clone)]
pub struct Choice {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
}

/// What the window says before the first message: the folder, the
/// boundary, and who is answering.
pub struct Opening {
    /// The granted folder, display form.
    pub folder: String,
    /// "Protection: {boundary}." — always [`HARDWARE_PROTECTION`].
    pub boundary_line: String,
    /// "Assistant: {label} ({model})."
    pub assistant_line: String,
    /// The ~2GiB warning sentence, when the folder is that large.
    pub large_folder_line: Option<String>,
}

/// One folder, held open over a booted machine and an agent, from the
/// first message to the last.
pub struct Conversation {
    grant: Grant,
    machine: Box<dyn Machine>,
    agent: Agent,
    engine: Arc<Engine>,
    history: workspace::HistoryStore,
}

impl Conversation {
    /// Open `folder`, boot the best machine this computer can hold it in,
    /// and start the agent over it.
    ///
    /// `choice` names a provider, model, and effort from [`menu`]'s or
    /// [`refresh_menu`]'s listing; `None` reproduces the old behaviour —
    /// whichever one account is set up on this computer, its default model,
    /// and [`provider::Tuning::initial`]'s thinking-on effort. A chosen
    /// effort that [`provider::pricing::caps_or_default`] positively knows
    /// the model does not take is masked to `None` regardless of what was
    /// asked for — the model would otherwise refuse the request outright.
    ///
    /// # Errors
    /// Returns `Err` if this computer cannot start a virtual machine at all
    /// — the wrong platform, missing boot media, or an unsigned build — if
    /// the folder cannot be granted, if no model account is set up (or a
    /// named one has vanished), if the chosen effort names no rung on
    /// [`provider::EFFORT_LADDER`], if the scratch or log directories cannot
    /// be made, if the system prompt cannot be assembled, or if the agent
    /// cannot be started.
    ///
    /// # Panics
    /// Panics if the chosen provider is absent from `store` — an invariant
    /// [`choose`] upholds by choosing only among available ones, and
    /// [`resolve_pinned_provider`] upholds by resolving only against the
    /// same list.
    pub fn begin(
        folder: &Path,
        store: &CredentialStore,
        choice: Option<Choice>,
    ) -> Result<(Self, Opening), String> {
        let grant = Grant::open(folder)?;
        let hypervisor = vm_manager::detect(crate::boot_media())?;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut machine = hypervisor.boot(&grant.machine_spec()).map_err(|e| {
            format!(
                "could not start a machine for {}: {e}",
                grant.root().display()
            )
        })?;

        // Said before any work happens, because it is about what is
        // *about* to happen to the user's own documents.
        let boundary_line = format!("Protection: {HARDWARE_PROTECTION}.");

        // The agent works where the machine put the folder: the guest's
        // `/work`.
        let workspace = machine.workspace_path().to_path_buf();
        let cwd = workspace.to_string_lossy().into_owned();

        let disk_warn_bytes = exarch::config::disk_warn_bytes()?;
        // The IT-set fetch-url policy, audit ledger, and rate budget — one
        // file regardless of which front-end is running, opened once here.
        let egress = exarch::fleet::egress::Egress::open(SYNOD)?;

        let available = store.available();
        if available.is_empty() {
            return Err(
                "no model account is set up on this computer — ask your IT department \
                        to set a provider API key (ANTHROPIC_API_KEY, OPENAI_API_KEY, …)"
                    .into(),
            );
        }
        let (id, model, effort) = if let Some(Choice {
            provider,
            model,
            effort,
        }) = choice
        {
            (
                resolve_pinned_provider(&provider, &available)?,
                model,
                effort,
            )
        } else {
            let (id, model) = choose(&available)?;
            (id, model, None)
        };
        let cred = store
            .get(&id)
            .expect("the chosen provider is one of the available ones")
            .clone();
        let assistant_line = format!("Assistant: {} ({model}).", id.label());
        let tuning = resolve_tuning(effort, &model)?;

        let run_dir = SYNOD
            .log_run_dir(&cwd)
            .map_err(|e| format!("could not make a log folder: {e}"))?;
        let config_dir = SYNOD.xdg_dir(ral_core::path::basedir::XdgKind::Config);

        // Everything the agent is told, and everything it is allowed, is in
        // the guest's namespace: the engine lives there, and the host path
        // of the folder names nothing inside it.
        let caps = grant.capabilities();
        let system = crate::prompt::assemble(&caps, &workspace, &grant.name(), &config_dir)?;

        // The agent's engine dials in from inside the guest, so the
        // workspace is a guest path — never a directory this host process
        // could `chdir` into; the trunk drives it over the wire the
        // machine hands back.
        #[cfg(unix)]
        let root_seat = {
            let fd = machine.take_control();
            exarch::agent::RootSeat::Wire {
                transport: Box::new(
                    ral_core::transport::WireTransport::adopt(
                        std::os::unix::net::UnixStream::from(fd),
                        ral_core::transport::Liveness::default(),
                    )
                    .map_err(|e| format!("could not take control of the machine: {e}"))?,
                ),
                cwd: workspace,
                // Home is the guest scratch, not the workspace: `$HOME` is
                // where XDG-defaulting tools drop caches and dotfiles, and
                // pointed at `/work` that litter would land among the
                // user's own documents — and in every change report.
                home: std::path::PathBuf::from(crate::grant::GUEST_SCRATCH),
            }
        };
        // `WireTransport` is unix-only, and so is every seat that can drive
        // one: there is no non-unix way to reach the machine's control
        // plane, so a build for this platform can only refuse honestly.
        #[cfg(not(unix))]
        let root_seat: exarch::agent::RootSeat = {
            return Err(
                "synod reaches its virtual machine over a socket this operating \
                         system does not provide — synod cannot run here"
                    .to_string(),
            );
        };

        let engine = Engine::new();
        let provider = Arc::new(Provider::build(
            engine.clone(),
            &id,
            model.clone(),
            &cred,
            None,
            tuning,
            None,
        ));
        let agent = Agent::root(
            exarch::agent::RootConfig {
                system,
                caps,
                run_dir,
                model,
                provider_label: id.label().to_string(),
                // Synod's agent may not schedule its own wakeups: a
                // conversing office assistant still runs on nothing but
                // the messages it is handed, never on its own authority.
                allow_schedule: false,
                // A conversation, not a job: the agent converses,
                // withholding `reply` and parking between messages rather
                // than returning once — [`exarch::headless::converse_sink`]
                // drives one exchange at a time over this same session.
                interactive: true,
                chat: false,
                disk_warn_bytes,
                // A conversation is asked and answered, one message at a
                // time, never a fleet: no sub-agent may ever start from it.
                fuel: 0,
                egress,
            },
            root_seat,
            Arc::clone(&provider),
        )
        .map_err(|e| format!("could not start the assistant: {e}"))?;

        // Checkpoint the folder before any work: the baseline every
        // exchange's report is judged against, and the state every undo
        // returns to.  Taken once, for the whole conversation — this late
        // so a setup failure above costs no folder walk.
        let folder_measure = workspace::measure(grant.root())?;
        let large_folder_line = (folder_measure.bytes > workspace::LARGE_FOLDER_BYTES).then(|| {
            let tenths = folder_measure.bytes / 100_000_000;
            format!(
                "This folder holds about {}.{} GB across {} files.  Synod keeps a \
                 copy of everything before starting, which can take a while — \
                 possibly minutes on a shared drive.",
                tenths / 10,
                tenths % 10,
                folder_measure.files
            )
        });
        let history = workspace::HistoryStore::open_for(grant.root())?;
        history.capture(grant.root(), workspace::Moment::Before)?;

        let opening = Opening {
            folder: grant.root().to_string_lossy().into_owned(),
            boundary_line,
            assistant_line,
            large_folder_line,
        };

        Ok((
            Self {
                grant,
                machine,
                agent,
                engine,
                history,
            },
            opening,
        ))
    }

    /// Drive one message through the conversation, streaming the bus's
    /// events into the caller's `sink` in order — the same events
    /// [`exarch::headless::converse_sink`] always drives one exchange
    /// through.
    ///
    /// Checkpoints what this exchange left behind, cumulatively from the
    /// baseline — taken even after a failed exchange, since whatever
    /// changed before the failure is still undoable.  Renders no report:
    /// the window reads one back through [`workspace::job_report`].
    ///
    /// # Errors
    /// Returns `Err` if the exchange itself fails; if the exchange
    /// succeeded but the after-checkpoint could not be taken, that error
    /// is returned instead.
    pub fn exchange<S: exarch::bus::Sink>(
        &mut self,
        message: String,
        sink: &mut S,
    ) -> Result<(), String> {
        let outcome =
            exarch::headless::converse_sink(&mut self.agent, message, self.engine.clone(), sink);
        match self
            .history
            .capture(self.grant.root(), workspace::Moment::After)
        {
            Ok(_) => {}
            Err(e) => {
                if outcome.is_ok() {
                    return Err(e);
                }
            }
        }
        outcome
    }

    /// Shut the machine down, ending the conversation.
    ///
    /// Drops the agent first: under a real VM its seat owns the wire, and
    /// closing that end is what makes the guest's engine see EOF and power
    /// the machine off from the inside — the same inside-out shutdown
    /// `boot-turn`'s own drop-then-stop performs, so the grace window
    /// `machine.shutdown` waits on below normally observes a stop already
    /// under way rather than forcing one.
    ///
    /// # Errors
    /// Returns `Err` if the machine does not stop cleanly — a failure
    /// this never swallows.
    pub fn end(self) -> Result<(), String> {
        let Self { machine, agent, .. } = self;
        drop(agent);
        machine
            .shutdown()
            .map_err(|e| format!("the machine did not stop cleanly: {e}"))
    }
}

/// The provider and model for a run whose [`Choice`] left both unnamed:
/// whichever one account is set up on this computer, and its default
/// model. An account that names no default model is a question for the
/// user, refused in the same plain register as having no account at all —
/// there is no menu entry left to answer it with.
fn choose(available: &[ProviderId]) -> Result<(ProviderId, String), String> {
    let id = available[0].clone();
    let model = id.famous().map(|kind| kind.info().1.to_string());
    model.map(|model| (id.clone(), model)).ok_or_else(|| {
        format!(
            "the account set up on this computer ('{}') does not say which model to \
             use — ask your IT department to set one up.",
            id.label()
        )
    })
}

/// The tuning [`Choice::effort`] resolves to, masked against what the
/// pricing catalog positively knows `model` supports.
///
/// An absent `effort` carries [`provider::Tuning::initial`]'s thinking-on
/// default forward untouched; `Some(label)` resolves strictly against
/// [`provider::EFFORT_LADDER`] — `"auto"` lands on `effort: None`
/// deliberately, distinct from the absent case landing on
/// [`provider::Tuning::initial`]'s `Some(Medium)`. Loads the pricing
/// catalog first (best effort — see [`ensure_pricing_loaded`]), then masks
/// the resolved effort to `None` when [`pricing::caps_or_default`]
/// positively reports the model does not take reasoning at all; before the
/// catalog loads, or on a lookup miss, that call reads the model as
/// reasoning-capable and no masking happens.
///
/// # Errors
/// Returns `Err` if `effort` names no rung on [`provider::EFFORT_LADDER`].
fn resolve_tuning(effort: Option<String>, model: &str) -> Result<provider::Tuning, String> {
    let tuning = match effort {
        None => provider::Tuning::initial(),
        Some(label) => provider::Tuning {
            effort: provider::effort_by_label(&label)?,
            temperature: None,
            top_p: None,
        },
    };
    ensure_pricing_loaded();
    Ok(mask_unsupported_effort(
        tuning,
        pricing::caps_or_default(model).supports("reasoning"),
    ))
}

/// Force `tuning.effort` to `None` when `reasoning` is `false`, leaving
/// every other field untouched — the actual masking step
/// [`resolve_tuning`] applies once it has learned whether the model takes a
/// reasoning control at all. Split out from that lookup so the masking
/// itself has a seam a test can reach without needing the pricing
/// catalog's own network-fetched, process-global snapshot to have loaded a
/// model that positively lacks the parameter.
fn mask_unsupported_effort(mut tuning: provider::Tuning, reasoning: bool) -> provider::Tuning {
    if !reasoning {
        tuning.effort = None;
    }
    tuning
}

#[cfg(test)]
mod tests {
    use super::*;
    use exarch::provider::models::ProviderEndpoint;
    use exarch::provider::{ChatGptAccount, ProviderKind, ReasoningEffort};
    use std::collections::BTreeMap;

    /// A famous provider's id — the common case in these tests.
    fn fam(kind: ProviderKind) -> ProviderId {
        ProviderId::Famous(kind)
    }

    /// A signed-in `ChatGPT` account's id — [`ProviderId::famous`] reads
    /// `None` for it, so it names no famous default and stands in for
    /// every provider kind [`menu_from`] cannot fall back on.
    fn chatgpt(label: &str) -> ProviderId {
        ProviderId::ChatGpt(Arc::new(ChatGptAccount {
            account_id: label.to_string(),
            label: label.to_string(),
        }))
    }

    type Lists = BTreeMap<ProviderId, Result<Vec<String>, String>>;

    /// A fake [`ModelSource`] whose list is shared (not forked) across a
    /// clone, so a background-fetch thread run by [`Listing::open`] serves
    /// the same lists the test set up.
    #[derive(Clone)]
    struct FakeSource {
        lists: Arc<Mutex<Lists>>,
    }

    impl FakeSource {
        fn new(lists: Lists) -> Self {
            Self {
                lists: Arc::new(Mutex::new(lists)),
            }
        }
    }

    impl ModelSource for FakeSource {
        fn list(&self, id: &ProviderId) -> Result<Vec<String>, String> {
            lock_lists(&self.lists)
                .get(id)
                .cloned()
                .unwrap_or_else(|| Err("no fake list".into()))
        }

        fn endpoints(&self, _model: &str) -> Result<Vec<ProviderEndpoint>, String> {
            Err("not exercised by these tests".into())
        }
    }

    fn lock_lists(lists: &Arc<Mutex<Lists>>) -> std::sync::MutexGuard<'_, Lists> {
        lists
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn one(id: ProviderId, models: &[&str]) -> Lists {
        let mut m = BTreeMap::new();
        m.insert(id, Ok(models.iter().map(ToString::to_string).collect()));
        m
    }

    fn model_names(choice: &ProviderChoice) -> Vec<String> {
        choice.models.iter().map(|m| m.name.clone()).collect()
    }

    #[test]
    fn menu_with_nothing_cached_offers_the_famous_default_alone() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let available = [fam(ProviderKind::Anthropic)];

        let menu = menu_from(&available, &mut catalog);

        assert_eq!(menu.providers.len(), 1);
        assert_eq!(
            model_names(&menu.providers[0]),
            vec![ProviderKind::Anthropic.info().1.to_string()]
        );
        assert_eq!(menu.efforts.first().map(String::as_str), Some("auto"));
        assert_eq!(menu.default_effort, "med");
    }

    #[test]
    fn menu_with_a_cached_list_puts_the_default_first_and_dedupes_it() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let default = ProviderKind::Anthropic.info().1.to_string();
        catalog.record(
            &fam(ProviderKind::Anthropic),
            vec!["claude-haiku-4".to_string(), default.clone()],
        );

        let menu = menu_from(&[fam(ProviderKind::Anthropic)], &mut catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            vec![default, "claude-haiku-4".to_string()]
        );
    }

    #[test]
    fn a_chatgpt_style_provider_with_no_famous_default_starts_empty() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let id = chatgpt("work-account");

        let menu = menu_from(std::slice::from_ref(&id), &mut catalog);

        assert!(menu.providers[0].default_model.is_none());
        assert!(model_names(&menu.providers[0]).is_empty());
    }

    #[test]
    fn refresh_menu_folds_fetched_lists_in_and_serves_them() {
        let id = chatgpt("work-account");
        let source = FakeSource::new(one(id.clone(), &["gpt-5.5-codex"]));
        let catalog = Mutex::new(ModelCatalog::memo_only(source));

        let menu = refresh_menu_for(std::slice::from_ref(&id), &catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            vec!["gpt-5.5-codex".to_string()]
        );
        assert_eq!(
            lock(&catalog).cached(&id),
            Some(vec!["gpt-5.5-codex".to_string()])
        );
    }

    #[test]
    fn refresh_menu_leaves_a_failed_fetch_uncached_but_still_shows_the_default() {
        let mut lists = Lists::new();
        lists.insert(fam(ProviderKind::Deepseek), Err("network down".to_string()));
        let catalog = Mutex::new(ModelCatalog::memo_only(FakeSource::new(lists)));

        let menu = refresh_menu_for(&[fam(ProviderKind::Deepseek)], &catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            vec![ProviderKind::Deepseek.info().1.to_string()]
        );
        assert_eq!(lock(&catalog).cached(&fam(ProviderKind::Deepseek)), None);
    }

    #[test]
    fn resolve_tuning_with_no_effort_keeps_the_thinking_on_default() {
        let tuning = resolve_tuning(None, "claude-opus-4").unwrap();
        assert_eq!(tuning, provider::Tuning::initial());
    }

    #[test]
    fn resolve_tuning_rejects_an_unknown_effort_label() {
        let err = resolve_tuning(Some("extreme".into()), "claude-opus-4").unwrap_err();
        assert!(err.contains("extreme"), "got: {err}");
    }

    #[test]
    fn resolve_tuning_auto_is_a_deliberate_none_not_an_absence() {
        let tuning = resolve_tuning(Some("auto".into()), "claude-opus-4").unwrap();
        assert!(tuning.effort.is_none());
        assert_ne!(
            tuning,
            provider::Tuning::initial(),
            "an explicit 'auto' must not read back as the thinking-on default"
        );
    }

    #[test]
    fn mask_unsupported_effort_clears_only_the_effort() {
        let tuning = provider::Tuning {
            effort: Some(ReasoningEffort::Medium),
            temperature: Some(0.5),
            top_p: None,
        };

        let masked = mask_unsupported_effort(tuning.clone(), false);
        assert!(masked.effort.is_none());
        assert_eq!(masked.temperature, Some(0.5));

        let kept = mask_unsupported_effort(tuning, true);
        assert!(matches!(kept.effort, Some(ReasoningEffort::Medium)));
    }
}
