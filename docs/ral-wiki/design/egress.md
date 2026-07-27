# Egress: a verb on a host

**Egress authority is a verb on a host, and it can only be enforced where the
bytes are in the clear.** Not a hostname the guest is or is not allowed to
resolve, not a port the guest is or is not allowed to open — a *method on a
name*, `GET files.pythonhosted.org` rather than `files.pythonhosted.org` —
because the moment enforcement happens anywhere the traffic is still
encrypted, all it can judge is who the guest is *talking to*, never what it is
*saying*. synod's guest network exists to make that judgment somewhere it
can actually be made: a host process the guest cannot see around, terminating
every connection in user mode before deciding whether it goes anywhere at all
([[map/synod|synod]], `dev/docs/VM/SYNOD.md` §6).

The policy the verb is checked against is
[[decisions/260727_the-guest-gets-a-network-not-a-verb|the-guest-gets-a-network-not-a-verb]]'s
`exarch::net_policy::NetPolicy` ([[map/exarch/agent|agent]]): a `read` list
granting `GET`/`HEAD` and a `write` list naming exactly the methods each host
admits, both checked against `exarch::egress::AuditLog`'s ledger, a
`RateLimiter`, and a per-response byte cap. Everything below is how a *host*,
not a *name*, becomes the thing a grant is written against.

## Four gates, four bypasses

Each gate exists because the one before it is not enough on its own — a raw
IP address, a name resolved once and abused twice, a method the policy never
named, a protocol the policy was never asked about.

- **DNS is answered in this process.** An off-list name gets `NXDOMAIN`
  (`AAAA` and everything but `A` gets `NOERROR`/zero-answers instead, so a
  stub resolver's negative cache never poisons the following `A` lookup); an
  on-list name gets an address minted fresh from a `100.64.0.0/10` pool no
  route on either side of the link means anything by. This is gate one, and
  it closes nothing by itself — a guest that already knows an address does
  not need DNS to reach it.
- **TCP is terminated in user mode, and the address is checked *at accept*,
  by host and port.** A `tcp::Socket` reaching `Established` names the
  address the guest dialled; if no name DNS ever minted stands behind it, the
  connection is `abort()`ed before a single byte of it is read. This is the
  gate that actually closes the raw-IP bypass: DNS filtering alone only stops
  a guest that asks nicely, and a guest is code, not an asker. A minted
  address with no name behind it cannot exist except as a guest's own
  fabrication or a stale replay from an unrelated session — `guest-net`'s
  `Names` ledger is the only place an address is ever created, so gate two
  is a lookup, not a heuristic.
- **80 and 443 are redirected to an intercepting proxy.** This is the gate
  that turns a *host* grant into a *verb* grant. TLS is terminated on a
  session-local CA no one outside this process holds the key to, so a
  `GET`/`POST`/whatever line is read in the clear before the policy is
  consulted a second time — the DNS/accept gates are method-blind by
  construction, since a name resolves before any request line exists to
  read. A guest that resolves an allowed name, dials its address, then asks a
  *different* name in `Host:`/SNI is refused here too: the address's minted
  name and the request's own claimed name must agree, or the leaf is never
  minted and the handshake itself fails (`guest_net::ca::LeafResolver`,
  [[map/synod|synod]]).
- **Everything else is refused by absence.** No listener exists on any port
  but 80 and 443, so `smoltcp` answers unprompted with `RST` or ICMP
  port-unreachable — a refusal the guest can act on, not a silent drop this
  process had to remember to write code for.

## The honest residual

Interception narrows what crosses far more than a hostname allowlist ever
could, but it does not close the channel. **An allowed `GET` still carries a
query string** — `GET files.pythonhosted.org/?leak=<granted-folder-contents>`
passes every one of the four gates, because every one of them is checked
against the *shape* of a request, never its content. The same is true of a
named allowlist of request headers (`Accept`, `Range`, `User-Agent`,
`Authorization`, and the rest `guest_net::proxy` relays, chosen for real
package-manager traffic — `apt`'s `Range`, PyPI's `Accept`, the `User-Agent`
some hosts require — with every hop-by-hop header and everything unnamed
dropped): each relayed header is the same class of channel as the query
string, and widens nothing this page has already closed. This is why the
`AuditLog` ([[map/exarch/agent|agent]]) is load-bearing rather than
decorative: every name resolved, every connection accepted or refused, and
every request
— method, full URL, host, allowed or not, status, byte count — lands as one
NDJSON line before the caller ever sees an answer. The four gates are what
make a broad default allowlist *safe enough to ship*; the ledger is what
makes a narrow one *reviewable* when it turns out not to have been.

## Where authority actually narrows

ral's own `net` capability is a flat boolean
([[design/two-enforcers|two-enforcers]] — `net` has no in-process gate, only
an OS-sandbox one, and inside a guest not even that: `docs/SPEC.md` §11.3,
§11.8). It cannot express "reachable, but only for `GET`" — nothing in the
grant vocabulary names an endpoint. The real narrowing this page describes
happens one layer outside the grant entirely, in the host policy `NetPolicy`
enforces: `synod::grant`'s `net: Some(true)` is a correctness bit that keeps
`core/src/sandbox/linux.rs` from stripping the network `bwrap
--unshare-net`-style, not a promise about what the network admits. `grant`
says *whether* a guest may reach the wire at all; egress says *what* it may
say once it does.

## See also

[[map/synod|synod]] — where `guest-net` and `ral-daemon`'s net wire live in
the source tree. [[map/exarch/agent|agent]] — `Egress`, `NetPolicy`, the
audit ledger, and how a fleet's trunk opens all three once.
[[decisions/260727_the-guest-gets-a-network-not-a-verb|the-guest-gets-a-network-not-a-verb]]
— the decision this page's architecture realises, and the alternatives it
declined. [[design/two-enforcers|two-enforcers]] — why `net` stays a boolean
at the grant layer while a host-side policy carries the endpoint vocabulary.
