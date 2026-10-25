---
generated_at_commit: 19bab4087d1253f3d0894a745beb916ff4dd5124
generated_at_date: 2026-10-25
covers_paths: [synod/, vm-manager/, ral-daemon/, ral-initramfs/, vm-image/, core/src/protocol/channel.rs, core/src/carrier.rs, exarch/src/prompt.rs, exarch/src/agent/build.rs, exarch/src/agent/desk.rs]
---

# Map: synod

synod is a second product over the same engine — an *office-work* delegate
where [[map/exarch|exarch]] is a coding one. The user grants one folder,
describes a task in plain English, and the agent works in it — the folder
itself, in place, and is held to account for it: the folder is recorded
before the job and again after it, and the difference is reported in plain
language. Nothing is ever put back — what is done is done
([[decisions/260807_store-lives-as-long-as-the-conversation|store-lives-as-long-as-the-conversation]],
superseded). It is not a fork of exarch and not a mode of it: it depends on exarch as
a library and supplies only what differs
([[decisions/260721_synod-is-a-second-product|synod-is-a-second-product]]).
It is one crate in two halves: the library modules are the engine anyone
could drive, and the desktop shell rooted at `synod/src/main.rs` is the only
thing that drives it.

The design record is `dev/docs/VM/SYNOD.md`; the landed state is
`dev/docs/VM/SYNOD-v1.md`. Every conversation boots a real hardware machine
from shipped boot media — there is no software-only fallback — and the engine
runs inside the guest, one engine process per session, driven over the
design's §3 wire
([[design/engine-protocol|engine-protocol]]). **One guest,
two lifecycle backends**: Virtualization.framework on macOS arm64, Hyper-V
through the Host Compute System API on Windows x86_64
([[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]]). The
two are the same machine assembled from the parts each platform has, and the
guest cannot tell which one booted it, because every difference is either
invisible from inside (the disk bus, the socket family) or carried on the
kernel command line. On Windows the machine is created by a `LocalSystem`
service the installer registers, asked for over a named pipe by an
unprivileged window, so nobody has to be given a hypervisor's privilege to run
synod ([[decisions/260725_windows-machine-broker|windows-machine-broker]]).

## synod/ — the library

- `lib.rs` — the crate doc names the five differences from exarch. Every
  mutex here and in exarch is locked through `ral_core::sync::LockExt`,
  `pub` as the workspace's one poison policy; neither crate keeps a copy of
  its own.
- `build.rs` — the Tauri build and nothing else (`tauri_build::build()`).
- `examples/check-media.rs` — the one thing the bundle cannot check for
  itself, run as `tauri.bundle.conf.json`'s `beforeBuildCommand`: it reads
  `vm-image/out/boot/boot-manifest.txt` and puts it to both
  `ral_daemon::boot::check_media` and `ral_core::protocol::check_media`, so
  media whose boot contract or engine protocol is not this host's fails
  *packaging* rather than the guest's own reading of its command line or the
  engine's refusal of `Attach`
  ([[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]]).
  `ral-daemon` is a dev-dependency for exactly this; absent media is not a
  failure here, since the bundle is where Tauri names a missing resource.
- `boot.rs` — the media this build ships, found and readied once.
  `boot_media()` looks in three places, each simply *a place a file might be*
  rather than a `#[cfg]` branch: a macOS bundle's `Contents/Resources/boot/`,
  a Windows installation's `boot/` beside the executable, then the development
  pipeline's `vm-image/out/`. `BootPlan::realise` inflates a shipped zstd
  rootfs into the XDG cache against its `sha256` sidecar — a signed bundle is
  read-only, so the cache is the only writable home the image has — and yields
  a `vm_manager::BootArtifact`. The inflate is bounded by
  `vm_manager::ROOTFS_WINDOW`, which is `vm-image/build.sh`'s `--long=27` read
  from the other side: a decompressor unwilling to allocate the window the
  compressor chose cannot read the archive at all, so the two must be raised
  together. Both this inflate and the Windows broker's wrapping counterpart in
  `hcs/vhd.rs` funnel through the one rootfs inflate,
  `vm_manager::media::inflate(archive, out, verify: Option<&str>)` —
  verification is a parameter rather than a second copy of the loop: this
  caller passes the `sha256` sidecar as `verify`, `hcs/vhd.rs` passes `None`.
  Both write to a `.part` renamed into place only on success, and remove it
  on any failure, so a full disk is not left holding gigabytes it cannot
  use. What `session::begin` hands
  `vm_manager::detect` is a `vm_manager::BootMedia` closure over `realise`,
  not the artifact itself.
- `grant.rs` — the folder becomes a `ral_core::shell::runtime::Capabilities`
  ([[design/grant|grant]]), and a `vm_manager::MachineSpec` naming the same
  folder. One grant, read twice: once as authority, once as a workspace.
- `prompt.rs` + `data/*.md` — the office persona and the office toolbox,
  assembled through exarch's own section renderer (`exarch::prompt::render`)
  and grant placeholder (`exarch::prompt::GRANT_PLACEHOLDER`, filled per agent
  from its own engine), over a `host_section` of synod's own that tells the
  agent guest truths only. The ral language
  body itself is exarch's shared `data/ral.md`, included verbatim
  (`include_str!("../../exarch/data/ral.md")`), with synod's own
  `data/office-examples.md` adding table inspection and document conversion:
  retain the table while returning a bounded sample, then audit a conversion
  and reopen its output before reporting success. Exarch includes only the
  shared reference. Synod's base ends in
  **Talking to the user** (`data/surface.md`); the shared exarch per-agent
  resolver then appends **Agent** to returning helpers, after that office
  guidance, while the conversing synod trunk receives no return section. The
  same resolver appends spawn guidance while the trunk or helper still has fuel,
  so synod's prompt composition stays on the shared construction rules.
