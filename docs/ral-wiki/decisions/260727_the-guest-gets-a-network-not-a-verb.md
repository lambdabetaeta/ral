---
status: active
---

# The guest gets a network, not a verb

**A userland as rich as `synod`'s §7 is half wasted if nothing in it can
reach anything, so the guest gets a real interface — a `tun` whose only peer
is a user-mode TCP/IP stack in a host process we wrote — and the single
`fetch-url` verb that used to be the entire egress surface is retired
outright, not deprecated beside it.** Every protocol a package manager, a
build tool, or a document workflow actually speaks becomes reachable the way
it is reached everywhere else — DNS, TCP, TLS — rather than a bespoke host
enquiry per tool (apt, then pip, then npm, then git, then crates.io, each a
smaller worse version of the network it stands in for). What makes this safe
to ship broadly is [[design/egress|egress]]'s four gates, terminating every
connection host-side before deciding whether it goes anywhere: DNS answered
locally, TCP accepted only for an address DNS itself minted, 80/443
intercepted so a host grant reads as a verb grant, everything else refused by
the absence of a listener.

## Rejected alternatives

- **No network device, one verb per protocol.** The design this decision
  replaces. Coherent on paper and does not scale: every new protocol a real
  toolchain needs (`git`, `pip`, `npm`, `apt`, `crates.io`, …) becomes a new
  host-side enquiry, hand-written and re-reviewed, forever behind whatever
  the guest's own userland can already speak natively.
- **The macOS-only hypervisor frame tap.**
  `VZFileHandleNetworkDeviceAttachment` delivers Ethernet frames over a
  datagram socket for free, no entitlement, no guest cooperation — but
  Windows has no equivalent this cheap, and a design that runs one way on one
  platform and a structurally different way on the other is two designs
  wearing one name. Rejected for the same reason `synod` is one guest with
  two backends everywhere else: the guest must not be able to tell which
  hypervisor booted it.
- **The HDV device model on Windows** (`HdvInitializeDeviceHost`, a
  `FlexibleIov` PCI device slot the guest enumerates as a real NIC) —
  **declined on the `HdvCreateGuestMemoryAperture` seam, not on
  availability.** The device model itself is free (OpenVMM's MIT `virtio`/
  `virtio_net`), and at least one third party has already driven it from
  HDV — the door is open. What is ours to write is the seam: a device model
  earns its keep by following physical addresses the guest authored into a
  shared mapping, so implementing guest memory over
  `HdvCreateGuestMemoryAperture` is the most adversary-facing code the design
  could contain, the platform's own aperture semantics are reported
  incoherent with no invalidation notice to subscribe to, and a byte-stream
  parser in the pump's host side is a different, smaller class of exposure
  that no amount of good dependency code changes.
- **`CONNECT` without interception.** Answer the tunnel request, dial the
  named host, never look inside. Loses precisely because `synod` ships to
  arbitrary machines and makes a security claim on all of them: there is no
  IT department downstream curating a tight per-site allowlist, so what ships
  must be broad enough to be useful on day one — and a broad allowlist
  checked only at hostname depth is barely a boundary, since every allowed
  host that accepts writes is a full-bandwidth way out (allowing
  `github.com` would allow pushing the granted folder to it). Interception is
  what turns a broad default into a *verb* grant instead of a *host* grant.
- **Per-domain consent cards.** Worse than nothing for the non-technical user
  `synod` actually ships to: a person is trained to click "always allow" and
  is left with a control that only looks like one. A blocked request instead
  fails once, in plain English, naming the host.

## Consequences

- **Host-mode exarch loses its web door.** `fetch-url` was reachable from a
  bare exarch session with no guest underneath it; the four gates described
  in [[design/egress|egress]] exist only inside `guest-net`, so a host-mode
  session that wants the network now needs a guest to run it through. Search
  is unaffected — `search` stays a policy bit on `NetPolicy`, clamping the
  provider's own hosted web search, which never touched `fetch-url`'s
  machinery to begin with.
