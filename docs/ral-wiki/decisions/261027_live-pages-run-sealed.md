---
status: active
generated_at_commit: 14cd830e3885ef4fb3f22778a359e825fede95e0
verified_at_commit: 14cd830e3885ef4fb3f22778a359e825fede95e0
verified_at_date: 2026-10-27
anchors: [live::serve, live::SCHEME, is_live_frame, PRELUDE_FILES, POLICY]
---

# Live pages run sealed

**A fenced block tagged `live-html` in the assistant's prose runs as a page
inside synod's transcript, in a subframe that holds no command bridge, reaches
no network and loads only synod's vendored libraries.** The VM runs what the
model executes and the host displays what it says; a live page is the one place
the host runs what the model wrote. Containment is therefore the design, and
the main page is never where a page runs.

The audit that gates this decision has not been run; see Audit status below.

## What stays shut

Three layers each block an inline script in the main page, independently:
DOMPurify drops `<script>` (`SCRUB` in `synod/ui/js/math.js`), a script inserted
by `innerHTML` never runs, and the CSP has `script-src 'self'`. **None is
loosened.** The fence reaches the main page as inert text
(`pre > code.language-live-html`), and the only change to its CSP is a
`frame-src` naming the live origin.

## Facts the design rests on

Verified in the vendored sources (`tauri 2.12.1`, `wry 0.57.0`); a bump of
either reopens them.

- **The bridge lives in the main frame only.** Tauri's IPC init script, which
  embeds the per-launch invoke key, is injected with `for_main_frame_only: true`
  (`tauri/src/manager/webview.rs:161-164`). No plugin synod uses injects into
  all frames.
- **The key is the only barrier.** Every invoke must carry `Tauri-Invoke-Key`;
  a mismatch is rejected (`tauri/src/webview/mod.rs:2009-2011`).
- **A registered custom scheme counts as a *local* origin** (`is_local_url`,
  `tauri/src/webview/mod.rs:1961-1990`), and local origins reach every app
  command with no capability entry. A page served from synod's own scheme that
  ever ran as the main frame would receive the key and full IPC.
- **wry labels IPC messages by the sending frame's URL**
  (`wry/src/wkwebview/class/wry_web_view_delegate.rs:50-53`).
- **The navigation guard sees subframes on macOS only.** WebKit's
  `decidePolicyForNavigationAction` fires for every frame and wry passes only
  the URL (`wry/src/wkwebview/navigation.rs:50`); WebView2 hooks only
  `NavigationStarting`, which is top-level only (`wry/src/webview2/mod.rs:742`).
- **WebView2 serves custom-scheme requests from iframes**: wry registers its
  filter with `COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL`
  (`wry/src/webview2/mod.rs:1013-1016`).
- **The asset resolver falls back to `index.html`** for any missing path
  (`tauri/src/manager/mod.rs:409-415`), so the frame server must never pass the
  app's own HTML to a page.
- **`srcdoc`, `blob:` and `data:` frames inherit the parent's CSP**, so
  `script-src 'self'` blocks their inline scripts, and the guard refuses
  `about:` URLs.
- **The no-egress promise.** `synod/data/network.md` tells the model that
  nothing in the folder can be sent anywhere by any route. A page runs on the
  host, outside netgate ([[design/egress|egress]]), so any network channel from
  it breaks that promise. The web platform has channels CSP does not govern
  (WebRTC ICE, `dns-prefetch` and `preconnect`), so an empirical audit, not the
  CSP, is the evidence.

## The sealed frame

```text
assistant prose ──marked──▶ <pre><code class="language-live-html">SRC</code></pre>
                                   │ (settled chunk only)
                                   ▼
      <iframe sandbox="allow-scripts" src="synod-live://localhost/">     (main page)
                                   │ loads SHELL from window/live.rs
                                   ▼
      live-frame.js:  parent !== self ?  postMessage("ready") : inert
                                   │
      live.js (main page): on "ready" from that iframe ▶ postMessage(SRC)
                                   │
      live-frame.js: document.open(); write(PRELUDE + SRC); close()
                                   │
      live-sizer.js (from PRELUDE): ResizeObserver ▶ postMessage({height})
```

- **Two seals.** The element carries `sandbox="allow-scripts"`, set before
  `src` so the first load is already sealed; the response's own CSP repeats
  `sandbox allow-scripts`, so the document has an opaque origin however it is
  loaded.
- **No network source.** The frame's CSP is `default-src 'none'`; scripts and
  styles inline or from the frame's own origin; images `data:` and `blob:`;
  fonts `data:` and own origin; `base-uri 'none'`; `form-action 'none'`;
  `frame-ancestors` the app origin only. There is no `'unsafe-eval'`: D3's
  `csvParse`, `tsvParse` and `dsvFormat` row conversion uses `new Function` and
  fails, and Vega, Alpine and Babel are unusable for the same reason.