- `session.rs` — `Conversation`, one folder held open from first message to
  last, split by concern into `session.rs` itself plus
  `session/{menu,signin,baseline,opening,engine_log}.rs` (no `mod.rs`): `session.rs`
  keeps `SYNOD`, `Choice`, `Conversation`, `seat_machine`, `unseat_machine`,
  `select_account`, `control_seat`, `net_seat`, and `resolve_tuning`,
  re-exporting the rest. `begin` opens the grant, boots the machine, and
  seats exarch's agent on the wire the machine hands back
  (`exarch::agent::RootSeat::Wire` over `Machine::take_wires`, attached at
  the guest's `/work`) through `seat_machine` — the shared machine-seating
  seam, with `unseat_machine` its inverse for `end`.
  `synod/examples/boot-run.rs` calls both instead of reproducing them:
  `unseat_machine(agent, factory) -> Box<dyn Machine>` enforces the
  drop-agent → recover-machine → shutdown ordering by ownership, so the
  wrong order is unrepresentable rather than merely discouraged.
  `control_seat` carries no platform condition at all: `take_wires` hands
  back each platform's own owned handles and
  `ral_core::carrier::WireTransport::adopt` takes either, so the protocol
  is one function ([[design/engine-protocol|engine-protocol]]).
  Before any of that, `begin` spawns the folder's opening walk on its own
  thread as a `session/baseline.rs` `Baseline`. That walk is stat-only —
  path, kind, size, `mtime_ns`, mode, and not one byte of the user's folder
  read — so it is one walk, not a stat pre-flight followed by a full-byte
  capture, and it costs a stat walk however large the folder is. Nothing is copied, so nothing asks whether a copy would
  fit: there is no store, no free-space pre-flight, and no opening line
  about either. `begin` never joins the thread itself, so the boot runs
  alongside the walk rather than after it and the conversation opens the
  moment the boot is done; the walk reports its running file count through a
  progress callback the window renders, and carries a `Stop` the window
  trips when it closes or restarts, so a slow share is never something a
  user has to wait out.
  An unsettled baseline's walk thread is owned by a nested `PendingWalk`,
  the one thing in `Baseline` that implements `Drop`: dropped unjoined —
  `begin`'s error path, once `?` runs straight through rather than by way of
  an explicit abandon — it stops and joins the walk itself and discards
  whatever it produced, so no error path in `begin` can leave a walk thread
  running past it. Only an *unjoined* walk is stopped there: the switch ends
  the walk for good, and tripping it on a walk `settle` already took would
  end the baseline that walk was producing.
  `exchange` settles the baseline first — joining the capture thread on its
  first call, every call after finding it already settled — before it drives
  anything, since the guest must never write into a folder whose baseline is
  still being read, then drives one message through
  `exarch::headless::converse_settled`
  ([[decisions/260806_exchange-ends-at-fleet-quiescence|exchange-ends-at-fleet-quiescence]]),
  then takes the closing walk and holds the `JobReport` the window reads
  back — taken even after a failed run, since whatever changed before the
  failure still changed. That closing walk waits for fleet quiescence, not
  merely the trunk's own silence, so a helper still writing to the folder
  never races the report. **A lost engine ends the conversation**: after that
  walk, `capture_a_dead_engine` reads the agent's severance, writes
  `engine.log` while the machine is still up, and stores the `EngineLost`
  sentence as `ended`, which that exchange and every later one answer with
  rather than reaching the dead engine. `end` closes the wire — the guest halts itself —
  and only then joins a walk still running (a conversation closed before its
  first message). There is nothing to wipe: a conversation leaves nothing on
  disk at all. The trunk's fuel is
  `SPAWN_FUEL` (3), the same depth budget exarch's own trunks carry —
  promoted `pub` for exactly this reuse — and `RootConfig` carries a
  `Factory` implementation over `Machine::connect_guest` (below), handed to the
  fleet's `Launch`, so synod's
  office assistant may delegate to helpers that run concurrently in the
  same guest, against the same folder, and in the same report. Only `vz.rs`
  implements `connect_guest`; on Hyper-V a spawn is refused with the trait's
  `Unsupported` default.
- `session/menu.rs` — the model picker: `menu`/`refresh_menu` list what the
  computer's credentials can reach (cached-instant and fetched-complete), and
  a `Choice` names an `AccountId`, model, and effort — an id and never a
  display name, because a name that happened to match another account's would
  start a conversation on someone else's login. What flows the other way for
  display — the opening's and a finished sign-in's account name — is spelled
  `label` on the wire, so an id and a display string cannot be mistaken for
  one another in either direction.
- `session/signin.rs` — `sign_in` drives exarch's browser login flow
  (`exarch::provider::oauth::login_flow`, always `SignIn::Register`) and admits the fresh account to the
  live store and catalog through `exarch::provider::admit_login` — the same
  call exarch's own front end uses — so a ChatGPT plan signed in from the
  window is usable without a restart. The credential store is behind a
  `Mutex` for exactly that reason, taken only for an account list or an
  admission, never across a fetch or a boot. `admit_login` and
  `pricing::ensure_loaded_blocking` (`session.rs`'s `resolve_tuning` calls
  the latter) are the shared provider facts both front-ends call;
  synod's own `Cargo.toml` names no `tokio` dependency. `prepare` itself
  only delegates: where synod's accounts come from is `accounts.rs`.
- `session/opening.rs` — `Opening`, what the window shows before the first
  message: who is answering, and at what effort. It says nothing about the
  folder: nothing is copied, so there is no copy or free space to warn about.
- `session/engine_log.rs` — `capture` writes `engine.log` into the run
  directory when a conversation's engine is lost: the sentence the user was
  shown, under an `Assistant engine lost` heading, then the guest console's
  capped copy (`vm_manager::GuestConsole`), the one place an engine's dying
  words reach the host. On macOS no copy is kept, and the file says so rather
  than reading as a silent guest.
- `accounts.rs` — **synod's own credential story**, and the one place it
  stops borrowing exarch's. A key reaches exarch through the environment
  because exarch is started from a shell; synod is double-clicked, inherits
  the desktop's environment, and faces someone with no `.zshrc` to export
  from. So there are two layers, in exarch's one order: the launch
  environment's key first, the same sweep and scrub as exarch, and the
  computer's credential manager beneath it (`exarch::provider::keychain`,
  entries named `(synod, account-id)`, the app being `app::SYNOD`
  re-exported from the engine so the credential deny and synod's own
  directories cannot drift apart), read by `CredentialStore::read_vault` over
  a `SecretVault`, which `Keychain` implements. A row whose key comes from the
  environment says so, naming the variable and any saved key beneath it.
  Which services *exist* is a third thing and no secret, and synod re-derives
  none of it: the table is `exarch::provider::identity::built_in_services`,
  and further endpoints are declared in
  `$XDG_CONFIG_HOME/synod/providers.ral` — synod's file, in synod's directory,
  holding addresses and protocols but never keys — read through
  `exarch::provider::accounts`.

  Writing is `exarch::wallet::Wallet`'s: `accounts::wallet()`
  builds synod's (its vault, its `providers.ral`), and `shell/keys.rs` calls
  its `set_key`, `forget_key`, `add_endpoint` and `forget_endpoint`; the row
  derivation is exarch's `accounts::entries`, which synod's wire `Account`
  maps from.

  What stays synod's is what is about synod's *window*. A row carries the
  account's `identity::label`, where the credential in force came from, the
  hint naming this computer's vault, whether the row can be withdrawn, and at
  most the last four characters of the key in force and of a saved key it
  outranks; the `source` is read off the store's two layers, so drawing the
  list costs the vault nothing. A server that checks no key is reported as
  `keyless` outright rather than left to be inferred from a missing hint, and
  is declared so by the add form's own checkbox, never by a blank key. The old three-way `kind` is
  gone with the provenance it encoded — "is this a login" is `source ==
  SignedIn` — and since one service may own several ChatGPT accounts, the
  static sign-in card gives way to rows drawn from `store.available()`.
  See [[decisions/260807_synod-keeps-its-own-accounts|synod-keeps-its-own-accounts]].

