---
generated_at_commit: 1f2056b4d00720b668339430fa3ccc87ee1cd528
generated_at_date: 2026-10-28
covers_paths: [guest-abi/, ral-guest/, netgate/, vm-manager/, synod-machine-broker/, synod/, vm-image/]
---

# Map: synod

**Synod grants one folder to an office assistant inside a hardware VM and
accounts for its changes after each job.** It embeds
[[map/exarch|exarch]] as a library, supplying its own persona, grant and
window. The folder is worked in place; the account reads metadata, not file
contents, and offers no restoration operation.

## The crates

One runtime agreement connects the host, guest, engine and image tools
([[decisions/261027_the-vm-crates-are-layered|the-vm-crates-are-layered]]).

- `guest-abi` — dependency-free, std-only runtime constants, boot grammar
  and packet framing.
- `ral-core` — the engine, wire transport, path judgments and pure egress
  policy data; it depends on `guest-abi`.
- `exarch` — provider machinery, agents, the conversation driver, policy
  loading and guest-engine construction; it depends on core.
- `netgate` — host CONNECT gate, TCP/IP stack and audit ledger; it depends
  on core and `guest-abi`.
- `vm-manager` — machine construction, lifecycle, media and broker client;
  its runtime workspace dependency is `guest-abi`.
- `synod-machine-broker` — binary-only Windows service over `vm-manager`.
- `ral-guest` — binary-only Linux PID 1 over `guest-abi`.
- `ral-vm-image-build-tools` (`vm-image/build-tools/`) — manifest production
  and checking over core and `guest-abi`.
- `synod` — binary-only composition of exarch, core, `guest-abi`, netgate
  and vm-manager. Its internal modules are crate-private.

## guest-abi/ and ral-guest/ — the guest

One PID 1 assembles and supervises the guest without an exec between stages.

- `guest-abi/src/{boot,ports,link,layout,packet}.rs` owns the runtime
  agreement. `boot::Net` validates routing once and preserves it through
  private fields. Changes to boot, the factory port, layout or framing bump
  `boot::CONTRACT` beside its writer and reader.
- `boot::Export` states the host's filesystem choice, so the guest need not infer
  its hypervisor from devices. Virtiofs tags and 9p `aname`s share the stricter
  36-byte virtiofs limit; validation precedes export selection because key order
  is arbitrary. The engine path defaults because it belongs to the image.
- `ral.port`, `ral.plan9` and `ral.net` name separate host-side control,
  workspace and network ports: confusing them connects different protocols.
  Without a network, the engine receives no proxy environment for a missing peer.
- `packet` is shared by host and guest without a Linux gate to prevent codec
  drift. Frames have no prologue and reject invalid lengths before allocation.
  Raw `read_exact` prevents read-ahead bytes being lost on descriptor handoff;
  framing or I/O failure ends the link because a frame boundary cannot be guessed.
- `ral-guest/src/main.rs` refuses a non-init process before any side effect,
  ignores argv, then calls `assemble::assemble` and `init::serve`.
- `assemble/{plan,run}.rs` loads modules to a fixpoint, adopts the console,
  probes rootfs/session disk pairs, formats the session disk, assembles the
  overlay, installs the engine, reclaims the initramfs and switches root.
  Mount primitives are shared through `mount.rs`.
- Module metadata supplies a load-order hint; missing symbols can resolve after
  another pass, so loading repeats while it makes progress. An absent manifest
  permits kernels with built-in drivers; `EEXIST` already satisfies a requirement.
- `mount.rs` uses the LCOW agent's 64 KiB 9p `msize` for both socket buffer
  requests too: smaller buffers split messages into additional round trips.
  Linux doubles and clamps these advisory requests.
- The host supplies the boot clock because the guest has no reliable RTC.
  It is set once; long sessions can drift, and further correction belongs on
  the protocol rather than under a second clock authority in PID 1.
- `init/serve.rs` orders clock, mounts, cgroups, sysctls, control dial,
  networking, factory and root engine. `reap::mask` blocks signals before
  threads; children unmask before exec. `engine.rs` constructs one closed
  command/environment and hands each engine its stream unread on fd 3.
- `init/fleet.rs` owns admission and the host-CID-only engine factory;
  binding precedes the root spawn. `MachineFactory` dials this listener to
  create one engine per helper. `init/halt.rs` owns reaping, deepest-first
  engine cgroup cleanup and TERM/grace/KILL shutdown with `cgroup.kill`.
