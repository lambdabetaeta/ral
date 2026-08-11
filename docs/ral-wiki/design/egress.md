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
`exarch::net_policy::NetPolicy` ([[map/exarch/agent|agent]]): an exact list
of lowercase ASCII DNS names, `hosts`, plus a `search` bit unrelated to this
proxy. There is no `read`/`write`, no `max-bytes`, no `rate-per-minute` —
those keys are hard errors now, naming their own retirement rather than
being silently reinterpreted.

## The CONNECT door

`10.0.2.2:3128` is the guest's only network application endpoint —
`ral-daemon` sets `HTTPS_PROXY`/`https_proxy` to it for a networked boot,
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
10. Write `200 Connection Established`.
11. Copy bytes in both directions without parsing them.

The guest keeps the ordinary public CA bundle from its image; no session CA
is minted or installed, and TLS runs end-to-end between the guest and the
origin. HTTP/1.1, HTTP/2, gRPC and WebSockets all work inside that opaque
tunnel; HTTP, h2c, HTTP/3, QUIC, arbitrary TCP ports and UDP do not, because
only `HTTPS_PROXY` is set — an `http://` URL dies as a connection reset, not
a message.

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
merely narrow: `exarch::egress::AuditLog` ([[map/exarch/agent|agent]])
records one `Tunnel` per attempt — its final vetted address, and on close
the byte count each direction carried. Telemetry, not policy: it narrows
nothing this page has not already narrowed, and it is bounded with rotation.
A record that cannot be written closes the gate, because an unauditable
proxy does not proxy.

## Where authority actually narrows

ral's own `net` capability is a flat boolean
([[design/two-enforcers|two-enforcers]] — `net` has no in-process gate, only
an OS-sandbox one, and inside a guest not even that: `docs/SPEC.md` §12.5,
§12.11). It cannot express "reachable, but only this host"; nothing in the
grant vocabulary names an endpoint. The real narrowing this page describes
happens one layer outside the grant entirely, in the host policy `NetPolicy`
enforces: `synod::grant`'s `net: Some(true)` is a correctness bit that keeps
`core/src/sandbox/linux.rs` from stripping the network `bwrap
--unshare-net`-style, not a promise about what the network admits. `grant`
says *whether* a guest may reach the wire at all; egress says *which
destinations* it may reach once it does.

## See also

[[map/synod|synod]] — where `guest-net` and `ral-daemon`'s net wire live in
the source tree. [[map/exarch/agent|agent]] — `Egress`, `NetPolicy`, the
audit ledger, and how a fleet's trunk opens all three once.
[[decisions/260727_one-connect-door-not-four-gates|one-connect-door-not-four-gates]]
— the decision this page's mechanism realises, and the alternatives it
declined. [[design/two-enforcers|two-enforcers]] — why `net` stays a boolean
at the grant layer while a host-side policy carries the endpoint vocabulary.