## synod/src/workspace/ — the account of what changed

The module the product's one remaining promise lives in, all host-side,
exercised under ordinary `cargo test`. It promises an honest account of what
a job changed, and nothing about putting anything back. It states its
host-side standing rather than assuming it: `manifest.rs` and `report.rs`
are each one kind of site — synod's own walk of the granted folder, never
the model's turn-time I/O, which happens inside the guest — so each says so
once, at module scope, in the shape `vm-manager/src/hcs/vhd.rs` uses. synod holds no crate-level exemption from the I/O and path
disciplines in either of its two roots; every exception is named where it is
taken:

- `manifest.rs` — a folder's state at one moment, from `symlink_metadata`
  alone: path, kind, size, `mtime_ns`, mode. No hash, because no file is
  ever opened — the walk reads the folder's shape and never its contents,
  which is a privacy property as much as a cost one. `of_folder_via(root,
  stop, progress)` is the one walk, over a visitor (`Visit<'a>`, an `FnMut`
  over a key, a path, and its metadata); `progress` is handed the running
  file count so a caller can say something during a slow share, and `covers`
  — the crate's one folder-prefix-with-boundary rule — lives here beside the
  keys it resolves. A path the walk listed but could not record — a subtree
  whose `read_dir` answers `NotFound`, a file gone before it could be read,
  an entry gone between listing and `symlink_metadata` — goes into
  `Manifest::unread`, the subtree root alone rather than every key beneath
  it, and `changes.rs` suppresses anything it covers. Silence there was the
  worse bug: a subtree missing from a baseline for an ordinary reason (a
  share blipping, a cloud placeholder unhydrated) came back as *created by
  the assistant*. A `root` that is gone is an error, not an empty manifest,
  and so is a root the walk could not enter: the walk reports a directory it
  did not enter as `Unentered`, never as seen; for the same reason at the
  limit. Empty folders and symlink targets recorded, links never followed. A `Stop` the walk reads once per entry
  ends a walk nobody is waiting for any more; a stopped walk raises
  `WalkError::Stopped` and answers an error, never a short manifest, since a
  truncated record reads as one where everything unread had been deleted.
- `changes.rs` — the delta between two manifests: created, **modified**
  (size or mode differs, so the contents certainly changed), **touched**
  (the timestamp moved and nothing else did — something wrote to the file,
  and whether the bytes differ cannot be known without reading them, which
  synod does not do), deleted, and renamed. A rename pairs a deletion with a
  creation on identical `(size, mtime_ns)`: a move preserves both, which
  makes it a sharper signal than the content hash it replaced — a hash match
  is true of every pair of identical files in the folder, empty ones
  included. A key many candidates share on both sides is left unpaired,
  since mass duplication is not a mass rename, and an honest deletion beside
  an honest creation is never a lie where a confident rename would be.
- `report.rs` — the GUI's seam: `job_report` is a pure, infallible function
  of two manifests, and the live `Conversation` holds them, so drawing a
  report re-reads nothing from disk and cannot fail.

## synod/src/shell/ — the window