- `pump::start` owns tun/net descriptors in two threads, one writer per
  direction, with raw `read_exact`. Any I/O end or caught panic records the
  first fatal notification through `reap::fail` and signals PID 1 with TERM.
  Root exit ends the session; the root is never restarted.

## netgate/ — the host network boundary

The guest's tun has one peer: **a host TCP/IP stack that admits only vetted
CONNECT destinations**, with no network adapter on the VM
([[design/egress|egress]]).

- `ral_core::egress` owns `Host`, `NetPolicy`, `admits` and refusal text;
  exarch loads policy and passes only its search bit to the agent.
- `netgate/src/lib.rs` exports `run`, `Config`, `Session`, `Dialer`,
  `System`, `AuditLog` and `LEDGER`. `stack.rs` owns `Config`, `pipe.rs`
  owns `Wake`, and private `refusal.rs` serves decoding and vetting.
- `connect.rs` decodes bounded CONNECT headers; `vet.rs` resolves once,
  rejects non-public addresses and dials only vetted destinations. `ledger.rs`
  owns strict-poison audit writes and rotation: admission is recorded before
  sending 200, refusal before returning it, and close includes byte counts.
- `device.rs` uses core's `WireStream`; `pipe.rs` owns bounded rings and
  backpressure. Directional EOF is independent of whole-worker retirement:
  upload EOF shuts the origin's write half; download EOF closes only output.
  A full frame inbox drops packets so guest TCP backs off; transmit blocks
  on the wire. Full byte rings leave bytes in smoltcp's receive buffer.
  Rings ignore poison because joining reports worker panics and surviving
  peers must remain able to wake; ledger poison stays strict.
- `stack.rs` tracks live workers without joining them on the poll thread.
  Drained output can send FIN; whole-worker completion starts `FIN_GRACE`.
  `Session::{stop, join}` wakes and joins the workers. There is no active
  tunnel timeout; `TUNNEL_CAP` bounds the population.

The stack speaks bare IP (`medium-ip`) because its peer is a tun, with no
UDP sockets. Vetting and dials raise no separate card: their accounting is
the ledger, and the invoking exec already surfaces the operation. The fuzz
workspace isolates nightly/libfuzzer dependencies; ordinary parser tests
replay its corpus. Loopback tests retain real address vetting but dial local
fixtures, and run serially because thread-count assertions are process-wide.
Timeout margins allow scheduling jitter; finite join waits make a hung
session fail its case instead of hanging the suite.

## vm-manager/ — machines and media

The crate root is the public facade; backend children import their owning
modules directly. **Only real machines are constructed.**

- `spec.rs` owns `MachineSpec { folder, read_only }`, media resolution,
  `SIZE` (4 vCPUs, 6144 MiB) and `boot_line`, the caller of the sole ABI
  command-line writer. `machine.rs` owns `Booted`, `Wires`, `Socket` and
  the `Hypervisor`/`Machine` traits; every machine method is required.
- `direct(artifact)` and `detect(media)` check availability before
  construction and resolve artifacts before another process opens them.
  `detect` prefers the Windows broker without invoking lazy `BootMedia`.
  Missing hardware support, entitlement, permission or media gives a refusal.
  MSI cabinets cannot hold a two-gigabyte rootfs file, so installed media is
  compressed; laziness avoids inflating a second per-user copy beside the service's.
- `media::locate` tries bundle `../Resources/boot`, installed `boot` beside
  the executable, then a `target` ancestor's checkout output. Packed rootfs
  plus checksum wins in an installation; checkout rootfs is under
  `vm-image/out/rootfs/`.
- `Layout::ready` verifies and publishes `rootfs-<sha256>.img`;
  `Layout::found` preserves packed media for Windows' direct VHD preparation.
  `publish` locks `<leaf>.lock`, prepares a unique temporary file, syncs it
  and publishes without replacement. `inflate` bounds the zstd window.
- `lifecycle.rs::await_dials` receives control and net against one 60-second
  deadline. `stop_within` polls for five seconds before forcing.
  `SessionDisk` holds a std exclusive lease in a unique
  `ral-vm-session-<epoch>-<pid>-<suffix>` directory. Removal retries for two
  seconds; `sweep` reclaims aged, unleased directories without following
  links or Windows reparse points, on both backends.
