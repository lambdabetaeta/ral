---
generated_at_commit: 3b76d0db9bb6a50452bb643af0beb67f8b11c26c
---

# Live pages and the network: the seal

**A live page runs on the host, past the guest's network gate, so the webview's own configuration is the only thing between the page and the network.** On macOS that configuration is sealed; on Windows it is not, and the promise below is false there.

A *live page* is a block fenced as `live-html` in an assistant message. Synod shows it as a page in a sandboxed iframe (a frame embedded in the transcript, with the browser's sandbox restrictions on) and runs it on the host, the user's own machine. [[decisions/261027_live-pages-run-sealed|live-pages-run-sealed]] holds the frame's design: scheme, CSP, inert shell. This page holds what that design cannot guarantee by itself, and the audit that measures it.

The promise is that nothing in the folder can be sent anywhere. `synod/data/network.md` tells the model that "nothing in this folder can be sent anywhere by any route", and `synod/data/live.md` tells it that a page "cannot reach the network"; the model repeats both to the user.

The promise is at risk because the model's commands run in a VM (the guest), whose network egress is a destination policy at one door ([[design/egress|egress]]). A live page does not run there. It runs in the host's *webview*, the browser engine embedded in the app (WKWebView on macOS, WebView2 on Windows), which is past that door.

A hostname is itself a message. DNS turns a name into an address, and a *resolver* is the server that answers the lookup. A lookup of a name the page chose carries the page's bytes to the resolver, with no connection to the named host and no answer needed. Egress is therefore not "a connection was made" but "a datum the page chose left the machine".

On macOS the frame is *sealed* by three settings applied when the webview is created, each closing a class of channel the others leave open; a proof token keeps every page off until all three are in force.

## What a page can make the webview do

**A content-security policy closes what the document fetches; the engine does four further things on the document's behalf, and each needs a switch of its own.** A *content-security policy* (CSP) is a header listing the sources a document may load from, and the browser enforces it. It governs the document's own fetches, not what the engine does unasked.

A page can cause such actions with a single tag. A *link hint* is a `<link>` tag that makes the engine prepare for a request that may never come: `rel=dns-prefetch` resolves a name early, and `rel=preconnect` opens a connection early. `target=_blank` is the link attribute that asks for a new window. *WebRTC* is the browser's peer-to-peer API; before connecting it runs *ICE*, the procedure that finds usable network paths, which includes sending a UDP packet to a *STUN* server on the internet to learn the machine's public address (a *TURN* server relays traffic instead).

| The page can cause the webview to | Does CSP govern it | Closed by |
|---|---|---|
| fetch a resource: script, request, image, font, media file, socket, subframe, worker script | yes | the frame CSP (`default-src 'none'`), repeated by the content rule list |
| resolve a name it never fetches from (`rel=dns-prefetch`) | no | the content rule list |
| open a connection ahead of any fetch (`rel=preconnect`) | no | empty `allowedNetworkHosts`, or the content rule list |
| the same, from a link to a new window that it makes by script (`target=_blank`) | no | empty `allowedNetworkHosts` |
| gather ICE candidates: resolve a STUN or TURN name and send it a packet | no | `peerConnectionEnabled` off |

The last column names the three settings of the next section.

- `X-DNS-Prefetch-Control: off` is not a closure in WebKit. The frame sends it on every response, and the hints are honoured regardless.
- A blocked pop-up is not a blocked connection. The `target=_blank` link opens no window in the sealed frame and still makes the engine connect to the name the page chose, whatever the frame's CSP, the content rules and the navigation guard say. The probe's `target blank` button tests it.
- The closure column is measured, not derived. It is read off the table under "Why all three" below, and an engine update can move a channel from one row to another.

## The seal (macOS)

**The seal is three settings on the webview's configuration, applied before the webview exists; none is sufficient alone.** Each single setting measured leaks, so does the one pair measured, and only the three together leak nothing.

Two of the three are private API: settings Apple does not document, set by name through key-value coding (the Objective-C mechanism that reads and writes a property by its string name). The third is public API: a content rule list, WebKit's compiled list of URL patterns that the engine refuses to load.

| Setting | Kind | Closes |
|---|---|---|
| `allowedNetworkHosts` = the empty set | private property of the webview's configuration (`WKWebViewConfiguration`), set by key-value coding; must be set before the webview is created | connection warm-up: the `preconnect` hint and the script-made `target=_blank` link |
| `peerConnectionEnabled` = false | private property of the webview's preferences (`WKPreferences`), set by key-value coding | WebRTC: `RTCPeerConnection` is undefined, so there is no ICE gathering, no STUN or TURN lookup and no packet |
| a `WKContentRuleList` blocking `^https?:` and `^wss?:` | public API, added to the user content controller | name resolution from `dns-prefetch`; repeats the CSP for every load |

- One hook, at creation. `synod/src/window/seal.rs` (macOS only) builds the configuration holding all three and hands it to Tauri's `WebviewWindowBuilder::with_webview_configuration`. A configuration can be given only when the webview is built, so the main window is created in code, in `setup` (`"create": false` in `tauri.conf.json`).
- The seal is per webview. The main page is sealed along with its frame; it loads nothing from the network (`synod/ui/vendor/README.md`), so it loses nothing.
- The development origin. The rule list blocks every `http(s)` load in the main webview, so a `devUrl` in `tauri.conf.json` (the address of a development server, used in debug builds) would lose its resource loads on macOS. Synod configures none, so this is moot today. `is_dev_server` in `main.rs` is the navigation guard's separate, debug-only allowance for such an origin.
- Left out. WebKit's Lockdown Mode (a system hardening mode) and a dead proxy (a proxy setting that points at nothing) were measured and are not part of the seal: alone, each leaves channels open. Neither was measured in combination.

### Why all three

**No single setting closes every channel; the harness measurements show which setting closes which.** The harness is a bare `WKWebView` that copies synod's frame (shell, scripts, headers, CSP, sandbox, navigation guard). Local listeners stand in for the network (HTTP, TCP, a logging `CONNECT` proxy, a STUN responder, a QUIC sink), and the `mDNSResponder` unified log (macOS's resolver daemon) shows lookups. Each run applies one configuration. The harness is not in this repository.