The desktop shell (Tauri v2, hand-written static frontend, no bundler, pure
cargo) is the one process: it holds the `Conversation` in-process — no child
binary, no stdin framing. One window, three states: choose a folder and a
model (with a Thinking control beside the Assistant picker) and describe the
job; watch the assistant work, its narration streamed in; then read what
changed. `mod.rs` owns `Accounts` — the credential
scrub's outcome as an `exarch::provider::Holdings` (the store, the model
catalog built from it, and the allowance record), a private field reached only
through `resolved()`. The holdings are shared, because a conversation outlives
no store of its own: `Conversation::begin` builds an
`exarch::provider::Bureau::Live` over the very holdings the window keeps, plus
the engine it mints per conversation, so a sign-in admitted through the window
is visible to a running conversation's next spawn and the two can never drift.
`mod.rs` also owns the two refresh entries, `refresh_menu_now`
(synchronous) and `refresh_menu_async` (off the calling thread); the debounce
that decides when a refresh is worth asking for at all, `RefreshGate`, lives
in `commands.rs`. `commands.rs` holds the folder picker, which answers a
`PickedFolder` — the native `path` the window hands back as it came, and the
shown `label` it displays — the
conversation verbs (start, send, restart, end), the model listing (instant
from the cache, one background refresh), and opening before/after versions
with the user's own applications. The worker's `Emitter` is gated on the
generation that owns the conversation slot, and it speaks the shell's own
states into the status bar's one station before there is an agent whose
states to relay: `starting`, or `finishing the previous session` while a
restart waits for the generation it superseded to shut its machine down —
a wait the window otherwise sits through in silence, since two stores must
not walk one folder — through to `ready` at the opening, where the station
passes to the agent. The window writes that station only for `Working…`;
it never clears what the shell has said. `keys.rs` is the accounts screen's five
commands — list, save or forget a key, declare or withdraw an endpoint — each
returning the fresh list and ending in the same `models-refreshed` event a
sign-in ends in, so the picker converges the same way whichever route a
credential arrived through; `sink.rs` is the bridge that streams the
conversation's narration into the window, and routes by agent id: its
`Router` takes the first id it ever sees as the trunk's (`Router::root`),
since a fresh trunk always turns before any `agent` call can reach the model,
and every other id is a helper's. A helper's `Token`/`Thinking`/`State` events are
dropped (a helper's prose is not the conversation); its `Observation`/`Notice`/`Context`/
`Resources` fold to `ProcessCard` exactly as the root's do, `Done` is dropped for both, and its
`Card` folds in beside them — deliberately, where the root's own `Card` stays
first-class, since a helper's card is process, not conversation; `Usage`
still counts, the bill being the exchange's whoever spent it. A birth
(`Sink::born`) and a `Transient::Died` become `SynodEvent::Helpers { live: u32 }`, a counter the sink accumulates and
emits on change — one fixed-position magnitude mark in the dial region, no
stream, no animation, nothing entering the transcript. `SubagentDone` — which
arrives on the *root's* emitter, since `announce` runs in the parent's own
drain — becomes `SynodEvent::HelperDone { name, ok, elapsed_secs }`, rendered
as one process line inside the dial's rung; `WaitingOnAgents` maps to a
status-bar label. `sink.rs` also folds `ProviderError` at the seam:
`SynodEvent::ProviderError`/`Stalled` carry `{ text, severity }` — the
sentence composed once, where the module's deny reaches the fold — and
`Transient::Boundary` is carried across as `SynodEvent::Boundary`, which
tells the window that the turn has sealed and so the streaming bubble's last
markdown block is closed. Every enum on this wire is `#[serde(rename_all = "snake_case")]`
— the JSON seam speaks `snake_case` throughout, matching exarch, with no
`camelCase` renaming anywhere on it. Every type that crosses the seam derives
`ts_rs::TS` beside its `Serialize`, so the TypeScript the window is checked
against is written *from* the Rust rather than kept in step with it by hand:
`just ui-check` generates it into `synod/ui/js/bindings/` and runs `deno
check` over `synod/ui/js/`, which is the one check that reads both languages.
A `case` the Rust has no variant for is a type error, and a variant the window ignores is caught by the
`never` assertion in each switch's `default`. A mark's *payload* is
deliberately not typed: those four shapes are exarch's card vocabulary, and
typing them would put `ts-rs` in exarch to serve synod's window alone, so they
cross as open records and the discriminant carries the check. Plain register throughout: the window says *helpers*,
never agent/session/model, and there is deliberately no tab strip — the
assistant delegates onward, and the window reports the folder, not the org
chart. `signin.rs` runs the opening screen's
"Sign in with ChatGPT" button — one sign-in at a time, cancellable, its
progress and outcome events (`sign-in-step`, `sign-in-done`) rendered beneath
the button, and the account it wins arriving as the same `models-refreshed`
the picker already renders through; with no account set up the sign-in is the
screen's primary button and the folder picker waits for it; `review.rs`
translates the workspace vocabulary into cards, and carries
`WindowReport::unreadable` beside them — what the walk could not read, so
the panel can say what it is not answering for. The surface is read-only:
there is no status to distinguish, nothing to put back, and no conflict a
caller could be asked to resolve. That holding type, `Held`, carries the
report's own folder and refuses a later call whose `folder` argument does
not match, so a stale or mismatched frontend can never draw one job's
report against another's folder.
`synod/src/main.rs` runs exarch's
`exit_if_re_exec_child` re-exec trampoline first, like every
[[invariants/single-binary|multicall]] binary here.

The frontend is `synod/ui/`: `index.html` holds the markup and its tags,
`css/*.css` the sections the stylesheet's own comments already delimited, and
`js/*.js` the frontend's concerns as ES modules, one per file — `type="module"`, no bundler, which
is the whole reason the frontend is hand-written. `js/core.js` is what every
other module imports (`$`, `invoke`, `listen`, `state`, `show`); `app.js` is
the only file `index.html` names, and its `render()` is the first paint, after
the module graph has loaded. `chat-document.js` and `projector.js` import each
other, as do `conversation.js` and `projector.js`: safe because every binding
crossed is a hoisted function declaration and nothing calls across a cycle
during evaluation, which is stated at the import itself. Beside them are the
three libraries the window vendors and the nothing it fetches:
marked (GFM), DOMPurify, and KaTeX 0.18.1 (its stylesheet and its twenty
`woff2` faces; the only web fonts the app ships), each verbatim under
`ui/vendor/` beside its licence, with versions and SHA-256s in
`ui/vendor/README.md`.
Assistant prose is markdown with TeX, and `renderAssistantMarkdown` is the
single path model text takes to the DOM. Its order is the load-bearing part:
each formula is **lifted out before marked sees it** — markdown would read
`x_1 * x_2` as emphasis, and `breaks: true` would cut a multi-line `$$` with
a `<br>` — leaving a private-use sentinel that the prose carries through
marked unharmed; KaTeX's nodes (with `trust` off, neither a link nor a raw
node) replace each sentinel in the parsed, inert tree, and **DOMPurify scrubs
that whole tree last**, returning a fragment the caller appends as it is. No
scrubbed tree is ever serialised and parsed again, which is what closes
mutation XSS; one `SCRUB` config keeps KaTeX's MathML and inline metrics and
drops every element that could navigate the window or restyle it. An
unterminated formula is not one, which is what keeps a half-streamed `$$`
from flashing red while it arrives; a `$` inside code, beside a space, or
against a digit (`$5-$10`) is a dollar sign, not a delimiter. Code is judged
twice, because it must be: the scan before marked knows only fences and
backticks — whether four spaces open a code block or continue a list item is
a question only a block parser can answer — so a sentinel that marked put
inside a `<code>` is handed back as the text it was written as. No message
wears a name above it — who spoke is said by the bubble's side and colour.

A bubble that is still arriving is not re-rendered whole. `prose.js` treats
it as what it is — a document with a **settled prefix and one open block** —
and cuts the source at markdown block boundaries: `marked.lexer` runs over
the unsettled tail alone, everything before the block still open is rendered
once into the bubble and never touched again, and only the tail is rebuilt as
tokens land. That is what makes rendering *every* token affordable, where
re-parsing the whole message per token is quadratic in it (3.3s of main
thread by 8,000 tokens, measured): the window never shows raw markdown
between flushes, and settled prose — with its coloured ral and its laid-out
KaTeX — never reflows under the reader. Two facts decide where the cut may
fall. A list or an indented code block is *rejoined* across a blank line by a
later one of its kind rather than followed by it, so a trailing run of those
stays open together; every other block closes the moment another follows it.
And because `renderAssistantMarkdown` lifts formulae out before marked sees
them, a cut must never fall between a formula's delimiters, so a chunk
settles only once the multi-line mathematics opened inside it has closed
there too — judged with the same `codeMask` that decides where a `$` is only
a dollar sign. `SynodEvent::Boundary` and the close of a bubble both settle
the tail outright, having the turn's own word that nothing can join it. The
open block ends in a `caret`: the writing front as a mark at a fixed place in
the text, not an animation played over prose already read.