- `error.rs` owns data-only `Error`; `worded(spell)` renders at the caller's
  boundary. `GuestConsole::{Kept, Unkept}` distinguishes a retained empty
  console from no retained copy. `GuestConsole::report` supplies engine-loss
  diagnostics. `os_path.rs` uses serde's `OsString` encoding on the broker
  pipe, preserving non-Unicode paths.
- `vz/{mod,config,queue}.rs` owns the macOS arm64 machine, configuration
  and serial-queue operations. It requires the virtualization entitlement,
  shares the folder through virtiofs, attaches RO rootfs and RW sparse
  session disks, and multiplexes both accepted wires on one vsock device.
  Its console goes to stdout without a retained copy.
  The generated `objc2` binding avoids a Swift shim and its second toolchain;
  configuration construction needs no entitlement, but validation already does.
- `hcs/{mod,api,spec,hvsock,vhd,console,win}.rs` owns the Windows x86_64
  machine. HCS is dynamically loaded; its JSON document specifies direct
  kernel boot, a 9p folder share, disks and sockets, with no network adapter.
  A dynamic session VHD declares 8 GiB; immutable fixed rootfs VHDs are
  prepared through `media::publish`. The console tees stdout, a bounded
  log and a line ring. Access granted to boot files is revoked at teardown.
  `AllowOvercommit` backs guest memory lazily, so an idle 6 GiB session need
  not consume 6 GiB of host RAM.
- `Machine::dial_engine` asks for `guest_abi::ports::FACTORY`.
  macOS implements the guest dial; the two Windows machine implementations
  explicitly answer `Unsupported` pending the factory work.

## synod-machine-broker/ — the privileged service

**The connection is the machine's lease**, and the caller supplies only a
folder and read-only flag
([[decisions/260725_windows-machine-broker|windows-machine-broker]]).

- `vm-manager/src/broker/wire.rs` owns `PIPE`, `VERSION`, `Request`, `Reply`
  and length-prefixed JSON with a 1 MiB cap checked before allocation.
  Every change to a pipe-carried type increments the version.
- `vm-manager/src/broker/client.rs::Brokered` probes and asks the service;
  `adopt_socket` adopts the duplicated socket for this client process.
  `BrokeredGuest` closes the pipe on shutdown or drop.
- `synod-machine-broker/src/service.rs` is the privileged boundary: pipe
  SDDL admits SYSTEM, Administrators and interactive users; a first-instance
  pipe prevents replacement. Version refusal precedes `boot_for`.
- `open_for_client` impersonates the caller with a revert guard;
  `Granted` pins the folder, checks reparse points and verifies it remains
  unchanged. The kernel supplies the client PID for socket duplication.
  `Boot → Booted → Adopted` preserves socket and folder ownership order.
- `rights_for` checks list/traverse/attribute rights, adding file and folder
  creation for writable grants. It omits `FILE_DELETE_CHILD`: ordinary Modify
  grants delete through rights on the file and need not hold that directory right.
- `Granted::open` uses backup semantics to open directories; it grants no extra
  access without enabled backup privilege. Identification-only clients cannot
  open as the caller and are refused. The final name surrogate is refused because
  its access check covers the link, not the target; cloud placeholders remain valid.
- Holding the grant without `FILE_SHARE_DELETE` pins its path, including ancestors,
  while HCS opens it by name. An empty pinned folder can still become a junction,
  so `unchanged` rechecks after boot. Wires drop before the machine, then the grant.
- The service discovers and resolves its own media through
  `media::locate(&exe)?.found().resolve()` and uses
  `%ProgramData%\Synod\Machine`; direct HCS uses the user's local cache.
- `scm.rs` owns service startup and `--console`, reporting RUNNING before
  serving. `synod/wix/broker-service.wxs` installs the LocalSystem service
  beside the application and boot media. Checkout helpers are
  `just broker-install` and `just broker-uninstall`.
  Cargo declares the binary on every platform; off Windows it refuses to run.

## synod/src/ — five layers

The order is **folder < wire < assistant < seam < window**; `site.rs` is a
leaf shared by the layers' typed I/O sites. `main.rs` declares the modules
and composes the desktop application.

| Layer | Files | Responsibility |
|---|---|---|
| L0 folder | `folder/{grant,manifest,changes}.rs` | Authority and the account of the folder; only core and guest ABI workspace dependencies |
| L1 wire | `wire.rs` | Every `TS` type, pure DTO constructors, `WindowReport::of`, `OPENABLE`, `open_as` |
| L2 assistant | `assistant/{accounts,menu,prompt,conversation,baseline,guest,signin}.rs` | Product assembly and conversation lifecycle |
| L3 seam | `seam/{narration,ipc}.rs` | Event folding and event-name constants |
| L4 window | `window/{state,conversation,files,accounts,highlight,live,seal}.rs` | Tauri state, commands, publication and the live-frame scheme |