Platform: macOS 27.0.1, WebKit 22625.1.29.11.28. "Leaks" means observed at a listener or as a name in DNS.

| Settings applied | Leaks |
|---|---|
| none | DNS for `dns-prefetch`, `preconnect`, STUN/TURN names; TCP connections (`preconnect`); STUN packets answered |
| content rule list only | STUN (and its DNS); a `target=_blank` anchor still makes TLS connections |
| `allowedNetworkHosts` empty only | DNS for `dns-prefetch` and STUN/TURN names; STUN packets |
| WebRTC off only | TCP (`preconnect`) and DNS for `dns-prefetch`, `preconnect` |
| `allowedNetworkHosts` empty + WebRTC off | DNS for `dns-prefetch` only |
| all three (the seal) | nothing: no connection, no DNS, `RTCPeerConnection` undefined |
| Lockdown Mode only (not in the seal) | TCP, DNS (`dns-prefetch`, `preconnect`) |
| a dead proxy only (not in the seal) | STUN, TCP |

Every single setting measured leaks. The one pair measured leaks. Only all three leak nothing.

The `target=_blank` channel needs its own closure. A script-made `target=_blank` link makes WebKit connect (DNS, TCP, TLS) to a name the page chose, regardless of the frame's CSP, the content rules and the navigation guard. Empty `allowedNetworkHosts` closes it; the content rule list does not.

## Fail closed

**A live page runs only once the seal is proven, and the proof is a value that exists only when the seal does.** `Sealed` is a zero-sized token: a value with no contents, so its existence is the whole message. `seal.rs` builds it only after all three settings are applied and the two private ones are read back from the configuration, and only then registers it as Tauri state. The `synod-live` handler in `live.rs` serves the frame shell only if that state exists. Otherwise it answers 503 (service unavailable), the frame never posts `ready`, and no page source is sent.

- A token, not a flag. A flag is a claim by whoever sets it: it can be set before the evidence exists, kept after the evidence changes, and its unset default is a choice someone must get right. The token is the evidence. It is built in one place, after the read-backs, and its absence is the failure state, which needs no code to be correct. The read-back makes "applied" mean that the configuration now holds the value, not that a call returned.
- Why it matters here. Two of the three settings are private API. A later macOS may rename or remove one; the setter then fails or does nothing, and an unsealed webview would run pages exactly as before. The read-back catches that, and the token makes the failure loud and total: a message on stderr and live pages off, never pages running unsealed.
- What the token does not prove. It proves that the three settings are in force, not that three settings suffice. Sufficiency is the audit's job, and is re-measured when the engine changes.