- **Three more headers.** `X-Content-Type-Options: nosniff`;
  `Access-Control-Allow-Origin: *`, since module scripts and CSS fonts are CORS
  fetches from an opaque origin; `X-DNS-Prefetch-Control: off`, since CSP does
  not govern `rel=dns-prefetch`. One function, `sealed`, builds every response,
  so none can omit them.
- **A closed server.** `route` serves `/` (the shell), the four prelude files
  (`js/live-frame.js`, `js/live-sizer.js`, `css/tokens.css`,
  `css/live-frame.css`) and `vendor/`, and only by plain paths: every segment is non-empty, is not `.`
  or `..`, and is ASCII alphanumerics and `._-`, so no encoding survives. A
  resolver answer of type `text/html` is refused, since a missing asset resolves
  to the app's `index.html`. A non-GET gives 405; everything else, a 404 naming
  what is served. `tokens.css` is custom properties and `color-scheme` only, so
  a page inherits the window's colours and type; the app's base rules
  (`html, body { height: 100% }`, the background) live in `css/base.css`, which
  only the app page links. The sizer measures the root's border box, and
  `height: 100%` would pin it to the viewport.
- **Content arrives by message, not by URL.** The shell is a fixed document. A
  framed copy posts `"ready"`; the parent answers that iframe, addressed by its
  `contentWindow` since an opaque origin has no name to target, with the page
  source. The frame accepts exactly one page, removes its listener, and writes a
  prelude (tokens, frame CSS, the sizer) followed by the page. A frame reloaded
  by DOM reattachment asks again, so it refills itself.
- **The frame reports its height.** `live-sizer.js` posts the root's border-box
  height, not `scrollHeight`, which never falls below the viewport and would let
  a frame grow but never shrink. The parent clamps it to 24 to 1600 px.
- **Streaming.** While a fence is open (the live tail) it renders as a quiet
  placeholder box, never as source: to the user no script exists
  (`synod/data/system.md`). It becomes a frame when its chunk settles;
  `growProse` renders each settled chunk exactly once, and the projector never
  detaches surviving nodes, since a detach reloads an iframe.
- **Auto-run.** No click.

| Name | Value |
|---|---|
| Fence | `live-html`, marked class `language-live-html` |
| Scheme | `synod-live` |
| Frame origin | `synod-live://localhost` (macOS), `http://synod-live.localhost` (Windows) |
| App origin | `tauri://localhost` (macOS), `http://tauri.localhost` (Windows) |
| Frame to parent | the string `"ready"`; `{ height: number }` |
| Parent to frame | the page source, a string |

## The inert-shell invariant

**A live page's content only ever enters a subframe.** The scheme is a local
origin, so any document of ours that loaded as the main frame would hold the
invoke key. The shell is fixed and carries no page: it acts only when framed
(`live-frame.js`), so even a top-level load of `synod-live://localhost/`, by
navigation or a dropped link, runs nothing the model wrote.

- **The guard admits the frame in WebKit's spelling.** `is_live_frame` accepts
  `synod-live://localhost` without a port, because WebKit asks the guard about
  subframe loads. WebView2 asks only about top-level loads, so its spelling
  `http://synod-live.localhost` is never admitted and never needs to be.
- **`frame-src` names one spelling per platform.** `tauri.conf.json` names
  `synod-live://localhost`; `tauri.windows.conf.json` overrides only that
  directive (platform configuration merges as JSON Merge Patch) with
  `http://synod-live.localhost`. One list naming both is refused: on macOS
  `http://synod-live.localhost` is a loopback port-80 target a page could
  navigate itself to.
- **The key refuses a forgery.** wry labels a message by its sending frame's
  URL, and that URL is local; only the invoke key, which the frame never
  receives, refuses it. The probe page forges one.

## Rejected alternatives

- **The main page.** Running the model's script there hands it the bridge and
  the key, the only barrier. Allowing it means loosening the CSP, the
  sanitiser and the renderer at once, each for all prose and not for the page
  alone.
- **`srcdoc` or blob frames.** They inherit the parent's CSP, so its
  `script-src 'self'` blocks their inline scripts; making them work means
  loosening the main page's CSP for everything. The guard also refuses `about:`
  URLs.
- **URL-carried content.** A page in a `synod-live:` URL (path, query or
  fragment) turns every such URL into a page: one dropped, dragged or navigated
  to would carry the model's content into the main frame. A fixed shell and a
  message keep the URL free of content.
- **Loosening DOMPurify.** Passing `<script>` through the sanitiser admits the
  model's code to the main frame's DOM, where it runs with the bridge, and
  fixes one of the three layers only. The sanitiser is untouched: the fence is
  inert text to it.

## Audit status

**The empirical egress audit and the smoke test have not been run, on macOS or
on Windows, as of the commit that adds this page.** The implementing plan made
them a merge gate on both platforms; the change is committed ahead of that gate
at the owner's instruction. Until they run, that a page reaches no network
rests on the frame's CSP and headers alone, and nothing has measured the
channels CSP does not govern. When they run, record here the platform, the
WebView version, the probe table and the DNS capture.

