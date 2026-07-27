---
status: active
---

# One CONNECT door replaces four gates

**Egress narrows to a destination policy: the guest may open TCP
connections to port 443 of the public addresses these exact hostnames
resolve to, once, on the host — and to no other network destination.**
[[decisions/260727_the-guest-gets-a-network-not-a-verb|the-guest-gets-a-network-not-a-verb]]'s
four gates — DNS answered locally, TCP accepted only for a minted address,
80/443 intercepted on a session CA so a host grant reads as a verb grant,
everything else refused by absence — collapse to one explicit proxy at
`10.0.2.2:3128` that only ever answers `CONNECT`: read at most 8 KiB of
request head, require the method, refuse `@` in the raw target before it is
parsed, parse the target as an `http::uri::Authority` rejecting IP literals
and every port but 443, require an agreeing `Host` header, look the
lowercased name up in `exarch::net_policy::NetPolicy`'s exact `hosts` list,
resolve it once on the host, discard every non-public address, dial the
vetted `SocketAddr`s in order and pin them, then copy bytes both ways
without parsing them. This is a destination policy, not an information-flow
one — an allowed host receives whatever the guest sends it — and it must
never be described as read-only or as preventing exfiltration.

## Rejected alternatives

- **Keep the intercepting proxy.** Terminating TLS on a session-local CA let
  the old design check a *method* (`GET`/`POST`) against a *name*, narrower
  in principle than a destination-only gate. But that narrowing is
  structurally undermined already: most allowed hosts sit on shared CDN
  edges, so a tunnel vetted for one name can carry a TLS hello naming any
  other site the same edge serves — seeing that name is exactly the
  interception this decision removes. What interception cost to buy a
  narrowing the CDN residual already leaked was real: a session CA minted
  and trusted inside every guest, and `rcgen`, `rustls`,
  `rustls-platform-verifier` and `reqwest` on the same host process an
  adversarial guest reaches first. A parser and a resolver are a smaller,
  more auditable trust boundary than a certificate authority and a decrypting
  HTTP client. Per-method grants, the response-size and rate ceilings, and
  the once-per-name blocked card that interception made possible do not
  survive this decision — retiring them is a decision, not an omission: the
  audit ledger's `Tunnel` record and an optional pre-tunnel HTTP error are
  what remain in their place.
- **Normalise unusual hostname forms (wildcards, IDNA, a trailing dot)
  instead of rejecting them.** Rejecting keeps the policy enumerable — every
  admitted name is exactly one of the strings an operator wrote. Normalising
  wildcard suffixes or Unicode labels is a real feature with its own
  decision to make about what it authorises; nothing in this design needs it
  yet.
- **A private-network escape hatch in the address classifier.** The
  classifier fails closed on every private, link-local, carrier-grade-NAT,
  and documentation range without exception. An operator who needs a guest
  to reach an internal host does not get there by weakening `hosts`; that is
  a new policy form naming an explicit address range, not a carve-out in
  this one.

## Consequences

- **`read`/`write` method rules, `max-bytes`, and `rate-per-minute` are hard
  errors.** `NetPolicy` now carries only `hosts: [String]` and `search:
  bool`. An installed fleet policy using the old keys fails to load with a
  message naming the replacement, rather than being silently reinterpreted.
- **The audit vocabulary reduces to one `Tunnel` record.** The attempt, its
  final vetted address, and — on close — the byte count each direction
  carried. This is telemetry, not policy: nothing about a tunnel's content
  is or was ever recorded. A ledger write that cannot be made closes the
  gate — an unauditable proxy does not proxy.
- **The fixed live-tunnel cap is an implementation limit, not policy.** It
  protects the host process from unbounded worker population; it expresses
  no claim about what the guest may reach.
- **`git clone` and every other write-shaped use of an allowed host are no
  longer a separate grant.** There is no `write` list left to withhold a
  `POST` from; a host on `hosts` admits its full HTTPS surface, because the
  proxy cannot tell a `GET` from a `POST` without becoming the interceptor
  this decision removes.
- **No session CA is minted or installed.** The guest keeps the ordinary
  public CA bundle from its image, and TLS is end-to-end between the guest
  and the origin.
- **`search` stays a separate policy bit.** Provider-side search does not
  route through this proxy at all, so it is untouched by everything above.

## Open questions

- **Wildcard and IDNA hostnames.** Left unaddressed on purpose; a real
  deployment that needs either gets its own decision rather than a
  normalisation quietly folded into this one.
- **CDN-edge co-tenancy.** Named in the "Keep the intercepting proxy"
  rejection above, not solved: an exact-name allowlist really allowlists the
  infrastructure behind those names, and nothing in this design distinguishes
  a tunnel's declared name from the other names the same edge could serve
  once the handshake is opaque.

## Standing beside this decision

[[decisions/260727_the-guest-gets-a-network-not-a-verb|the-guest-gets-a-network-not-a-verb]]
is not reversed by this page, only its four-gate architecture: the guest
still reaches the network itself rather than a bespoke per-protocol verb,
`fetch-url` stays retired, and host-mode exarch still has no web door of its
own.