A ral listing — a ```` ```ral ```` fence in the prose, or the script on a dial
row — is coloured by `ral-highlight.js`, which asks the shell
(`highlight_ral`) and wraps the answer's spans itself, building text nodes
rather than markup because the source is the model's. The classification is
`ral_core::syntax::highlight`, the lexer's own reading of what a token is and
the one exarch's TUI colours from, so the two front ends cannot disagree and
neither keeps a grammar of its own. Byte offsets become UTF-16 ones at the
command, that being where the window's indices start; the result is cached on
the exact source text, since a streaming block re-renders many times per
second.

## vm-manager/ — the machine

One trait each side of a boot: `Hypervisor::boot(&MachineSpec) ->
Result<Box<dyn Machine>, Error>`, with `MachineSpec::resolve` the one
platform-independent judgment of a spec, called by every backend so a bad
spec is refused in the same words everywhere. `BootArtifact::resolve` is its
twin for the media, and makes every file absolute: the paths are opened by
*another process* — `vmcompute` runs in `C:\Windows\System32` — so a relative
path that resolved for the caller names nothing by the time the machine is
built. `Machine::take_wires` is the one signature that varies — `Wires` holds
an `OwnedFd` per wire on Unix and an `OwnedSocket` per wire on Windows —
because each platform owns its own accepted sockets, and both are adopted by
the engine protocol unchanged.

**The crate boots only real machines.** `detect(Option<BootMedia>)` answers
`Vz`, `Brokered`, or `Hyperv`, or refuses with an `Error` worded for a
non-programmer: not a platform with a hypervisor at all (`NoBackend`), no
boot media (`NoBootMedia`), a macOS build unsigned for virtualization
(`Unentitled`), or a Windows account the compute service will not serve
(`NotPermitted`). On Windows the order is the machine broker first —
which needs no boot media from this process at all, since the service has its
own installed beside it — and only then the in-process `Hyperv`, which is a
checkout rather than an installation. There is deliberately no software
fallback: a synod that cannot put hardware between the agent and the rest of
the computer refuses to start rather than degrade to a weaker mode.
`examples/boot-smoke.rs` is the human-driven boot, one body over both
backends.

**Every failure is one data-only `Error`** (`error.rs`), from `detect`, the
`BootMedia` closure, `boot`, and `shutdown` alike: paths are carried as
`PathBuf`s, there is no `Display`, and the sentence is assembled only where it
is shown, by `Error::worded(spell)` with the front-end's spelling (synod passes
`ral_core::path::shown`, once, at its boundary in `session.rs`). A sentence
says what went wrong and never at which stage — the caller asked for a
backend, a boot, or a stop, so it frames that (`session.rs`'s "could not start
a machine for …", "the machine did not stop cleanly: …"), and nothing is
framed twice. The variants with data of their own are their own —
`NotConstructed` (the five files the hypervisor was handed), `Undialled` (the
guest's console), `ForcedStop` (the grace), `VersionSkew` (both broker
versions), `ServiceUnlocated`, the folder and `HostFile` refusals — and
`Backend { hypervisor, why }` is the rest: the hypervisor's own complaint, or
the machine layer's account of the plumbing around it. Synod's own media
readying (`boot.rs`) refuses in the same `HostFile` vocabulary the machine
layer's inflate does. Serialised for the broker's pipe, with every path — in
the `Error` and in `Request`/`Reply` — encoded by `os_path` as the platform's
own units (bytes on Unix, UTF-16 little-endian on Windows) in one base64
string, so a path that is not Unicode crosses whole rather than failing the
frame that was to carry the refusal, at a uniform 8/3 bytes per UTF-16 unit
(`serde_json` would write the units as an array of numbers, up to six bytes
each). Crossing is not reaching a hypervisor, though: a path the HCS document
would have to carry as text, and that is not Unicode, is refused as
`NotUnicode`, never spelled lossily into some other file's name.
Virtualization.framework is handed an `NSURL` made from the path's own bytes,
so a name that is not Unicode reaches it whole; only a path with no `file:`
URL, empty or holding a NUL, is refused, as `NotAFileUrl`.

- `vz.rs` — `Vz`: Virtualization.framework, macOS arm64, bound through
  `objc2-virtualization`. It builds and validates the full configuration —
  direct kernel boot, RO rootfs + RW sparse session disks, a virtiofs share
  of the granted folder with `read_only` as the mount's law, a vsock device
  and no network device, console to the host log — drives the `!Send`
  machine from a dedicated thread against a private serial dispatch queue,
  and declares boot only when the guest's daemon has dialled both the
  control port and the net port. One socket device multiplexes them: a
  second network *device* is exactly the fix that must never be made, and a
  test asserts `socketDevices().count() == 1` to say so.
  Those accepted connections are the host ends of the §3 control plane and
  the §6 net wire: `Machine::take_wires` hands both out exactly once (a
  second ask panics as a caller's bug), and a second guest dial is refused. Booting requires the
  `com.apple.security.virtualization` entitlement — `vz::entitled()` is a
  process check, not a platform check.

  Those two listeners are the only ones: every other wire a machine carries
  the *host* opens. `Machine::connect_guest(port)` dials a listener bound
  inside the guest and hands back its host end — a `Command::Connect` on the
  machine thread under this backend, defaulted to a refusal sentence on any
  backend that cannot dial inwards. There is no third listener, no accept
  pump, no published preamble, and no host-side table: a connection the
  host opened needs nothing to correlate it, because the side that opened it
  already knows what it opened it for. The listener it dials is the guest
  daemon's ([[#ral-daemon/ — the guest's PID 1|below]]).

  `synod/src/machine_factory.rs` is that seam's synod end — `MachineFactory`,
  an `exarch::agent::Factory` ([[map/exarch/agent|agent]]) over a machine it
  holds in trust from `Conversation::begin` to `Conversation::end`, which takes
  it back with `into_machine` once the agent that shared it is gone. Each
  `engine()` dials `ral_daemon::boot::FACTORY_PORT`, never spelled twice. The
  `Mutex` it wraps the machine in is for `Sync`, not for the dial: `Machine` is
  declared `Send` and no more, and one `Arc<dyn Factory>` crosses the desk's
  threads. The conversation's scratch is `GUEST_SCRATCH` (`/tmp`), a
  `ral_core::path::GuestPath` const, named as `$EXARCH_SCRATCH` in the root's
  `Attach` and in every child's, with its tool homes from
  `exarch::app::guest_scratch_env`, `/`-joined whatever the host. The root's
  `Attach` (`control_seat`) names its `HOME` and cwd as `EnginePath::Guest`
  paths: the scratch, and the mount point `Machine::workspace_path` reports,
  which `vm_manager` still hands over as a `Path` and `seat_machine` lifts
  into a `GuestPath` through `path::utf8`.

## vm-manager/src/hcs/ — the Windows machine

`Hyperv`: Hyper-V through the Host Compute System API — `computecore.dll`, the
surface the Virtual Machine Platform feature provides and the one WSL 2 and
Linux containers are built on. This is the module the broker below runs in its
own process; `detect` reaches it directly only in a checkout. `available()`
answers in one of three remedies rather than one failure: the feature is not
installed, this account is outside the computer's local **Hyper-V
Administrators** group, or the compute service is not answering at all. An HCS
system has no thread affinity — it is a handle, not a queue-bound object — so a
`Guest` holds its machine directly and the only threads in the backend serve
blocking I/O.

- `mod.rs` — `Hyperv`, `Guest`, `available()`, the refusal texts, and the table
  of correspondences with `vz.rs`. `boot`'s order is load-bearing at three
  points: the console pipe exists before the machine that names it, the
  control-plane listener is bound before the machine *starts*, and
  `HcsGrantVmAccess` runs on the four boot files before a worker process opens
  them as its own virtual account. `Guest::stop` closes the wire first, so the
  guest powers itself off from inside, then revokes every access entry it
  granted, so none naming a dead per-machine identity is left on anyone's
  folder; `Drop` shares that path. Three constants carry the teardown's own
  patience — `REMOVE_GRACE`/`REMOVE_PULSE`, over which `remove` waits out the
  worker process that holds the session disk past `Stopped`, and `ORPHAN_AGE`,
  above which `Hyperv::new`'s `sweep_orphans` reclaims what earlier runs left;
  `session_disk_epoch` is what makes that sweep incapable of naming anything but
  a session disk
  ([[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]]).
  Both dial timeouts are `Error::Undialled` (`vm-manager/src/error.rs`), whose
  wording quotes the guest's own last lines and names its log, and `Guest::dialled` is what decides whether that log
  survives the teardown
  ([[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]).
- `api.rs` — the entry points, resolved with `LoadLibraryW`/`GetProcAddress`
  rather than statically imported, so a Windows without the feature gets a
  sentence instead of a process that will not start. `Api::settle` holds the
  whole operation protocol — mint an operation, hand it to the call, read the
  real outcome and the service's own JSON error text out of
  `HcsWaitForOperationResult` — in one place, and `HCS_E_ACCESS_DENIED` is the
  one code recognised rather than merely reported.
  `HcsGrantVmAccess`/`HcsRevokeVmAccess` are looked for in `computecore.dll`
  *and* `computestorage.dll`, because they are exported by the former —
  Microsoft's own documentation and Go binding name the latter, which on
  10.0.26100 exports neither.
- `spec.rs` — the machine as one JSON document, since HCS takes no builder
  objects: `Chipset.LinuxKernelDirect`, `ComputeTopology`, `Devices.Scsi` (the
  rootfs at LUN 0, the session disk at LUN 1), `Devices.Plan9` (the granted
  folder, with `LINUX_METADATA` always and `READ_ONLY` when the grant is),
  `Devices.HvSocket`, `Devices.ComPorts`,
  `ShouldTerminateOnLastHandleClosed`, and **no network adapter at all** —
  absent, not disabled. Being data rather than a sequence of setter calls, the
  document is built and read back under ordinary `cargo test` with no machine
  and no privilege. `kernel_command_line` writes `ral.port` and `ral.plan9`
  from named fields, never positionally: the guest would mount its own control
  plane if the two were ever swapped.
- `hvsock.rs` — the control plane. `service_guid` is the entire bridge between
  the two addressing schemes: a Linux vsock port `p` is the service GUID
  `pppppppp-facb-11e6-bd58-64006a7986d3`, which is why a guest that knows
  nothing of Windows can still be dialled. `socket_sddl` names SYSTEM,
  built-in Administrators, and *this user's own SID* — never a wildcard, since
  this socket is one of two routes into a machine with no network *adapter* of
  its own (the guest's actual network rides the second `HvSocket` port,
  `NET_PORT`, into a host process — [[design/egress|egress]]) — and
  `fresh_machine_id` draws on `ProcessPrng` because the machine's identifier
  *is* half the socket's address.
- `vhd.rs` — a `VirtualDisk` attachment must be a VHD, so the raw ext4 images
  are wrapped as **fixed VHDs**: the sectors verbatim followed by one 512-byte
  footer, which makes wrapping an append rather than a conversion — no block
  map, nothing transcoded, the filesystem identically placed.
  `ensure_rootfs_vhd` does it once into `%LOCALAPPDATA%\Synod\Machine\` behind
  a marker recording *which* image was wrapped, and passes a shipped `.vhd`
  through untouched; `create_session_vhd` makes the session disk, which the
  guest formats on every boot and the machine's teardown deletes. That one is a
  **dynamic** VHD declaring 8 GiB — ~18 KB of metadata when empty — because
  Hyper-V refuses a virtual disk whose *file* is sparse (`0xC03A001A`), so
  growth has to be the format's business rather than the filesystem's.
- `console.rs` — the guest's `ttyS0` on a named pipe the compute service dials
  as a client, because without it a boot that failed and a boot that is merely
  slow are the same timeout. The pump *tees*: `stdout`, a per-machine
  `synod-console-<id>.log` in the same cache the disks live in, and `Tail`, a
  ring of the last lines the boot failure quotes. `RETAINED_LINES`, `LINE_LIMIT`,
  `LOG_LIMIT` and `LOG_LIFETIME` are the four bounds that keep the diagnostic
  from becoming litter, and `discard` is what a boot that dialled calls on its
  own log ([[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]).
  `Console::wake` connects to its own pipe to release a pump parked on a machine
  that never started.

## vm-manager/src/broker/ — the privileged half, on Windows

The service that owns the machine so the window does not have to, and the
client that asks it. One instruction crosses (`Request::Boot` — a folder and a
read-only flag), and everything else about the machine is the service's own
([[decisions/260725_windows-machine-broker|windows-machine-broker]]).

- `mod.rs` — the protocol and the argument: `PIPE`
  (`\\.\pipe\synod-machine-broker`), `VERSION` checked before anything else,
  the `Request`/`Reply` pair, and length-prefixed JSON with a 512 KiB frame cap
  — sized for a refusal naming five longest Windows paths, the arithmetic on
  `frame::MAX` — written out here rather than borrowed from `ral-core` (a machine layer that
  needed the shell to talk to its own service would have the dependency
  backwards). `Request::Adopted` is the third step of `Boot` → `Booted` →
  `Adopted`, and its doc is where the ordering trap is stated.
- `client.rs` — `Brokered`, a `Hypervisor` that asks rather than acts, so
  `detect` can prefer it without `synod::session` or the seat knowing which
  backend it got; `Brokered::available()` is the probe `detect` asks.
  `adopt_socket` turns the service's `WSAPROTOCOL_INFOW` bytes back into an
  `OwnedSocket`. `BrokeredGuest` holds the pipe as the lease: `shutdown` and
  `Drop` both close it, and closing it is what stops the machine.
- `service.rs` — the only privileged code synod ships, and the file to review:
  `PIPE_SDDL` (`D:P`, SYSTEM + built-in Administrators + *interactively
  logged-on* users, never `AU` and never a wildcard), `serve`/`serve_client`
  (one machine per connection, held in the serving thread's local; a client of
  another protocol version is refused with `Error::VersionSkew`),
  `boot_for` (a folder whose name is not Unicode refused first, as
  `Error::NotUnicode`, since the machine's document could only spell it as some
  other name), `open_for_client` (`ImpersonateNamedPipeClient`, the folder
  opened as the caller, reverted by a `Drop` guard on every path out including
  an unwind),
  `client_process` (`GetNamedPipeClientProcessId` — the kernel's answer, not
  the client's), `describe_socket` (`WSADuplicateSocketW` for that one process
  id), `media` (the boot artifact beside *this* executable), and `cache`
  (`%ProgramData%\Synod\Machine`, machine-wide because the wrapped rootfs is
  identical for every user and `LocalSystem`'s `%LOCALAPPDATA%` is SYSTEM's
  profile).
- `vm-manager/src/bin/synod-machine-broker/` — the program: `main.rs`, the
  entry point every platform gets, since a Cargo binary target belongs to the
  package and not to a platform; and `service.rs`, the two ways it starts on
  Windows — the service control dispatcher (`SERVICE_NAME` =
  `SynodMachineBroker`, report `RUNNING` before serving, stop by process exit
  since the threads own the machines), and `--console`, the same behaviour with
  a terminal attached, which is how a maintainer sees the guest's own console
  say why a kernel did not come up.
- `synod/wix/broker-service.wxs` — the installer side: a WiX fragment
  (referenced from `synod/tauri.windows.conf.json`) declaring the service into
  `INSTALLDIR`, so it shares the one `boot\` directory with the application;
  `LocalSystem`, automatic, started at install, removed on uninstall.
  `just broker-install` / `broker-uninstall` do the same from a checkout with
  `sc.exe`.

## ral-daemon/ — the guest's PID 1

Runs inside every booted guest. Every decision — the kernel-cmdline
`boot::Boot`, the mount plan and its inside-before-outside ordering invariant
(`mounts.rs`), the guest-wide sysctls the §5 jail depends on (`sysctl.rs`),
the engine's command line and fd plumbing (`engine.rs`: the vsock connection
arrives as fd 3), the classification of a wait result (`reap.rs`) — is a pure
function unit-tested on any machine; the syscalls are a thin edge only a
guest can exercise. The overlay root is deliberately the initramfs's job; the
daemon verifies and names what it was handed. No ral semantics, no authority
policy. It is also the guest's **engine factory**: after spawning the root it
starts one accept thread on `boot::FACTORY_PORT` (the listener is bound before
the root, so a host dial waits in the backlog), admits only a peer whose CID is
`VMADDR_CID_HOST` (`vsock::is_host`), and spawns one engine per connection
exactly as it spawned the root, on fd 3. Transient `accept` errors are retried; any other is fatal to the machine (the factory `kill`s its own process with SIGTERM, so `attend` returns naming it), never a silently dropped listener.
The daemon installs no signal handlers: SIGCHLD and the stop signals (SIGTERM, SIGINT, SIGPWR) are blocked in every thread before the first is spawned (`reap::mask`; std hands a child its parent's mask, so each spawn's `pre_exec` unblocks them), and `attend` takes them in `sigwaitinfo`. On SIGCHLD it drains `waitpid(-1, WNOHANG)` under the fleet lock, so it cannot steal the child std is itself waiting on after a failed spawn, and tears down the dead engines' cgroup trees only after releasing it, so a concurrent dial never waits on a settle; on a stop signal it returns.
When the root engine exits — the wire's EOF is its cue — the daemon powers
the machine off from inside, `halt` signalling every engine's process group
(TERM, a grace for every engine alive then, KILL), not the root's alone: the clean inside-out halt. Commands leave their engine's group, so the KILL sweep also writes `cgroup.kill` at the jail's cgroup root (`mounts::EXEC_CGROUP`), and `attend` does the same for `engine-<pid>` when it reaps a child engine, then removes that tree deepest first. A detached survivor lives in `detached/exec-<seq>` instead, beside every engine's tree, so it outlives its engine exactly as on the host identity seat and only the halt's sweep ends it. The daemon makes that root, delegating `+memory +pids +cpu` into it, beside its mounts (`mounts::exec_cgroup`). A child engine ends when the
host drops its connection.

`Boot::workspace` is where the guest learns which hypervisor's folder it has:
an `Export`, either `Virtiofs { tag }` or `Plan9 { name, port }`, decided by
whether `ral.plan9` names a port on the command line. The 9p arm is the one
mount whose options cannot be written before the mount is attempted, because
`trans=fd` names the descriptor of a *fresh vsock connection to the host's
server* — the daemon dials, sizes the socket's buffers, and mounts
`trans=fd,rfdno=N,wfdno=N,msize=…,version=9p2000.L,aname=<share>` over it.
`ral.plan9` and `ral.port` are two sockets for two jobs and are read by name,
never by order.

The `ral.` key set and its value grammar are one versioned agreement, and
`boot.rs` is where the version lives, beside the command line's only writer and
only reader: `boot::CONTRACT` (3: the factory port), `MANIFEST_KEY`, and `check_media`, the judgment a
*host* build runs over the media it is about to package
([[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]]).
`ral-daemon/examples/boot-contract.rs` prints that constant and nothing else, so
the media's manifest records a number it compiled rather than one it read.

## vm-image/ and ral-initramfs/ — the boot media

The design record's §7 built, and `ARCH`-parametric over synod's two guests:
`ARCH=arm64` for Virtualization.framework, `ARCH=amd64` for Hyper-V. A build
refuses a container that is not its own architecture rather than let qemu-user
emulation quietly produce something else. `build.sh` assembles the rootfs — a
pinned **Ubuntu 26.04 LTS (resolute)** office userland (LibreOffice headless,
the Python document stack, pandoc, OCR, wide fonts, full locales, no
toolchain) via mmdebstrap → ext4 → zstd in a native container, checksummed and
version-manifested. `build-boot.sh` builds the boot pair, stamping the git hash
`boot_contract=`, and `proto_version=` into `boot-manifest.txt` — the two
lines packaging reads back — and the kernel is where the two guests part: arm64
takes Ubuntu's generic kernel apart to the raw Image `VZLinuxBootLoader` wants,
amd64 keeps that same `vmlinuz` verbatim, because it already *is* the bzImage
`LinuxKernelDirect` loads. So do the module sets — virtio on arm64; on amd64
`hv_vmbus` and the drivers on it (`hv_storvsc`, `hv_utils`, `hv_balloon`),
`vsock` + `hv_sock`, and 9p as three modules: `9p`, `9pnet`, and **`9pnet_fd`**,
the trap worth naming, since upstream split `trans_fd` out and a `trans=fd`
mount with `9pnet` loaded and `9pnet_fd` missing fails with a bare `ENODEV`.
There is deliberately no `hv_netvsc`: the guest has no network device to drive.
`vm-image/README.md` records the corrections of §7's prose to real package
names and the open questions (squashfs, distribution, determinism).

`ral-initramfs/` is the initramfs itself, and every decision in it is a typed
plan — assemble the overlay root, make the session disk, install daemon and
engine, `switch_root` — unit-tested on any machine. It hardcodes no disk:
`plan.rs`'s `resolve_disks` probes candidate *pairs* in order, virtio
(`/dev/vda` + `/dev/vdb`) first, then Hyper-V's SCSI (`/dev/sda` + `/dev/sdb`),
and refuses by naming every candidate it looked for.

## What is not here

Three things about the Windows machine, none of them settled by the code
compiling:

- **Delegation.** Neither Hyper-V `Machine` (`hcs/mod.rs`'s `Guest`,
  `broker/client.rs`'s `BrokeredGuest`) implements `connect_guest`, so the
  trait's `Unsupported` default refuses every helper a Windows trunk spawns;
  only macOS delegates.

- **A completed guest boot is not witnessed yet.** What is: a machine created
  and started through the broker, booting a kernel and an initramfs that formats
  the session disk and reaches the daemon — known because the daemon refused a
  command line carrying a `ral.` key its own build predated, and said so on its
  own console. What is not: a guest that finishes booting and dials, since the
  media rebuilt against this host's boot contract has not been booted yet. The
  path is otherwise compiled, clippy- and rustdoc-clean, and unit-tested wherever
  a test can reach without a machine — the document's shape, the VHD footer's
  checksum and geometry, the port→service-GUID mapping, the socket's own
  descriptor, the pipe's, the console ring's line bookkeeping, the release of a
  disk another process holds. `vm-manager/examples/boot-smoke.rs` and
  `synod/examples/boot-run.rs` are the vehicles for the rest.
- **Whether the host's 9p server can read the granted folder unaided is
  untested.** `HcsGrantVmAccess` is called on the four boot files, which a
  worker process really does open as its own virtual account, and deliberately
  *not* on the user's folder. The broker's impersonation check answers a
  different question — may the *caller* read it — so if a guest's mount is
  refused, a session-scoped grant is still the knob.

Also the image pipeline's open questions above. The rest of the design record
runs — end to end on macOS, and everything above the machine on both: the §3
wire carries real runs (`synod/examples/boot-run.rs` witnesses
boot → shared folder → engine → settled report), the §5 spawn jail stands
inside the guest (a fresh uid and a cgroup between the engine and what it
runs), and §6 gives the guest a network of its own — a `tun` whose only peer
is `guest-net`, a user-mode TCP/IP stack in a host process — rather than the
single `fetch-url` verb an earlier draft made the whole egress surface
([[design/egress|egress]],
[[decisions/260727_the-guest-gets-a-network-not-a-verb|the-guest-gets-a-network-not-a-verb]]).
[[decisions/260715_vm-workspaces-cross-by-copy|vm-workspaces-cross-by-copy]]
records where synod's work-in-place workspace deliberately departs from
exarch's cross-by-copy position.

## Where to look

- `dev/docs/VM/SYNOD.md` — the design record; `dev/docs/VM/SYNOD-v1.md` —
  what landed; `dev/docs/VM/EXARCH-VM-v2.md` — the shared VM implementation
  plan; `vm-image/README.md` — the boot-media pipeline and its open
  questions.
- [[map/exarch|exarch]] — the sibling binary and the library synod embeds.
- [[design/exarch-architecture|exarch-architecture]] — the loop both products
  run.
- [[decisions/260725_windows-hyper-v-backend|windows-hyper-v-backend]] — why the
  Windows machine is assembled the way it is.
- [[decisions/260725_windows-machine-broker|windows-machine-broker]] — why a
  `LocalSystem` service creates it, what the Hyper-V Administrators group would
  have cost every user, and what keeps the service's surface narrow.
- [[decisions/260730_boot-contract-is-versioned|boot-contract-is-versioned]] —
  why the kernel command line carries a version, and why the build rather than
  the boot is where a host/guest mismatch is caught.
- [[decisions/260730_guest-console-outlives-stdout|guest-console-outlives-stdout]]
  — why the guest's console is teed to a log and quoted in the failure, and why
  the broker protocol did not have to change to carry it.
- [[decisions/260730_session-disk-outlives-its-machine|session-disk-outlives-its-machine]]
  — why teardown waits out the worker process, and why a starting backend sweeps
  the cache.
- [[map/core/transport|core / transport]] — the framed seam the control plane
  rides, whose stream type is std's owner for a connected socket and not a claim
  about the address family.
- [[map/exarch/agent|exarch / agent]] — the one-exchange wire spawn synod's
  fleet rides: the engine seals its bequest and names it in its enquiry, the
  desk dials the guest daemon's factory while answering, and the `Factory`
  trait synod implements above is the seam it dials through.
- [[design/agents|agents]] — the one-snapshot law: `Shell::bequeath` is
  the one fork both the identity arm's nursery park and the wire arm's
  seal take, so `` exarch-agents `start `` means one thing regardless of
  seat.
- [[design/engine-protocol|engine-protocol]] — why the daemon
  listens and the host dials, and why that direction is what deleted the
  correlation machinery rather than shrinking it.
- [[decisions/260806_exchange-ends-at-fleet-quiescence|exchange-ends-at-fleet-quiescence]]
  — why synod's closing walk waits for the whole fleet, not just the
  trunk, so a helper still writing never races the report.
- [[map/core/io-process|core / io-process]] — the guest spawn jail
  (`jail.rs`) whose unfiltered `socket(AF_VSOCK)` the daemon's peer-CID admission and
  the guest kernel's refusal of a guest-local dial both stand behind, until a
  seccomp address-family filter lands.
