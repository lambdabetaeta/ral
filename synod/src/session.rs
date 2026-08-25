//! The conversation itself: one folder, one agent, and the exchanges the
//! window drives between them.
//!
//! Synod's session is exarch's session with the developer removed.  The
//! provider transport, the exchange driver, and the card bus are exarch's
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
//!
//! The store the whole module reads is held behind a [`Mutex`] because
//! [`sign_in`] can add to it: a `ChatGPT` plan signed in from the window
//! becomes available to the very next [`menu`] and conversation, with no
//! restart.  Every function here that takes it locks it only for as long
//! as it takes to read the account list — never across a network fetch or
//! a machine boot.

use crate::grant::Grant;
use crate::workspace;
use exarch::agent::{Agent, RecordedAccount};
use exarch::bootstrap;
use exarch::provider::{
    self, Engine, Provider,
    credential::{Credential, CredentialStore},
    identity::{self, Account},
    listing::Listing,
    models::{LiveSource, ModelCatalog, ModelSource, resolve_account},
    oauth, pricing,
};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Synod's own directories.
///
/// `$XDG_STATE_HOME/synod/<folder>/` for the run logs.  A synod run must
/// never write into an exarch run's logs, nor read its model selection.
/// The agent's working area is not among these: it is the guest's own
/// scratch tmpfs ([`crate::grant::GUEST_SCRATCH`]), no host directory.
pub const SYNOD: bootstrap::App = bootstrap::App::new("synod");