- **Caps were resized, not just renamed.** `fetch-url`'s defaults were sized
  for one enquiry at a time; a guest with a real network runs a real install,
  many requests in a burst, so `max-bytes` and `rate-per-minute` both moved
  up rather than surviving as the old verb's numbers under a new name.
- **The ledger records the method.** `exarch::egress::AuditLog`'s `Record`
  grew a discriminant per gate (`Name`, `Connect`, `Request`) instead of one
  shape for every access, because the three gates check different things at
  different moments and collapsing them into one record shape was how the
  earlier design's DNS/accept split got lost.
- **`git clone` is off the shipped list.** The default `read` allowlist
  grants `GET`/`HEAD` only, and cloning over HTTPS needs a `POST` to
  `git-upload-pack` even for a read-only fetch — so it is refused by the
  shipped default's empty `write` list until a fleet's own policy names it,
  the same way every other write-shaped protocol is.
- **The jargon guard moved.** The plain-English check that used to guard
  `fetch-url`'s refusal text (`fleet::desk`'s `FETCH_URL_JARGON`) is retired
  with the verb it guarded and rebuilt beside `guest_net::refusal::Refusal`
  as `REFUSAL_JARGON`, checked against every refusal variant exhaustively
  rather than against strings a socket had to be opened to produce.

## Open questions

- **The jail-vs-install collision.** §5's fresh-UID spawn jail means a
  model-spawned `apt` fails on privilege inside the guest, so `pip3 --user`
  is the one install path the network's allowlist actually admits — `curl`
  rides beside it for whatever `pip3` cannot reach directly. Whether a
  networked guest ever wants a real package-manager install path, and what
  that would need from the jail, is recorded and not resolved.
- **The rate cap is a guess.** `rate-per-minute: 240` is sized off one `pip
  install`, not a measurement. A number to move once real traffic is
  observed, not a boundary anyone has tested against.
- **Path depth.** `AuditLog`'s `Request` record carries a URL whole, query
  string included — the honest half of [[design/egress|egress]]'s residual.
  Whether a review surface built on the ledger should ever truncate a
  logged path for readability, and how much of one that could do before the
  audit stops being trustworthy evidence of what actually crossed, is open.
- **Request headers relay by a named allowlist.** `guest_net::proxy` first
  shipped keeping only `Host` and `Content-Length` from the guest's
  request, which broke real traffic silently — no `Range` for `apt` to
  resume a partial download, no `Accept` for PyPI's JSON API, no
  `User-Agent` for hosts that 403 a client with none. Resolved: a named
  allowlist (`Accept`, `Accept-Encoding`, `Accept-Language`, `Range`,
  the `If-*` conditionals, `User-Agent`, `Authorization`, `Content-Type`)
  is relayed upstream, matched case-insensitively; every hop-by-hop header
  and everything else unnamed is dropped. The same [[design/egress|egress]]
  residual that already covers the query string covers this list too — see
  its "honest residual" section.
- **The upstream client has no idle-read timeout to bound a stall.**
  `reqwest`'s **blocking** client exposes `timeout` (a total-request cap,
  incompatible with a legitimately slow multi-hundred-megabyte wheel) and
  `connect_timeout`, but no `read_timeout` — there is no knob for "abandon a
  connection that stopped sending bytes but never closed." What bounds a
  stalled transfer today is the connect timeout plus `NetPolicy`'s
  `max-bytes` cap, which stops a slow-but-finite response but not a peer
  that opens a connection and then goes silent forever without closing it.
  Whether that gap is worth a hand-rolled read-progress watchdog is not yet
  decided.

## Standing beside this decision

[[decisions/260706_enquiry-channel|enquiry-channel]] is not superseded by
this page — it still carries the host seam's agent, schedule, and reply
families, none of which this decision touches. Losing one enquiry class
(`fetch-url`) is not a supersession of the channel that carried it.
