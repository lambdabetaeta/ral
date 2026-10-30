---
generated_at_commit: 64ecd691
---

# Egress: a destination gate

**The guest may open TCP connections to port 443 of the public addresses
these exact hostnames resolve to, once, on the host — and to no other
network destination.** That is the whole claim, and it is a destination
policy, not an information-flow one: an allowed host receives whatever the
guest sends it through its own TLS connection. This page must never call
that read-only or say it prevents exfiltration. synod's guest network exists
to make even that narrow judgment somewhere it can actually be made: one
explicit proxy the guest cannot see around, terminating the `CONNECT`
handshake in user mode before deciding whether a tunnel opens at all
([[map/synod|synod]], `dev/docs/VM/SYNOD.md` §6).

The policy the door is checked against is
[[decisions/260727_one-connect-door-not-four-gates|one-connect-door-not-four-gates]]'s
`ral_core::egress::NetPolicy` ([[map/exarch/agent|agent]]): an exact list
of lowercase ASCII DNS names, `hosts`, plus a `search` bit unrelated to this
proxy. There is no `read`/`write`, no `max-bytes`, no `rate-per-minute` —
those keys are hard errors now, naming their own retirement rather than
being silently reinterpreted.

## The CONNECT door

`10.0.2.2:3128` is the guest's only network application endpoint —
`ral-guest` sets `HTTPS_PROXY`/`https_proxy` to it for a networked boot,
plus `NO_PROXY`/`no_proxy` so a tool testing its own server is not routed
here and refused. `smoltcp` owns this one listening socket and no others: it
does not answer DNS, mint addresses, or listen on 80 or 443, so a direct
guest connection has no listener and goes nowhere. For each accepted
connection, in order:

1. Read at most 8 KiB of request head before a short handshake deadline.
2. Require the method to be exactly `CONNECT`.
3. Reject any `@` in the raw request target before it is parsed —
   `http::uri::Authority` parses userinfo successfully, so
   `a.example@169.254.169.254:443` would otherwise decode cleanly with the
   IP as its host.
4. Parse the target as an `http::uri::Authority`, rejecting IP literals and
   every port but 443.
5. Require one agreeing `Host` header, compared lowercased.
6. Look the validated name up in the exact `hosts` allowlist.
7. Resolve that name once, on the host.
8. Discard every address that is not public — unspecified, loopback,
   private-use, link-local, carrier-grade NAT, documentation, benchmarking,
   multicast, reserved, and their IPv4-mapped forms all fail closed.
9. Dial the vetted `SocketAddr`s in order and pin them: the name is never
   resolved again, so a client that resolves it a second time cannot reopen
   DNS rebinding or host-side SSRF.
10. Record admission in the audit ledger before writing `200 Connection Established`.
11. Copy bytes in both directions without parsing them, then record the close.

The public-address tables are explicit: several IPv4 range predicates and
`Ipv6Addr::is_global` are unstable, and a toolchain update must not silently
move this boundary. IPv6 admits only `2000::/3` after the listed carve-outs;
IPv4-mapped and IPv4-compatible forms pass through the IPv4 classifier.

The guest keeps the ordinary public CA bundle from its image; no session CA
is minted or installed, and TLS runs end-to-end between the guest and the
origin. HTTP/1.1, HTTP/2, gRPC and WebSockets all work inside that opaque
tunnel; HTTP, h2c, HTTP/3, QUIC, arbitrary TCP ports and UDP do not, because
only `HTTPS_PROXY` is set — an `http://` URL dies as a connection reset, not
a message.

## Tunnel lifetime

**Clean EOF ends one direction, not the tunnel.** Guest-facing FIN delivery
and whole-worker retirement are separate facts:

- Guest FIN drains queued upload bytes, then shuts down only the origin's
  write half; the response continues. Origin EOF drains queued output before
  guest-facing FIN; a still-open guest upload continues.
- The 30-second retirement grace starts only after the whole worker has
  finished and its output has drained, not when one direction ends. Only a
  completed worker can be reaped: immediately if guest TCP is closed,
  otherwise once that grace is exceeded.
- A full guest TCP close/reset, session stop, or real copier I/O failure
  wakes both pipe rings and shuts down the origin both ways. The core keeps
  polling other connections and joins a worker only after it has completed.
- There is no idle or active-tunnel lifetime. A valid remaining half-stream
  or a guest withholding reads can still occupy one of the 32 tunnel slots.
  Session joining remains subject to system resolver and per-address dial
  timeouts; the grace does not bound those waits or ledger operations.

## The honest residual

Three limits this page keeps stated, not implied:

- **This is a destination policy, not an information-flow policy.** An
  allowed host can receive whatever the guest sends through its TLS
  connection. Nothing here reads, filters, or bounds that content.
- **TLS is what well-behaved tools do inside the tunnel, not what the gate
  enforces.** The proxy never parses the tunnelled bytes, so a guest may
  speak anything at all to an allowed origin's port 443.
- **An exact-name allowlist is really an allowlist of the infrastructure
  behind those names.** Most allowed hosts sit on shared CDN edges, and a
  tunnel to `pypi.org`'s resolved address can carry a TLS hello naming any
  other site the same edge serves. Seeing that name would take exactly the
  interception this design does not do.

The audit ledger is what makes a narrow `hosts` list reviewable rather than
merely narrow: `netgate::AuditLog` (`netgate/src/ledger.rs`)
records a `Tunnel` verdict for each attempt and, for an admitted tunnel, a
second close record carrying the final vetted address and byte count each
direction carried. Telemetry, not policy: it narrows
nothing this page has not already narrowed, and it is bounded with rotation.
A record that cannot be written closes the gate, because an unauditable
proxy does not proxy.

The gate owns the ledger, its only writer.
Strict mutex poison stops an untrustworthy ledger;
an interprocess file lock covers size checking, rotation and append.
Exarch loads the policy and carries only `LaunchConfig.search`, opening no ledger
in host mode. The JSON record shape and rotation bound are the gate's.

## Where authority actually narrows

ral's own `net` capability is a veto
([[design/two-enforcers|two-enforcers]]): it has no in-process guard, since
ral dispatches no network operation. In the guest, `net: offline` gives each
command a fresh network namespace with loopback up (`docs/SPEC.md` §12.5,
§12.11). IP egress and abstract Unix sockets outside that namespace are out
of reach; pathname Unix sockets remain reachable through the shared
filesystem and can relay for an offline command. It cannot express
"reachable, but only this host"; nothing in the grant vocabulary names an
endpoint. The real narrowing this page describes happens one layer outside
the grant, in the host policy `NetPolicy` enforces: `synod/src/folder/grant.rs` leaves
`net` unvetoed to preserve the guest's wire, a correctness choice rather
than a promise about what the network admits. `grant` says *whether* a
command may reach the wire; egress says *which destinations* it may reach
once it does.

## See also

[[map/synod|synod]] — where `netgate` and `ral-guest`'s net wire live in
the source tree. [[map/exarch/agent|agent]] — the policy loader and the
search bit every fleet inherits.
[[decisions/260727_one-connect-door-not-four-gates|one-connect-door-not-four-gates]]
— the decision this page's mechanism realises, and the alternatives it
declined. [[design/two-enforcers|two-enforcers]] — why `net` stays a boolean
at the grant layer while a host-side policy carries the endpoint vocabulary.