- Tauri, its plugins, `url`, L4 and the desktop `main` compile only for
  macOS or Windows. L0–L3 compile on Linux; its `main` prints a refusal.
  `just lint-cross` is the dead-code authority for the lower layers.
- `build.rs` calls `tauri_build::build` only for a desktop target;
  `tauri-build` is an unconditional build dependency. Generated permission
  schemas in `synod/gen/` are ignored build products.
- `main.rs` runs `exit_if_re_exec_child` first, prepares credentials before
  starting threads, installs plugins and state, registers the `synod-live`
  scheme (`live::serve`) and registers the window commands. `open_main_window` builds
  the main window in `setup` (`"create": false` in `tauri.conf.json`); on macOS
  it hands the builder the sealed `WKWebViewConfiguration` from `seal.rs`, and
  live pages stay unserved until the `Sealed` state exists.
  Its navigation guard admits only the app origin, the live frame or a
  configured development origin; `is_live_frame` accepts WebKit's spelling
  because WebKit asks the guard about subframe loads. CSP and capabilities are
  in `tauri.conf.json` and `capabilities/`.
  The guard is a plugin so that it covers every webview, not only the main
  window; foreign navigation would expose the injected command bridge to model
  output.

### Folder and report

The account distinguishes **unseen changes from known absence**.

- `folder/grant.rs::Grant::open` resolves the root and refuses sensitive
  territory, with a temporary-directory carve-out. Its capabilities name
  guest `/work` and `/tmp`, the office toolbox, no editor, no GPU and no
  direct engine network authority. The `Grant::machine_spec` implementation
  lives in `assistant/conversation.rs`, keeping machine types out of L0.
  Settings can execute at sign-in; temp descendants hold documents without
  that authority. Bare toolbox names keep image additions from widening the
  grant; Python and `find -exec` mean this list is a job policy, not containment.
  Ral supplies shell composition; pip's user installs work under fresh guest
  UIDs where apt cannot. Empty deny paths preserve the folder the user granted.
- `folder/manifest.rs::Manifest::of_folder_via` walks through typed sites,
  recording paths, kinds, sizes, nanosecond mtimes and modes. It records
  empty folders and link targets without following links. `Stop` and a
  progress callback make the baseline observable and interruptible.
- A listed entry that vanishes becomes `Manifest::unread`; `covers` is the
  shared folder-prefix judgment. A missing or unentered root and a stopped
  walk are errors, never empty or partial accounts.
- `folder/changes.rs` compares manifests without I/O. Kind, size or mode
  changes are `Modified`; a timestamp alone is `Touched`. Matching
  `(size, mtime_ns)` pairs deletions with creations as `Renamed`, except
  when both sides have multiple candidates. Unread prefixes suppress claims
  beneath them.
- `changes::job_report` produces `JobReport { changes, unreadable }`.
  The folder layer has no serde representation. `wire::WindowReport::of`
  creates display cards; `Change::leaves_file` determines where Open/Reveal
  is possible, and `open_as` classifies a file by the extension allowlist.
- `ral_core::path::spelled` supplies the shared path-resolution judgment
  used by the folder layer.

### Assistant and guest seam

The conversation owns the machine's work and the baseline against which it
is judged; shared machinery stays with exarch and vm-manager.

- `assistant/accounts.rs` builds `Wallet::of(SYNOD, DECLARATIONS_FILE, LABEL)`
  and calls `Wallet::prepare`. Environment credentials are resolved and
  scrubbed once, before threads, above saved keys in the computer's vault.
  Endpoints live in synod's `providers.ral`; keys never do.
- `assistant/menu.rs` supplies the cached and refreshed model listings.
  `wire::Choice` names an account by id, model and optional effort.
  `exarch::provider::Tuning::for_model` applies the shared `Tuning::masked`
  law; absent effort and explicit `auto` retain their distinct meanings.
  Account selection and listing keep locks off boot, walks and network waits,
  so opening a conversation or fetching models cannot hold up sign-in.