## Windows

**Live pages are enabled on Windows and the frame is not sealed, so the sentences in `synod/data/network.md` and `synod/data/live.md` that call a page sealed are false there.** On Windows `may_serve_shell` in `live.rs` returns true unconditionally: there is no token to wait on.

- What carries over. The frame's design is the same: opaque origin, CSP, closed server, inert shell, no bridge. None of it has been measured on Windows.
- Open channels. DNS names through link hints; WebRTC and its STUN lookups; the script-made `target=_blank` connection; and anything peculiar to one engine. No Windows run exists to measure them, and the macOS table measures WebKit only.
- No known WebRTC switch. No documented way to turn WebRTC off in WebView2 was found.
- Candidate closures, untried. Each is a new design decision, not applied:
  - `additionalBrowserArgs` (command-line flags for WebView2's Chromium engine) with `--host-resolver-rules="MAP * ~NOTFOUND"`, which makes every name fail to resolve.
  - `additionalBrowserArgs` with a dead `--proxy-server`.
  - Chromium's `--force-webrtc-ip-handling-policy`: in Chromium only `content_shell` and headless read it, and no effect on WebView2 is verified.

## What the model may be told

**A promise to the model is as strong as the last passing audit on that platform, and no stronger.**

| Platform | What the text may say |
|---|---|
| macOS, seal in force | that a page reaches no network, while the latest macOS run in Results passes |
| macOS, seal not in force | nothing about pages: none runs |
| Windows | no more than that the frame's CSP names no network source; whether names or packets leave is open (Windows, above) |

The two data files carry the sealed claim without a platform split, so as written they overstate Windows. This page does not reword them.

## How the seal is tested

**The seal is tested by running a probe page in the sealed frame while packet captures watch the wire, on a procedure that makes a silent capture mean something.**

### Two vantage points

**The probe page reports from inside the page; packet captures watch the wire from outside; neither alone can show a leak.** The page cannot see a lookup that the engine makes on its behalf, because no event reaches the page. A capture sees every packet but cannot say which channel caused it. Together they say that something left and what the page did to make it.

### What the probe tries

**The probe page tries each channel and prints one row per attempt.** A row reads `sealed` if the attempt was blocked and `LEAK` if it got through. Two rows, labelled informational, report whether a bridge handler exists and always read `sealed`; so does the `csp violations seen` row, which lists the CSP directives that fired. The probe reports a worker as `sealed` iff it never runs, because a blocked Worker script does not throw; WebKit queues an error event.

Its blocks try:

- The bridge objects `__TAURI_INTERNALS__` and `__TAURI__`, which must be undefined, and a forged `open_url` message. Tauri's bridge is how the main page asks the host to act, and it accepts a message only with a secret per-launch key, `__TAURI_INVOKE_KEY__`. The forged message omits the key, so it must be dropped and no browser may open.
- The fetch family: `fetch`, `XMLHttpRequest`, `WebSocket`, `EventSource`, `Worker` and `sendBeacon`, all aimed at `https://example.com/`.
- Subresources: an image, a CSS background image, a font, a subframe and an audio file.
- Link hints: `dns-prefetch`, `preconnect` and `prefetch` links to names of the form `<rel>-probe-<tag>.example.com`. The tag is random per render and printed as `DNS watch: probe-…`, so the DNS capture can pick the names out.
- WebRTC: an `RTCPeerConnection` aimed at a public STUN server. It reads `sealed` iff no reflexive candidate (the public address a STUN server reports back) arrives within 5 s, or the API is absent.
- Eight buttons. Seven attempt navigations and pop-ups: navigate self, meta refresh, click link, navigate top, pop-up, submit form, target blank. The eighth, `spin 10 s`, busy-loops for ten seconds.

### Setup rules

**These rules keep a capture from reading clean for the wrong reason, or dirty for the wrong reason.**

- Flush the DNS cache first. A name the system already holds sends no packet.
- Run a positive control: look up a made-up name such as `probe-control.example.com` and confirm that it appears in the DNS capture. If it does not, the capture is blind and the run does not count.
- Run no other lookups on the machine during the capture. A stray `dig` looks like a leak.
- Launch synod from a terminal if its stderr is wanted: a seal failure is reported there.

### Captures

**One capture watches names and one watches connections, so a lookup and a packet are each seen on their own.**

On macOS, flush the cache, then capture names:

```sh
sudo dscacheutil -flushcache; sudo killall -HUP mDNSResponder
sudo tcpdump -k NP -l -n -i any port 53 | grep -iE 'probe-|example\.com|stun'
```

Then capture connections, with a `host` filter on the four addresses that `dig +short example.com A` and `dig +short example.com AAAA` return (two IPv4, two IPv6):

```sh
sudo tcpdump -k NP -l -n -i any 'host A or host B or host C or host D'
```

This catches IPv4 and IPv6, TCP and UDP (QUIC, the UDP transport under HTTP/3). The older filter `tcp[tcpflags] & tcp-syn != 0 and port 443` misses IPv6 and QUIC. `-k NP` prints the process name and id; `-n` stops tcpdump doing lookups of its own.

On Windows, `ipconfig /flushdns` before the run and `ipconfig /displaydns | findstr /i "probe- example.com stun"` after it read the system's DNS cache in place of the wire.

### Procedure

**The run is seven steps, in this order.**

1. Build and launch synod.
2. Flush the DNS cache.
3. Start both captures.
4. Run the control lookup and confirm that it appears.
5. Ask the assistant to repeat the probe page verbatim in a `live-html` fence.
6. Click the eight buttons once each, in order, checking after each that the frame still shows the table.
7. Wait about ten seconds, then stop the captures.

### Pass criteria

**On macOS with the seal in force, a run passes when nothing the page tried got out.**

- Every row reads `sealed`. The informational rows always do; the `webrtc` detail is `absent`; `worker` reads `sealed` because the worker never ran.
- No button changes what the frame shows. `target blank` makes no connection and no lookup of `example.com`, and its pop-up stays blocked.
- No `probe-`, `example.com` or `stun` name appears in the DNS capture, and no packet to or from `example.com`'s addresses appears in the connection capture.
- The forged `open_url` is dropped and no browser opens. The message fails to parse, so Tauri writes a `console.error` into the main webview and nothing reaches stderr. The informational rows may show a handler present: the handler exists, and it drops any message that lacks the secret key.
- `spin 10 s` records whether a runaway page freezes synod. It is recorded, not gating.

On Windows the same criteria are the target. A run is expected to fail until a closure exists (Windows, above); record exactly what leaks.

### If anything leaks: stop

**A leak ends the run and goes to a person.** Report the probe table and the captures. Do not patch: a closure is a design decision.

### When to re-run

**The audit is the evidence and the settings are the hypothesis it tests, so re-run it whenever the hypothesis could have changed.** That is: when WebKit changes (a macOS update); when Tauri or wry (Tauri's webview library) is bumped; after any edit to `seal.rs`, to the frame's headers or CSP in `live.rs`, or to the navigation guard; and before shipping any Windows closure. A new setting is measured on the harness first.

## The probe page

**The probe page is the exact text the assistant must repeat inside a `live-html` fence; the fence is the executable audit.** Run it as described in "How the seal is tested" above.

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

## Results

**Each run records the platform, the WebView version, the probe table and the captures.**

| Run | Platform, WebView | Probe | Captures |
|---|---|---|---|
| Before the seal | macOS, WKWebView; version not recorded | `webrtc` read `LEAK` (a STUN server answered) | four lookups reached the resolver: the `dns-prefetch` hint name, the `preconnect` hint name (twice) and `stun.l.google.com` |
| Sealed app | macOS 27.0.1, WebKit 22625.1.29.11.28 | pass: every row `sealed` (14 at load; `top` and `pop-up` after the clicks); the frame unchanged after all eight buttons; `spin 10 s` freezes for its 10 s | DNS: control seen, nothing from the page; connections: host capture on `example.com`'s IPv4 and IPv6 addresses, nothing; forged `open_url`: no browser opened |
| Windows | WebView2 | not run | not run |

## Where

- `synod/src/window/seal.rs` (macOS only): the three settings and `Sealed`. `synod/src/window/live.rs` serves the shell only once `Sealed` exists. The main window is built in `setup`. [[map/synod|synod]] locates the code.
- `synod/data/network.md` and `synod/data/live.md`: the promises this page audits.

## See also

[[decisions/261027_live-pages-run-sealed|live-pages-run-sealed]],
[[design/egress|egress]], [[map/synod|synod]],
[[design/two-enforcers|two-enforcers]] for the same split between a policy and
the layer that enforces what it cannot see.