| Platform | WebView version | Smoke | Probe table | DNS capture |
|---|---|---|---|---|
| macOS (WKWebView) | not run | not run | not run | not run |
| Windows (WebView2) | not run | not run | not run | not run |

If no Windows machine is available, Windows ships unaudited only with the
owner's word.

**If anything leaks: stop.** Report the probe table and the DNS capture to the
owner; do not patch. These are candidate closures, to be reported with the
leak and not applied; each is a new design decision:

- WebView2: `additionalBrowserArgs` with a dead `--proxy-server` and
  `--webrtc-ip-handling-policy=disable_non_proxied_udp`; or
  `--host-resolver-rules`.
- WKWebView: the window's `proxyUrl` (macOS 14+, Tauri feature `macos-proxy`),
  or the private `WKPreferences` peer-connection switch.

## Reproducing the audit

Build and launch synod (`cargo run -p synod`). Ask the assistant to repeat a
page verbatim inside a `live-html` fence. Each page below is shown exactly as
it must be repeated.

### Egress probe page

Run DNS capture alongside:

- macOS: `sudo tcpdump -l -n -i any port 53 | grep -iE 'probe-|example\.com|stun'`
- Windows: `ipconfig /flushdns` before; `ipconfig /displaydns | findstr /i "probe- example.com stun"` after.

**Pass:**

- Every row reads `sealed`; the two informational rows always do, their detail
  column reporting whether a handler exists.
- No navigation button changes what the frame shows.
- No `probe-…`, `example.com` or `stun` name appears in the DNS capture.
- The forged `open_url` (the `// 1. bridges` block of the page) is rejected:
  on macOS through the WebKit handler, on Windows through `chrome.webview`.
  Synod's stderr (debug build) shows the invoke-key rejection and no browser
  opens. The two informational rows may legitimately show a handler present:
  it is the key, not the handler's absence, that seals the bridge.

The `spin 10 s` button records whether a runaway page freezes synod's composer;
it is recorded, not gating.

```live-html
<style>td{padding:2px 8px;font:12px var(--mono)}.ok{color:var(--accent)}.bad{color:var(--danger)}</style>
<table id="r"></table>
<p>
  <button id="nav">navigate self</button> <button id="meta">meta refresh</button>
  <button id="link">click link</button> <button id="top">navigate top</button>
  <button id="pop">pop-up</button> <button id="form">submit form</button>
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
attempt("worker", () => new Promise((ok, no) => { try { new Worker(URL.createObjectURL(new Blob(["1"]))); ok(); } catch (e) { no(e); } }));
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

## Open

- **A runaway page.** Whether a page that spins freezes the window is
  unknown; the probe page's spin button records it. Pages auto-run.
- **Pages are invisible to the agent.** Nothing a page does comes back: not its
  render, not its script errors. `synod/data/live.md` tells the model so and
  asks for pages simple enough to be right the first time.
- **Follow-ups.** A ```` ```mermaid ```` fence rendered through the same frame,
  since models emit it unprompted and the frame and library exist. A `page` card
  mark, so ral can build a page from computed data rather than typing it into
  prose; it touches exarch's `card::Mark` and the TUI. Reporting a page's
  script errors back to the agent.
- **Non-goals.** Live pages in exarch's TUI, where they stay code; a channel
  from a page to the agent; network access for pages under any setting.
- **The library set.** Chart.js 4.5.1, D3 7.9.0 and Mermaid 12.1.0 join the
  vendored KaTeX and three.js; Mermaid adds 5.5 MB raw (about 1.5 MB
  compressed) to the binary. Tabulator, Observable Plot and htm/preact are not
  vendored.

## Where

- `synod/src/window/live.rs`: `SCHEME`, `route`, `serve`, `sealed`, `POLICY`,
  `PRELUDE_FILES`. `synod/src/main.rs` registers it (`live::serve`) and
  `is_live_frame` extends the navigation guard.
- `synod/tauri.conf.json` and `synod/tauri.windows.conf.json`: the CSP as a
  directive map, and the per-platform `frame-src`.
- `synod/ui/js/live.js` (`mountLiveFrames`, `holdLiveFrames`), `live-frame.js`
  (the shell's script), `live-sizer.js`, `css/live-frame.css`, `css/tokens.css`
  (shared with the prelude), `css/base.css` (the app-only sheet), `ipc.js`
  (`LIVE_FRAME_URL`) and `prose.js`, which mounts frames from settled chunks and
  holds a placeholder for the open tail.
- `synod/data/live.md`, pushed after "Talking to the user" by
  `synod/src/assistant/prompt.rs`; `synod/ui/vendor/` and its README.

## See also

[[map/synod|synod]], [[design/egress|egress]],
[[decisions/260721_synod-is-a-second-product|synod-is-a-second-product]].