- `assistant/signin.rs` drives the ChatGPT browser login, converts
  `LoginPhase` into `wire::SignInStep` and admits the credential through
  exarch's shared OAuth machinery. Shared `Holdings` make an admitted account
  available without a restart. Synod has no direct tokio dependency.
- `assistant/prompt.rs` and `data/*.md` supply the office persona, examples,
  toolbox and live-page rules (`data/live.md`).
  `exarch::prompt::{RAL_REFERENCE, EDIT_REPLACE}` supply shared text. The host
  section states only guest truths; no host folder path enters the prompt, and
  HOME is the guest scratch.
  String replacement avoids line witnesses for mostly opaque office formats;
  optional house rules come from trusted config, and surface guidance then
  live-page guidance come last for recency. The host supplies only time,
  matching the guest's clock.
- `assistant/conversation.rs::Conversation::begin` opens the grant, selects
  a machine and account, opens the network ledger and run log under
  `RunLock`, starts `Baseline`, boots, then starts `Guest`. Provider requests
  share a `provider::Pool` for the conversation, separate from the guest's
  ral engines. The ledger opens
  over `SYNOD.own(XdgKind::State)` as `netgate::LEDGER` and passes beside
  policy in `netgate::Config`. Networking must
  start successfully; the root cannot schedule itself or resume on reset,
  and may delegate with exarch's `SPAWN_FUEL`.
- `assistant/baseline.rs::Baseline` owns its walk thread, stop switch and
  settled outcome. The walk starts alongside boot and is joined before the
  first guest act. `Drop` stops and joins only an abandoned walk; it cannot
  turn a settled baseline into a stopped one.
- `assistant/guest.rs::Guest::start` consumes `Booted { machine, wires }`.
  `RootEngine::guest` adopts control at `guest_abi::layout::{WORK, SCRATCH}`;
  `netgate::run` adopts net. `MachineFactory(Mutex<Box<dyn Machine>>)`
  implements exarch's `Factory` through `Machine::dial_engine`; the mutex
  supplies `Sync`. `exarch::app::SCRATCH_VAR` names the guest scratch variable.
- `Guest` holds `factory`, `settled` and `net`. Its `SettledConversation`
  owns the root `Agent` and a bus spanning jobs; `handle`, `await_job` and
  `run(job, sink)` forward to that driver. `severance` reads the driver's
  `severance()` / `run_dir()`, never borrows the root.
- A *job* opens on human text through `await_job`, then runs through fleet
  quiescence. Replied resident helpers do not hold it open. A stopped job
  takes up nothing new. Every job waits for ended helpers' acknowledged
  teardown inside the output pump before returning `JobEnd`
  ([[decisions/260806_exchange-ends-at-fleet-quiescence|job settlement]]).
  The attending owner's membership is counted exactly once even if it ends.
- `Conversation::settle_baseline` joins the opening walk before the message
  loop and Ready. `run(job, sink) -> JobEnd` retains the cached baseline
  guard: failure ends the conversation and closes the unattended job.
  After guest attendance it takes the closing walk unless the driver failed
  or remote teardown could not be certified. Reports are cumulative
  against the opening baseline; a failed walk clears the report, and a later
  success restores it.
- `Conversation::{handle, await_job}` expose the `AgentHandle` and human-only
  job wait. `lost()` records terminal failure; the loop ends without another job.
  Engine diagnostics are captured into `engine.log` while the machine is up.
  Capture repeats the failure so the file stands alone; capture failure is
  secondary.
- `Guest::end` calls `SettledConversation::end_fleet`, intentionally cancelling
  every root in the shared fleet. The driver/root drops before waiting on the
  detached `Census`. The tenancy releases last, after engine/log and
  fleet references, so this wait retains no launch or factory.
  Only then does `Guest::end` recover the uniquely owned `MachineFactory`
  machine, stop and join netgate, and shut the machine down.
  `Conversation::end` drops its baseline after guest teardown. The guest's
  `Detach → Ended` certifies engine and worker teardown; EOF alone does not.

### Narration and window

Publication and ownership agree at one generation boundary.

- `seam/narration.rs` folds durable and transient events by `Role::{Trunk,
  Helper}`. `Router` latches the first signaling agent id as root; helper
  prose is suppressed, helper cards become process cards, usage still counts,
  and plumbing faults remain visible. `SubagentDone` becomes `HelperDone`.
  A trunk `UserPrompt` or `Steering` with `Voice::Human` projects as
  `SynodEvent::Human { text }`; helper prompts project nothing. Transcript
  position is uptake, not submission. A human item drawn just before
  cancellation is appended before the exchange scope is polled; it is recorded
  and projected once without starting a provider round.
