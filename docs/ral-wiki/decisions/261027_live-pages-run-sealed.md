---
status: active
generated_at_commit: 3b76d0db9bb6a50452bb643af0beb67f8b11c26c
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

The audit that gates this decision, and the seal it led to, are on
[[design/live-page-egress|live-page-egress]].

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
  fetches from an opaque origin; `X-DNS-Prefetch-Control: off`, a belt for
  engines that honour it. WebKit does not: the header is not what closes
  `rel=dns-prefetch`, which CSP does not govern either. The seal's content rule
  list closes it ([[design/live-page-egress|live-page-egress]]). One function,
  `sealed`, builds every response, so none can omit them.
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
  a frame grow but never shrink. The parent clamps it to 24 to 1600 px. The
  sizer measures `<html>`'s border box, so a page that pins the root to the
  viewport (`html { height: 100% }`) closes a feedback loop, F' = f(F): any gap
  between the frame and its viewport, such as a classic horizontal scrollbar, is
  lost on every round, which drifted the frame down to the 24 px minimum in a
  real WKWebView run. The prelude therefore forces
  `html, body { height: auto !important }`, which beats the page's own
  stylesheets and its inline `style.height`. The prelude also hides the frame's
  own scrollbars (`overflow-x: hidden`, `scrollbar-width: none`): the frame is
  sized to its content and the sizer cannot see a scrollbar's thickness, so a
  classic horizontal bar would clip the page's last 15 px and force a vertical
  one. A page wider than the frame is clipped; a page taller than `MAX_PX` still
  scrolls, without a bar. Viewport units (`vh`) can still grow a frame, but only
  up to `MAX_PX`.
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

## Audit

The empirical egress audit, its page (the egress probe), the pass criteria and
the log of runs are on
[[design/live-page-egress|live-page-egress]]. The first macOS run found DNS
lookups and a WebRTC STUN exchange that the frame's CSP and
`X-DNS-Prefetch-Control: off` do not govern; macOS pages now run only once the
webview is sealed at creation (`synod/src/window/seal.rs`), and Windows pages
run unsealed.


## Open

- **The Windows seal.** Windows pages run unsealed: no switch for WebRTC in
  WebView2 is known and no closure for link-hint names is measured, so the
  sealed claim in `synod/data/network.md` and `synod/data/live.md` is false
  there until one is found ([[design/live-page-egress|live-page-egress]]).
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
