// Inert unless framed: a top-level copy would hold Tauri's bridge.
if (globalThis.parent !== globalThis.self) {
  // Scoped: document.open() keeps the realm, so a global would clash with the page's own.
  const PRELUDE = '<!doctype html><link rel="stylesheet" href="/css/tokens.css">'
    + '<link rel="stylesheet" href="/css/live-frame.css"><script src="/js/live-sizer.js"></script>';
  addEventListener("message", function take(e) {
    if (e.source !== globalThis.parent || typeof e.data !== "string") return;
    removeEventListener("message", take);
    document.open();
    document.write(PRELUDE + e.data);
    document.close();
  });
  globalThis.parent.postMessage("ready", "*");
}