- `Router::busy` is a `BTreeSet<AgentId>`: birth or a pending state inserts;
  ready or death removes. `SynodEvent::Helpers { busy }` is emitted only on
  a change. `Narrator<F>` implements `Sink` using a callback, independent
  of Tauri.
- `wire.rs` owns every `#[derive(TS)]`, including account/menu/sign-in types,
  narration, reports, highlighting and `ConversationEvent`. `seam/ipc.rs`
  owns the four event names. Serde tags use `snake_case`; exarch has no ts-rs
  dependency. Mark payloads remain open records of exarch's card vocabulary.
  `Record<string, any>` keeps fields readable without erasing the union tag;
  `Measure` uses `unknown` because `any` in its flattened intersection erases it.
  Raw marks become lossy text once at the seam, avoiding arrays of byte numbers.
- `window/state.rs` owns `Accounts`, `Running`, `Owner`, `Handle`, `Close`,
  `Review` and `SignIn`. `Owner::{Open, Closing}` admits one close owner;
  its five-second grace bounds the wait for teardown. `Handle` holds an
  `Inlet`: `Booting(Vec<String>)` buffers human messages, then `Live(AgentHandle)`
  steers them. Queue observation and recall work in both states; Stop names
  the live root's matching running `JobId`.
  Supersession ends the old inlet and stops its baseline, then joins outside
  the owner lock before opening the next conversation.
- `window/conversation.rs::Emitter` publishes only while its generation owns
  the slot. `install` transfers buffered messages to the root's address
  under the slot lock. Every update carries the window's attempt.
  `registered` checks that attempt for `send_message` and `recall_queued`.
  `stop_job(attempt, job)` is synchronous: under the slot lock it calls
  `AgentHandle::stop`, then publishes the queue. A missing/different attempt,
  booting inlet, stale id or duplicate Stop returns no prompts. Stop and recall
  withdraw human text oldest first. Job identity and Stop/close arbitration
  belong to exarch's root inbox, not a second window state machine.
- **Queue snapshots are read and published under the same slot lock.**
  `ConversationUpdate::Queue { messages }` replaces the strip, oldest first.
  Commands, installation, human uptake and job completion refresh it;
  an older snapshot cannot publish after a newer one.
- `converse` installs the address, settles the baseline, publishes Ready,
  then loops over `await_job` and `run(job, sink)`. `JobBegan { job }`
  announces before attendance; supersession drops the unattended job,
  closing it. A superseded worker ends quietly; baseline failure or
  engine loss ends the current attempt.
- `wire::JobId` converts to/from exarch's `JobId`; `ConversationUpdate`
  carries `JobBegan`, `Queue` and `JobDone` with snake-case tags.
- `JobDone { job, stopped, report: Option<WindowReport> }` carries
  the account and backend Stop verdict with addressed completion. `None` means
  the folder could not be walked, never no changes.
  `Emitter::job_done` replaces `Review` and publishes under the same
  generation lock, taking Running before Review, then refreshes the queue.
  Registration clears Review
  inside its own critical section; a superseded worker cannot replace it.
- `Ended { explained }` terminates the attempt. A panic gives
  `explained: false`; a narrated failure gives `true`. `Held` retains the
  report's folder root after a worker ends so its cards remain openable.
- `window/files.rs::open_file` accepts `(index, shown name)` against the
  held report. Its descendant walk refuses symlinks, reparse points and
  non-plain files. Allowlisted documents open in the default application;
  other files are revealed. Windows applies Mark of the Web. `open_url`
  admits only web and email schemes.
  Provenance marking is best-effort because FAT and some shares lack alternate
  streams. HTML is revealed because browser scripts can reach other host files.
- `window/accounts.rs` owns account writes, endpoint writes, single-flight
  cancellable sign-in and menu refresh. Sign-in releases its slot before
  refreshing the menu and announcing completion. `window/highlight.rs`
  translates core's syntax spans to UTF-16 offsets.