/// Resolve the credential store, once, at startup — see
/// [`crate::accounts::prepare`], which is where synod's accounts actually
/// come from.
///
/// # Errors
/// Returns `Err` if synod's own provider settings cannot be read.
///
/// # Panics
/// This function must be called while the process is still
/// single-threaded: the credential scrub mutates the environment, and
/// that is only safe before any other thread — the transport runtime, a
/// session's worker threads — has been created.
pub fn prepare() -> Result<CredentialStore, String> {
    crate::accounts::prepare()
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

/// One account the window can offer, and the models known for it.
#[derive(serde::Serialize, Clone)]
pub struct ProviderChoice {
    /// The [`AccountId`](exarch::provider::identity::AccountId) rendering —
    /// the identifier that round-trips as [`Choice::account`] through
    /// [`Conversation::begin`], which resolves it back to an [`Account`] via
    /// [`resolve_account`]. Never displayed; [`Self::label`] is.
    pub account: String,
    /// What the window shows for this entry — [`identity::label`],
    /// set-relative to every account [`menu`] or [`refresh_menu`] offers.
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
pub fn menu<S>(store: &Mutex<CredentialStore>, catalog: &Mutex<ModelCatalog<S>>) -> ModelMenu
where
    S: ModelSource,
{
    let available = lock(store).available();
    menu_from(&available, &mut lock(catalog))
}

/// The complete provider picker: every available provider's live model
/// list, fetched from the network wherever the catalog has nothing cached.
///
/// Locks `catalog` only twice, and only briefly — once to open the
/// [`Listing`] (seeding from cache, spawning a background fetch per miss),
/// once to fold the fetches' results back in — never while
/// [`Listing::settle`] blocks on the network in between, so a concurrent
/// instant [`menu`] call is never held up behind this one's fetches.
/// `store` is read once, up front, for the account list alone: a sign-in
/// running alongside this fetch waits on nothing.
pub fn refresh_menu<S>(
    store: &Mutex<CredentialStore>,
    catalog: &Mutex<ModelCatalog<S>>,
) -> ModelMenu
where
    S: ModelSource + Clone + Send + 'static,
{
    let available = lock(store).available();
    refresh_menu_for(&available, catalog)
}

/// [`refresh_menu`]'s body, over a provider list rather than a store — so
/// the fetch/fold/shape logic is exercised directly, with a fake
/// [`ModelSource`] and no [`CredentialStore`] to stand up.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the guard is deliberately held from the fold-in loop through menu_from's read of the same catalog — one lock for both, not one per use"
)]
fn refresh_menu_for<S>(available: &[Account], catalog: &Mutex<ModelCatalog<S>>) -> ModelMenu
where
    S: ModelSource + Clone + Send + 'static,
{
    let listing = {
        let mut catalog = lock(catalog);
        let ids = available.iter().map(|account| account.id.clone()).collect();
        Listing::open(ids, &mut catalog)
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
fn menu_from<S>(available: &[Account], catalog: &mut ModelCatalog<S>) -> ModelMenu
where
    S: ModelSource,
{
    let providers = available
        .iter()
        .map(|account| provider_choice(account, available, catalog))
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

/// One account's entry: its cached models (if any) merged with its
/// service's default, each carrying whether the pricing catalog knows it
/// reasons.
fn provider_choice<S>(
    account: &Account,
    available: &[Account],
    catalog: &mut ModelCatalog<S>,
) -> ProviderChoice
where
    S: ModelSource,
{
    let default_model = account.service.default_model.clone();
    let cached = catalog.cached(&account.id).unwrap_or_default();
    let models = merged_models(default_model.as_deref(), cached)
        .into_iter()
        .map(to_model_choice)
        .collect();
    ProviderChoice {
        account: account.id.as_str().to_string(),
        label: identity::label(account, available),
        default_model,
        models,
    }
}

/// `default` first, then `cached` in its own order — filtered so the
/// default never appears twice when `cached` already lists it.
fn merged_models(default: Option<&str>, cached: Vec<String>) -> Vec<String> {
    default
        .map(str::to_string)
        .into_iter()
        .chain(cached.into_iter().filter(|m| Some(m.as_str()) != default))
        .collect()
}

fn to_model_choice(name: String) -> ModelChoice {
    let reasoning = pricing::caps_or_default(&name).supports("reasoning");
    ModelChoice { name, reasoning }
}

/// One step of a sign-in in progress, in the words the window says out
/// loud.
#[derive(Clone, serde::Serialize)]
pub struct SignInStep {
    /// What the window should say while this is the step in hand.
    pub say: String,
    /// The sign-in link, for the window to offer alongside its prose.
    pub link: Option<String>,
}

impl From<oauth::LoginPhase> for SignInStep {
    fn from(phase: oauth::LoginPhase) -> Self {
        match phase {
            oauth::LoginPhase::AwaitingBrowser { url } => Self {
                say: "Finish signing in, in your browser.  If no window opened, \
                      follow this link:"
                    .to_string(),
                link: Some(url),
            },
            // The window never runs the device flow — a sign-in in a window
            // is a sign-in on the machine the browser is on — so this arm
            // completes the phase vocabulary rather than describing
            // anything synod shows.
            oauth::LoginPhase::AwaitingDevice {
                user_code,
                url,
                expires_in,
            } => Self {
                say: format!(
                    "Follow this link and enter the code {user_code} to sign in.  \
                     The code expires in {expires_in}."
                ),
                link: Some(url),
            },
            oauth::LoginPhase::ExchangingCode => Self {
                say: "Signing you in…".to_string(),
                link: None,
            },
        }
    }
}

/// A finished sign-in, as the window reports it.
#[derive(Clone, serde::Serialize)]
pub struct SignedIn {
    /// The signed-in account's [`identity::label`], the name [`menu`] lists
    /// it under. A display string — the wire says so, so no window is ever
    /// tempted to hand it back as a [`Choice::account`].
    pub label: String,
    /// Whether this refreshed the login for an account already set up here,
    /// rather than adding a new one.
    pub replaced: bool,
}

/// Sign in to a `ChatGPT` plan and admit the account to this run.
///
/// The flow is exarch's ([`oauth::login_flow`]): it opens the user's
/// browser, waits on the loopback callback, exchanges the code, and stores
/// the token where `exarch login` stores it, so a computer signed in here
/// is signed in for both.  It blocks — for as long as the user takes at
/// their browser — so the caller runs it on a thread of its own,
/// `on_phase` carrying each step to the window and `cancel` the abandon
/// flag the flow's waits poll.
///
/// The last step is synod's own: the fresh token goes into the live store
/// and into the catalog built from it, so the account appears in the very
/// next [`menu`] and can open the very next conversation.  Nothing here
/// re-runs [`prepare`] — its scrub is only safe on a single-threaded
/// process, and this one has long since stopped being one — which is why
/// the account is admitted rather than re-resolved.
///
/// # Errors
/// Returns the flow's own sentence: a refused or abandoned sign-in, a
/// browser that never came back, a network that would not carry the
/// exchange.
pub fn sign_in(
    store: &Mutex<CredentialStore>,
    catalog: &Mutex<ModelCatalog<LiveSource>>,
    on_phase: impl Fn(SignInStep),
    cancel: &Arc<AtomicBool>,
) -> Result<SignedIn, String> {
    let (token, replaced) = oauth::login_flow(
        oauth::LoginMethod::Browser,
        |phase| on_phase(SignInStep::from(phase)),
        cancel,
    )?;
    let (admitted, credential) = lock(store).add_oauth(&token);
    // The store's name for it, which says which account when two share an email.
    let label = identity::label(&admitted, &lock(store).available());
    lock(catalog).add_credential(admitted, credential);
    Ok(SignedIn { label, replaced })
}

/// Lock `m`, recovering the guard even if a prior holder panicked — the
/// codebase's established pattern for a lock whose data outlives any one
/// thread's confusion about it.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
#[derive(serde::Deserialize)]
pub struct Choice {
    /// An [`AccountId`](exarch::provider::identity::AccountId) rendering, as
    /// handed out in a [`ProviderChoice::account`] — resolved back to an
    /// [`Account`] by [`resolve_account`], id only, never a name: two
    /// accounts can share a display label, and starting the wrong one's
    /// conversation is exactly the bug this type exists to prevent.
    pub account: String,
    pub model: String,
    pub effort: Option<String>,
}

/// What the window shows before the first message: who is answering, at
/// what effort, and the ~2GiB warning when the folder is that large.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Opening {
    /// The answering account's [`identity::label`], set-relative to every
    /// account available when the conversation began. A display string, and
    /// named as one on the wire, unlike [`Choice::account`]'s id.
    pub label: String,
    /// The model that account is driving.
    pub model: String,
    /// The [`provider::EFFORT_LADDER`] label of the effort actually in
    /// force, after [`resolve_tuning`]'s masking — not what was asked for,
    /// which the window already knows and which a model that takes no
    /// reasoning control never receives.
    pub effort: String,
    /// The ~2GiB warning sentence, when the folder is that large.
    pub large_folder_line: Option<String>,
}

/// The net wire's stream type, adopted from [`vm_manager::Wires::net`]: an
/// `OwnedFd` wearing an `AF_VSOCK` connection on Unix, an `OwnedSocket`
/// wearing an `AF_HYPERV` one on Windows — [`guest_net::device::Wire`] is
/// implemented for both, the same pretence [`ral_core::wire::WireStream`]
/// makes for the control plane.
#[cfg(unix)]
type NetWire = std::os::unix::net::UnixStream;
#[cfg(windows)]
type NetWire = std::net::TcpStream;

/// One folder, held open over a booted machine and an agent, from the
/// first message to the last.
pub struct Conversation {
    grant: Grant,
    /// The machine, held in trust by the dialler: [`Conversation::end`]
    /// recovers it via [`crate::machine_dial::MachineDial::into_machine`],
    /// the only other owner being the [`Agent`] this conversation also
    /// holds, which drops first.
    dial: Arc<crate::machine_dial::MachineDial>,
    agent: Agent,
    engine: Arc<Engine>,
    baseline: Baseline,
    /// The guest's whole network, running on its own threads since before
    /// the first exchange — see [`net_seat`].
    net: guest_net::Session<NetWire>,
}

/// The folder's baseline capture: started the moment [`Conversation::begin`]
/// opens the store, alongside the machine's boot, and joined the first time
/// [`Conversation::exchange`] or [`Conversation::end`] needs the settled
/// store — never before, since the guest must not touch the folder while
/// its baseline is still being read.
enum Baseline {
    Pending(std::thread::JoinHandle<CaptureResult>),
    Ready(workspace::HistoryStore),
    /// The capture itself failed — a read error, a full disk — but the
    /// store it was capturing into came back regardless, since
    /// [`workspace::HistoryStore::capture`] only borrows it; `end` still
    /// has something to wipe.
    Failed(workspace::HistoryStore, String),
    /// The capture thread panicked outright, taking its store down with
    /// it: unwinding dropped that store's lock the same as a clean return
    /// would have, so the directory it leaves behind is unlocked, not
    /// leaked — [`workspace::history::sweep_stale`] collects it at the next
    /// start.
    Crashed(String),
    /// [`Baseline::settle`]'s placeholder for the instant between taking
    /// `Pending`'s handle and joining it — never observed outside that
    /// call.
    Settling,
}

/// What the capture thread hands back: the store it captured into,
/// regardless of outcome, and the capture's own result.
type CaptureResult = (
    workspace::HistoryStore,
    Result<workspace::history::Checkpoint, String>,
);

impl Baseline {
    /// The settled store, joining the capture thread the first time this is
    /// asked; every call after the first finds the settled state already
    /// waiting, so a settled baseline never blocks or re-reads anything.
    ///
    /// # Errors
    /// The capture's own error, or a plain sentence if its thread panicked
    /// before finishing.
    fn store(&mut self) -> Result<&workspace::HistoryStore, String> {
        self.settle();
        match self {
            Self::Ready(store) => Ok(store),
            Self::Failed(_, e) | Self::Crashed(e) => Err(e.clone()),
            Self::Pending(_) | Self::Settling => unreachable!("settled just above"),
        }
    }

    /// Join the capture thread the first time this is called, settling
    /// into [`Self::Ready`], [`Self::Failed`], or [`Self::Crashed`]; a call
    /// once already settled does nothing.
    fn settle(&mut self) {
        if let Self::Pending(_) = self {
            let Self::Pending(handle) = std::mem::replace(self, Self::Settling) else {
                unreachable!("just matched Pending above")
            };
            *self = match handle.join() {
                Ok((store, Ok(_before))) => Self::Ready(store),
                Ok((store, Err(e))) => Self::Failed(store, e),
                Err(_) => Self::Crashed(
                    "Synod's safety copy of the folder crashed while it was being read."
                        .to_string(),
                ),
            };
        }
    }

    /// The store [`Conversation::end`] wipes, settling first so a
    /// still-running capture is joined rather than abandoned — `None` only
    /// for [`Self::Crashed`], the one case with no store to hand back.
    fn into_store(mut self) -> Option<workspace::HistoryStore> {
        self.settle();
        match self {
            Self::Ready(store) | Self::Failed(store, _) => Some(store),
            Self::Crashed(_) | Self::Pending(_) | Self::Settling => None,
        }
    }
}

/// Abandon a baseline capture the rest of [`Conversation::begin`] never got
/// to use: joins its thread so no walk outlives the conversation that
/// started it, discarding whatever it found — the conversation is already
/// failing for its own reason. The store it was capturing into drops with
/// it, releasing the store's lock; [`workspace::history::sweep_stale`]
/// collects the now-unlocked directory at the next start.
fn abandon_baseline(handle: std::thread::JoinHandle<CaptureResult>) {
    let _ = handle.join();
}

/// The line to show before a conversation's one, unavoidable full read of
/// its folder — its store never survives past the conversation that opened
/// it, so this fires on nothing but the folder's own size.
///
/// Composed from a stat-only [`workspace::manifest::measure`], so it can
/// arrive before a single byte is read or copied.
fn opening_warning(measure: workspace::manifest::Measure, store_dir: &Path) -> Option<String> {
    if measure.bytes <= workspace::LARGE_FOLDER_BYTES {
        return None;
    }
    use std::fmt::Write as _;
    let mut line = format!(
        "This folder holds about {} across {} files.  Synod keeps a copy of \
         everything before starting, which can take a while — possibly minutes \
         on a shared drive.",
        as_gb(measure.bytes),
        measure.files
    );
    if let Some(free) = workspace::history::free_bytes(store_dir)
        && free < measure.bytes
    {
        let _ = write!(
            line,
            "  There may not be room for that safety copy: the folder holds about \
             {} but the disk has about {} free.",
            as_gb(measure.bytes),
            as_gb(free)
        );
    }
    Some(line)
}

/// `bytes` as a one-decimal gigabyte figure, the large-folder warning's unit
/// throughout.
fn as_gb(bytes: u64) -> String {
    let tenths = bytes / 100_000_000;
    format!("{}.{} GB", tenths / 10, tenths % 10)
}

/// Everything [`Conversation::begin`]'s boot-and-start closure produces,
/// besides the baseline capture running alongside it — split out so the
/// closure's own errors can be joined against the still-running capture
/// before they are reported, while `grant` (borrowed throughout the
/// closure) and the capture's `handle` (untouched by it) stay with the
/// caller.
struct Booted {
    dial: Arc<crate::machine_dial::MachineDial>,
    agent: Agent,
    engine: Arc<Engine>,
    net: guest_net::Session<NetWire>,
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
    /// be made, if the system prompt cannot be assembled, if the agent
    /// cannot be started, or if guest networking cannot start. Guest
    /// networking does not start degraded; a conversation with no network
    /// it can trust is refused outright rather than opened quietly without
    /// one. The before-checkpoint itself is not among these: its capture
    /// runs on past `begin`'s return, and [`Conversation::exchange`] reports
    /// its failure instead.
    ///
    /// # Panics
    /// Panics if the chosen account is absent from `store` — an invariant
    /// [`choose`] upholds by choosing only among available ones, and
    /// [`resolve_account`] upholds by resolving only against the same list —
    /// or if the resolved tuning's effort is one [`provider::EFFORT_LADDER`]
    /// does not name, which [`resolve_tuning`] cannot produce.
    pub fn begin(
        folder: &Path,
        store: &Mutex<CredentialStore>,
        choice: Option<Choice>,
    ) -> Result<(Self, Opening), String> {
        let grant = Grant::open(folder)?;
        // Handed as the *means* of readying the media rather than the media
        // itself.  On a computer where synod was installed, none of it is needed:
        // the machine service boots from its own copy, inflated once into a cache
        // every session on the computer shares, and readying it here would put
        // two and a half gigabytes into this user's cache for nothing.  Which of
        // those is the case is `vm_manager::detect`'s to know, so this hands over
        // the closure and lets it decide.
        let boot = crate::boot::boot_media()
            .map(|plan| -> vm_manager::BootMedia { Box::new(move || plan.realise()) });
        let hypervisor = vm_manager::detect(boot)?;

        let disk_warn_bytes = exarch::config::disk_warn_bytes()?;
        // The IT-set network policy, audit ledger, and rate budget — one
        // file regardless of which front-end is running, opened once here.
        let egress = exarch::egress::Egress::open(SYNOD)?;

        let Selected {
            account,
            label,
            model,
            effort,
            credential,
        } = select_account(store, choice)?;
        let announced_model = model.clone();
        let tuning = resolve_tuning(effort, &model)?;
        let announced_effort = provider::effort_label(&tuning.effort)
            .expect("resolve_tuning yields only efforts the ladder names")
            .to_string();

        let run_dir = SYNOD
            .log_run_dir(&grant.root().to_string_lossy())
            .map_err(|e| format!("could not make a log folder: {e}"))?;
        let config_dir = SYNOD.xdg_dir(ral_core::path::basedir::XdgKind::Config);

        // The store is fresh at every `begin` — it never outlives its own
        // conversation — so this folder's one full read is paid here every
        // time, and the large-folder warning below fires purely on size.
        let history = workspace::HistoryStore::open_for(grant.root())?;
        let measure = workspace::manifest::measure(grant.root())?;
        let large_folder_line = opening_warning(measure, history.dir());

        // The two slow arms of an opening wait on different things
        // entirely: the boot on a guest kernel coming up, the
        // before-checkpoint on every byte of the folder being read and
        // kept.  Neither needs the other, and nothing touches the folder
        // until the first exchange, so the capture starts now, before the
        // boot, and the conversation opens the moment the boot is done —
        // the capture goes on running, joined only once an exchange or
        // `end` needs the settled store. Every error path below still joins
        // it before reporting, so no walk is ever left running past the
        // conversation that started it.
        let root = grant.root().to_path_buf();
        let handle = std::thread::spawn(move || {
            let result = history.capture(&root, workspace::Moment::Before);
            (history, result)
        });

        let booted = (|| -> Result<Booted, String> {
            let mut machine = hypervisor.boot(&grant.machine_spec()).map_err(|e| {
                format!(
                    "could not start a machine for {}: {e}",
                    grant.root().display()
                )
            })?;

            // The agent works where the machine put the folder: the guest's
            // `/work`.
            let workspace = machine.workspace_path().to_path_buf();

            // Everything the agent is told, and everything it is allowed, is in
            // the guest's namespace: the engine lives there, and the host path
            // of the folder names nothing inside it.
            let caps = grant.capabilities();
            let system = crate::prompt::assemble(&caps, &workspace, &grant.name(), &config_dir)?;

            // The agent's engine dials in from inside the guest, so the
            // workspace is a guest path — never a directory this host process
            // could `chdir` into; the trunk drives it over the wire the
            // machine hands back.  Taken once here, for both wires at once —
            // `take_wires` panics on a second call, and `net_seat` wants its
            // own half of the same pair.
            let wires = machine.take_wires();
            let root_seat = control_seat(wires.control, workspace)?;
            let net = net_seat(wires.net, egress.clone())?;

            // The machine itself goes to the dialler now: nothing else in
            // this function touches it, and `MachineDial` is what `end`
            // asks for it back once the agent it dials for is gone.
            let dial = Arc::new(crate::machine_dial::MachineDial::new(machine));

            let engine = Engine::new();
            let provider = Arc::new(Provider::build(
                engine.clone(),
                &account,
                model.clone(),
                &credential,
                None,
                tuning,
                None,
            ));
            let agent = Agent::root(
                exarch::agent::RootConfig {
                    system,
                    caps,
                    run_dir,
                    resume: None,
                    no_logs: false,
                    run_lock: None,
                    model,
                    account: RecordedAccount {
                        label: label.clone(),
                        service: account.service.name.as_str().to_string(),
                        id: account.id.as_str().to_string(),
                    },
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
                    // Every agent may delegate: the office assistant hatches
                    // helpers that run concurrently in the same guest, and
                    // the exchange ends only once the whole tree does
                    // (`converse_settled`'s Law B), not merely the trunk.
                    fuel: exarch::agent::SPAWN_FUEL,
                    egress,
                    dial: Some(dial.clone()),
                },
                root_seat,
                Arc::clone(&provider),
            )
            .map_err(|e| format!("could not start the assistant: {e}"))?;

            Ok(Booted {
                dial,
                agent,
                engine,
                net,
            })
        })();

        match booted {
            Ok(Booted {
                dial,
                agent,
                engine,
                net,
            }) => Ok((
                Self {
                    grant,
                    dial,
                    agent,
                    engine,
                    baseline: Baseline::Pending(handle),
                    net,
                },
                Opening {
                    label,
                    model: announced_model,
                    effort: announced_effort,
                    large_folder_line,
                },
            )),
            Err(e) => {
                abandon_baseline(handle);
                Err(e)
            }
        }
    }

    /// Drive one message through the conversation, streaming the bus's
    /// events into the caller's `sink` in order — the same events
    /// [`exarch::headless::converse_settled`] drives through, ending only
    /// once the whole fleet quiesces: the trunk parked, no live helpers
    /// left, their results drained.
    ///
    /// Settles the baseline first — joining its capture thread if this is
    /// the first exchange — before the guest touches anything, since the
    /// folder must never be read and written at once. Checkpoints what this
    /// exchange left behind, cumulatively from the baseline — taken even
    /// after a failed exchange, since whatever changed before the failure is
    /// still undoable. Taking it only after quiescence is what keeps a
    /// helper's late write from ever landing after the checkpoint and being
    /// blamed on the user. Renders no report: the window reads one back
    /// through [`workspace::job_report`].
    ///
    /// # Errors
    /// Returns `Err` if the baseline capture failed or its thread panicked;
    /// otherwise if the exchange itself fails; otherwise if the exchange
    /// succeeded but the after-checkpoint could not be taken, that error is
    /// returned instead.
    pub fn exchange<S: exarch::bus::Sink>(
        &mut self,
        message: String,
        sink: &mut S,
    ) -> Result<(), String> {
        let history = self.baseline.store()?;
        let outcome =
            exarch::headless::converse_settled(&mut self.agent, message, self.engine.clone(), sink);
        let after = history.capture(self.grant.root(), workspace::Moment::After);
        outcome.and_then(|()| after.map(drop))
    }

    /// Shut the machine down, ending the conversation — and, its store
    /// having no life left to serve, wipe it: closing the window is
    /// accepting the folder as it stands, so undo ends here too.
    ///
    /// Drops the agent first: under a real VM its seat owns the wire, and
    /// closing that end is what makes the guest's engine see EOF and power
    /// the machine off from the inside — the same inside-out shutdown
    /// `boot-run`'s own drop-then-stop performs, so the grace window
    /// `machine.shutdown` waits on below normally observes a stop already
    /// under way rather than forcing one. The machine comes back from the
    /// dialler next, which has held it since `begin` — its only other owner
    /// was the agent just dropped, whose own helpers (if any) are gone too
    /// by the time an exchange has settled, so this is always the dialler's
    /// last reference. The net wire follows, never before the control wire
    /// — a session with its control plane gone but its network still live
    /// has nothing left to police what that network is used for. The wipe
    /// comes last, and runs whatever state the baseline settled into — a
    /// `Failed` baseline may still have left partial objects behind, and
    /// those are exactly what a wipe is for. A wipe cut short by the
    /// window's own close timeout is not a leak: the directory it left
    /// unlocked is exactly what [`workspace::history::sweep_stale`] collects
    /// at the next start.
    ///
    /// # Errors
    /// Returns `Err` if the machine does not stop cleanly, or if a
    /// guest-net worker panicked — failures this never swallows. A baseline
    /// still `Pending` (the user closed before ever sending a message) is
    /// joined too; if it alone failed, that error surfaces, but a failed
    /// shutdown always wins over it. A wipe failure surfaces only once
    /// everything above it has succeeded.
    ///
    /// # Panics
    /// Panics if the dialler's `Arc` has another owner left once the agent
    /// is gone — a construction bug, since nothing else in this crate
    /// clones it.
    pub fn end(self) -> Result<(), String> {
        let Self {
            dial,
            agent,
            net,
            mut baseline,
            ..
        } = self;
        drop(agent);
        let dial = Arc::try_unwrap(dial).unwrap_or_else(|_| {
            panic!(
                "the guest dialler outlived the agent that was its only other owner — a helper \
                 must have leaked past the exchange that was supposed to settle it"
            )
        });
        let machine = dial.into_machine();
        // Joined, not merely stopped: ending a conversation must leave no
        // guest-net thread behind, and `join` is what reports a worker panic.
        net.stop();
        let net_end = net.join();
        let shutdown = machine
            .shutdown()
            .map_err(|e| format!("the machine did not stop cleanly: {e}"))
            .and(net_end);
        // Only after the machine is down: a conversation can end before its
        // first exchange, and joining a still-running walk must not hold a
        // dead-to-the-user window open ahead of the shutdown that ends it.
        let baseline_err = baseline.store().err();
        let wipe_err = baseline.into_store().and_then(|store| store.wipe().err());
        match (shutdown, baseline_err) {
            (Err(e), _) | (Ok(()), Some(e)) => Err(e),
            (Ok(()), None) => wipe_err.map_or(Ok(()), Err),
        }
    }
}

/// The whole of what a conversation needs from the credential store — the
/// account it runs on, the label it goes by among its fellows, the model,
/// the effort asked for, and the credential it authenticates with — read
/// under one brief lock.
struct Selected {
    account: Account,
    label: String,
    model: String,
    effort: Option<String>,
    credential: Credential,
}

/// Everything slow in [`Conversation::begin`] (the machine's boot, the
/// folder's safety copy) happens after this returns, so a sign-in in the
/// window is never held up behind a conversation opening, nor the other way
/// round.  The credential is cloned rather than borrowed for the same
/// reason, and clones as what it already is — a `ChatGPT` login's shared
/// cell, so a token refreshed later is still the one this conversation
/// sends.
///
/// `choice.account` resolves by id alone, through [`resolve_account`] —
/// never by the label a human reads, which two accounts can share; naming
/// one by its display label is the CLI's business (`--provider`), not the
/// window's, whose menu only ever hands back what it was given.
///
/// # Errors
/// Returns `Err` if this computer has no account set up, if `choice` names
/// one that has since gone, or if the sole account names no default model.
fn select_account(
    store: &Mutex<CredentialStore>,
    choice: Option<Choice>,
) -> Result<Selected, String> {
    let store = lock(store);
    let available = store.available();
    if available.is_empty() {
        return Err(
            "no assistant account is set up on this computer — sign in with ChatGPT on the \
             opening screen, or ask your IT department to set a provider API key \
             (ANTHROPIC_API_KEY, OPENAI_API_KEY, …)"
                .into(),
        );
    }
    let (account, model, effort) = if let Some(Choice {
        account,
        model,
        effort,
    }) = choice
    {
        let account = resolve_account(&account, &available)
            .ok_or("the chosen account is no longer available on this computer")?;
        (account, model, effort)
    } else {
        let (account, model) = choose(&available)?;
        (account, model, None)
    };
    let label = identity::label(&account, &available);
    let credential = store
        .get(&account.id)
        .expect("the chosen account is one of the available ones")
        .clone();
    // Everything the caller does next is slow, and none of it is the
    // store's business.
    drop(store);
    Ok(Selected {
        account,
        label,
        model,
        effort,
        credential,
    })
}

/// The seat the trunk drives the guest's engine from: `control`, the
/// machine's own control plane, adopted as a wire, working at `cwd`.
///
/// Split out of [`Conversation::begin`] because it is the one step that used
/// to be platform-conditional, and a `#[cfg]` around a `return` inside that
/// long body left every line after it dead. It is no longer conditional at
/// all, and the shape of *why* is worth keeping in view: what
/// [`vm_manager::Machine::take_wires`] hands over differs by platform — an
/// `AF_VSOCK` descriptor under Virtualization.framework, an `AF_HYPERV`
/// socket under Hyper-V — and yet no `#[cfg]` appears below, because
/// [`ral_core::transport::WireTransport::adopt`] takes whatever converts into
/// its own [`WireStream`](ral_core::wire::WireStream) and each platform's
/// owned handle does. The frame protocol never learns which hypervisor it is
/// talking through.
///
/// Takes the wire directly rather than the whole [`vm_manager::Wires`] pair
/// — [`Conversation::begin`] calls [`vm_manager::Machine::take_wires`]
/// itself now, once, so its other half can go to [`net_seat`] instead of
/// being dropped unread.
///
/// # Errors
/// Returns `Err` if the control plane cannot be adopted as a wire.
fn control_seat(
    control: impl Into<ral_core::wire::WireStream>,
    cwd: std::path::PathBuf,
) -> Result<exarch::agent::RootSeat, String> {
    Ok(exarch::agent::RootSeat::Wire {
        transport: Box::new(
            ral_core::transport::WireTransport::adopt(
                control,
                ral_core::transport::Liveness::default(),
            )
            .map_err(|e| format!("could not take control of the machine: {e}"))?,
        ),
        cwd,
        // Home is the guest scratch, not the workspace: `$HOME` is where
        // XDG-defaulting tools drop caches and dotfiles, and pointed at
        // `/work` that litter would land among the user's own documents —
        // and in every change report.
        home: std::path::PathBuf::from(crate::grant::GUEST_SCRATCH),
    })
}

/// Hand the net wire to [`guest_net::run`], which owns it from here on.
///
/// # Errors
/// Returns `Err` if `guest_net::run` cannot start guest networking.
fn net_seat(
    net: impl Into<NetWire>,
    egress: exarch::egress::Egress,
) -> Result<guest_net::Session<NetWire>, String> {
    guest_net::run(
        net.into(),
        guest_net::Config {
            egress,
            gateway: vm_manager::GUEST_LINK.gateway,
            dialer: Arc::new(guest_net::vet::System),
        },
    )
    .map_err(|e| format!("could not start guest networking: {e}"))
}

/// The account and model for a run whose [`Choice`] left both unnamed:
/// whichever one account is set up on this computer, and its default
/// model. An account that names no default model is a question for the
/// user, refused in the same plain register as having no account at all —
/// there is no menu entry left to answer it with.
fn choose(available: &[Account]) -> Result<(Account, String), String> {
    let account = &available[0];
    account
        .service
        .default_model
        .clone()
        .map(|model| (account.clone(), model))
        .ok_or_else(|| {
            format!(
                "the account set up on this computer ('{}') does not say which model to \
                 use — ask your IT department to set one up.",
                identity::label(account, available)
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
    use exarch::provider::ReasoningEffort;
    use exarch::provider::identity::AccountId;
    use exarch::provider::models::ProviderEndpoint;
    use std::collections::BTreeMap;

    /// A built-in service's sole account — the common case in these tests.
    fn fam(name: &str) -> Account {
        let service = identity::built_in_services()
            .into_iter()
            .find(|service| service.name.as_str() == name)
            .unwrap_or_else(|| panic!("no built-in service named {name}"));
        Account::of_service(service)
    }

    /// A signed-in `ChatGPT` account, whose service names no default model
    /// and so stands in for every service [`menu_from`] cannot fall back on.
    fn chatgpt(handle: &str) -> Account {
        let service = identity::chatgpt_service();
        Account {
            id: AccountId::of_login(&service.name, handle),
            service,
            handle: handle.to_string(),
        }
    }

    type Lists = BTreeMap<AccountId, Result<Vec<String>, String>>;

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
        fn list(&self, id: &AccountId) -> Result<Vec<String>, String> {
            lock(&self.lists)
                .get(id)
                .cloned()
                .unwrap_or_else(|| Err("no fake list".into()))
        }

        fn endpoints(&self, _model: &str) -> Result<Vec<ProviderEndpoint>, String> {
            Err("not exercised by these tests".into())
        }
    }

    fn one(id: AccountId, models: &[&str]) -> Lists {
        let mut m = BTreeMap::new();
        m.insert(id, Ok(models.iter().map(ToString::to_string).collect()));
        m
    }

    fn model_names(choice: &ProviderChoice) -> Vec<String> {
        choice.models.iter().map(|m| m.name.clone()).collect()
    }

    #[test]
    fn menu_with_nothing_cached_offers_the_service_default_alone() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let available = [fam("anthropic")];

        let menu = menu_from(&available, &mut catalog);

        assert_eq!(menu.providers.len(), 1);
        assert_eq!(
            model_names(&menu.providers[0]),
            vec![fam("anthropic").service.default_model.unwrap()]
        );
        assert_eq!(menu.efforts.first().map(String::as_str), Some("auto"));
        assert_eq!(menu.default_effort, "med");
    }

    #[test]
    fn menu_with_a_cached_list_puts_the_default_first_and_dedupes_it() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let anthropic = fam("anthropic");
        let default = anthropic.service.default_model.clone().unwrap();
        catalog.record(
            &anthropic.id,
            vec!["claude-haiku-4".to_string(), default.clone()],
        );

        let menu = menu_from(std::slice::from_ref(&anthropic), &mut catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            vec![default, "claude-haiku-4".to_string()]
        );
    }

    #[test]
    fn a_chatgpt_style_account_with_no_service_default_starts_empty() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let account = chatgpt("work-account");

        let menu = menu_from(std::slice::from_ref(&account), &mut catalog);

        assert!(menu.providers[0].default_model.is_none());
        assert!(model_names(&menu.providers[0]).is_empty());
    }

    #[test]
    fn refresh_menu_folds_fetched_lists_in_and_serves_them() {
        let account = chatgpt("work-account");
        let source = FakeSource::new(one(account.id.clone(), &["gpt-5.5-codex"]));
        let catalog = Mutex::new(ModelCatalog::memo_only(source));

        let menu = refresh_menu_for(std::slice::from_ref(&account), &catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            vec!["gpt-5.5-codex".to_string()]
        );
        assert_eq!(
            lock(&catalog).cached(&account.id),
            Some(vec!["gpt-5.5-codex".to_string()])
        );
    }

    #[test]
    fn refresh_menu_leaves_a_failed_fetch_uncached_but_still_shows_the_default() {
        let deepseek = fam("deepseek");
        let mut lists = Lists::new();
        lists.insert(deepseek.id.clone(), Err("network down".to_string()));
        let catalog = Mutex::new(ModelCatalog::memo_only(FakeSource::new(lists)));

        let menu = refresh_menu_for(std::slice::from_ref(&deepseek), &catalog);

        assert_eq!(
            model_names(&menu.providers[0]),
            vec![deepseek.service.default_model.clone().unwrap()]
        );
        assert_eq!(lock(&catalog).cached(&deepseek.id), None);
    }

    /// The window dereferences `menu.default_effort` (`synod/ui/index.html`
    /// line 1436) and `p.largeFolderLine` (line 2119) — two structs in this
    /// file under opposite serde conventions.  Tidying either one into the
    /// other's convention compiles, passes every other test, and leaves the
    /// window reading `undefined`: an empty effort and a warning that never
    /// appears.
    #[test]
    fn the_window_reads_these_exact_json_keys() {
        let mut catalog = ModelCatalog::memo_only(FakeSource::new(Lists::new()));
        let menu = serde_json::to_value(menu_from(&[fam("anthropic")], &mut catalog))
            .expect("the menu serialises");
        assert!(menu["default_effort"].is_string());
        assert!(menu["efforts"].is_array());
        assert!(menu["providers"][0]["account"].is_string());
        assert!(menu["providers"][0]["default_model"].is_string());
        assert!(menu["providers"][0]["models"][0]["reasoning"].is_boolean());

        let opening = serde_json::to_value(Opening {
            label: "work".to_string(),
            model: fam("anthropic").service.default_model.unwrap(),
            effort: "med".to_string(),
            large_folder_line: Some("This folder is very large.".to_string()),
        })
        .expect("the opening serialises");
        assert_eq!(opening["label"], "work");
        assert_eq!(opening["largeFolderLine"], "This folder is very large.");
        assert!(opening.get("large_folder_line").is_none());
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