- `window/live.rs` serves the `synod-live` scheme, the origin live pages run
  in ([[decisions/261027_live-pages-run-sealed|live-pages-run-sealed]]).
  `route` admits `/` (the shell), four prelude files and `vendor/`, by plain
  path segments only; `serve` refuses a resolver answer of type `text/html`,
  since a missing asset resolves to the app's own page. `sealed` builds every
  response with the frame's CSP (`sandbox allow-scripts`, no network source, no
  `'unsafe-eval'`), `nosniff`, a wildcard CORS origin and
  `X-DNS-Prefetch-Control: off`. The main CSP's `frame-src` names the one frame
  origin per platform, `tauri.windows.conf.json` overriding it.
- `window/seal.rs` (macOS only) seals the webview when it is created, through
  `WebviewWindowBuilder::with_webview_configuration`: the private
  `WKWebViewConfiguration.allowedNetworkHosts` set empty, the private
  `WKPreferences.peerConnectionEnabled` set false (both by key-value coding),
  and a `WKContentRuleList` blocking `^https?:` and `^wss?:`. Once all three are
  applied and the private two read back, it manages the zero-sized token
  `Sealed` as Tauri state; `live::serve` answers 503 for the frame shell until
  the state exists, so a selector a later macOS renames turns live pages off
  rather than leaving them unsealed. The main window is built in `setup` (`"create": false` in
  `tauri.conf.json`) so the hook exists. Windows is not sealed. Channels,
  measurements and the audit: [[design/live-page-egress|live-page-egress]].

### synod/ui/ — the static frontend

The frontend uses ES modules without a bundler or npm; `index.html` loads
`js/app.js`, which paints after the module graph is ready.

- `js/ipc.js` alone holds the Tauri bridge and spells command/event names,
  with one typed function per command or event. `dom.js` owns DOM lookup,
  screens, buttons and error display; `status.js` owns facts and status;
  `core.js` owns state, confirmation and transcript pinning.
  WebView2 disables native confirm; the dialog plugin is injected separately,
  and its absence permits the restart button rather than silently disabling it.
- Screens feed the projector, which feeds the chat document and renderers.
  `chat-document.js::onChange` registers the projector's render callback,
  removing its import back into the projector.
- `conversation.js` filters attempts before projection. Only backend
  `job_began` and `job_done` move job activity; accepted and
  ready attempts admit composition even while busy. Terminal attempts cannot
  revive. Sending queues text without drawing a transcript prompt.
- `Queue` replaces the dimmed, text-only queue strip; Edit or Up in an empty
  composer recalls it. `Human` closes open prose and work before drawing the
  recorded prompt. Failed sends restore their text ahead of the live draft.
  `withdrawals.js` serializes Stop and recall requests per attempt, accumulating
  overlapping withdrawals in gesture order before one restoration ahead of
  the current draft. Superseded attempts cannot restore or report into a new one.
- Stop and Esc capture the attempt and job ID **at gesture time**, then
  enqueue `stopJob(attempt, job)` through `withdrawals.js`; waiting behind
  recall never retargets Stop. Idempotence is decided at the root inbox, not
  by a frontend pending-Stop register. Only matching backend
  `JobDone { stopped: true, .. }` draws “Stopped.” after in-flight output,
  even if completion precedes command acknowledgement. Recalled text stays
  in the composer; text sent after Stop stays queued and opens the next job.
  Ending an attempt restores the last queue snapshot and clears the strip
  ([[decisions/261028_one-scope-tree-one-inbox|one-scope-tree-one-inbox]]).
- `change-report.js::showReport` consumes the completion's report without
  another command. `null` displays the unavailable-account sentence and no
  cards; an empty report displays “Nothing has changed…”. Reset reuses a
  frozen empty report and creates a fresh shown-row set. Unread paths appear
  as a gap beside the account rather than claims about changes there.
- `SynodEvent::State` carries the typed `AgentStateDto`, never a label; the
  projector derives status text by an exhaustive switch. Window startup is not
  an agent state, so it rides `SynodEvent::Preparing { Preparation }`.
  `chat-document.js::thinking` holds true from `awaiting_model` until the
  step's first token, call or card; while it holds, the projector appends
  `armillary.js`'s element after the last block. It is an `AsciiEffect`
  armillary sphere, 16 by 7 glyphs, drawn as a pure function of the clock and
  looped only while shown; reduced motion draws one frame.
- `projector.js` and the helper dial display busy helpers. `prose.js`
  renders a settled markdown prefix once and rebuilds only its open tail;
  `Boundary` or bubble closure settles that tail.
  Rebuilding whole bubbles is quadratic and reflows settled math; only closed
  code is highlighted to avoid a fresh shell request for every streaming token.
- `live.js`, `live-frame.js` and `live-sizer.js` run a settled `live-html`
  fence as a page. `live.js` replaces the fence with a sandboxed iframe on the
  `synod-live` origin (a placeholder while the fence is open), answers the
  frame's `"ready"` with the page source and sets the frame's height from its
  clamped reports. `live-frame.js` is the inert shell: framed, it takes one
  page by message and writes it after a prelude; at top level it does nothing.
  `live-sizer.js` reports the page's root height. `prose.js` mounts frames
  from settled chunks and holds a placeholder for the open tail.
- `css/tokens.css` is custom properties and `color-scheme` only, and the live
  prelude loads it with `css/live-frame.css`. `css/base.css`, linked after it in
  `index.html`, holds the app page's base rules (box sizing, `html, body`
  height, paper and gradient); the prelude omits it because `height: 100%`
  would pin the root's border box to the viewport and blind the sizer.
- `math.js::renderAssistantMarkdown` lifts formulae before marked parses prose,
  inserts KaTeX nodes with trust disabled, then applies DOMPurify to the whole
  inert tree. The same module supplies the delimiter scan. Sanitized trees
  are appended as fragments, never serialized and reparsed.
  Lifting prevents markdown emphasis and line breaks from corrupting formulae;
  stripped private-use sentinels cannot be forged. Reparsing scrubbed markup
  risks mutation XSS. KaTeX accessibility tags survive; navigation and style
  tags do not. Code restores raw delimiters, and malformed TeX renders in place.
- `ral-highlight.js` asks `highlight_ral` for core's classifications, builds
  text nodes and caches by exact source text. Marked, DOMPurify, KaTeX,
  three.js (for the armillary), and Chart.js, D3 and Mermaid (for live pages)
  are vendored verbatim under `vendor/`, with
  versions and hashes there; hand-written `.d.ts` files type the three.js slice
  in use, since `deno check` cannot read the minified build.
- `just ui-check` exports Rust bindings into ignored `ui/js/bindings/` and
  runs `deno check` on every platform where the recipe runs, including Linux.
  Exhaustive `never` switches catch unhandled Rust variants.
  Test workshops use canonical target paths: symlinked temp paths confuse grant
  comparisons, and host `/tmp` overlaps the guest scratch allowance.

## vm-image/ — build and packaging

Boot compatibility is checked by the image tools before bundling.

- `build.sh` constructs the office rootfs in a native arm64 or amd64
  container: Ubuntu userland, document tools, ext4 and bounded-window zstd,
  with a checksum and package manifest. `vm-image/README.md` records the
  pipeline and remaining image questions.
- `build-boot.sh` builds static musl `ral-guest` and exarch, packages PID 1
  as `/init`, and writes kernel, initramfs and `boot-manifest.txt`.
  It refuses kernels without `CONFIG_SECCOMP_FILTER` or `CONFIG_NET_NS`.
- `build-tools/src/manifest.rs` owns `read`, `check` and `lines`.
  `media-manifest` compiles `boot_contract` and `proto_version` from their
  defining constants; `check-media` compares them. Present media without
  a readable manifest is refused; absent media is left to bundling.
- `synod/tauri.bundle.conf.json` runs `check-media` before building the
  bundle; platform configurations carry media, entitlement and MSI service
  resources. `synod/gen/` is regenerated for the build target.
  The check and resource map are selected by `cargo tauri build --config`;
  keeping them in the bundle configuration prevents `tauri-build` from
  probing guest media during ordinary compilation.

## Verification boundary and related reading

The source describes behavior; a successful build alone does not witness a
machine boot.

- The macOS path has prior end-to-end exercise. On Windows, the recorded
  evidence reaches kernel/initramfs/PID 1; completion of a compatible guest
  boot and whether the host's 9p server can read the folder unaided still
  require runtime verification. Windows helper dials remain unsupported.
  `vm-manager/examples/{boot-smoke,broker-smoke}.rs` are the boot probes.
- `dev/docs/VM/SYNOD.md` is the design record; `dev/docs/VM/SYNOD-v1.md`
  records verification status and open runtime questions. The image README holds
  build instructions.
- [[map/exarch/agent|exarch / agent]], [[map/exarch/frontend|exarch / frontend]]
  and [[design/engine-protocol|engine-protocol]] locate the reusable guest
  engine, conversation bus and wire semantics.
- [[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]],
  [[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]]
  and [[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]
  record the media, disk and diagnostic laws.
