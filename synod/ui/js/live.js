import { pinTranscript } from "./core.js";
import { LIVE_FRAME_URL } from "./ipc.js";

// A frame asks for its page on every load, so one reloaded by reattachment refills itself.
/** @type {WeakMap<HTMLIFrameElement, string>} */
const pages = new WeakMap();
const MIN_PX = 24;
const MAX_PX = 1600;
const FENCE = "pre > code.language-live-html";

/**
 * @param {DocumentFragment} root
 * @param {(code: Element) => HTMLElement} make
 */
function replaceFences(root, make) {
  for (const code of root.querySelectorAll(FENCE)) code.parentElement?.replaceWith(make(code));
}

/** @param {DocumentFragment} root */
export function mountLiveFrames(root) {
  replaceFences(root, (code) => {
    const frame = document.createElement("iframe");
    // Sandbox before src: the first load must already be sealed.
    frame.setAttribute("sandbox", "allow-scripts");
    frame.className = "live-frame";
    frame.title = "Interactive page";
    pages.set(frame, code.textContent ?? "");
    frame.src = LIVE_FRAME_URL;
    return frame;
  });
}

/** @param {DocumentFragment} root */
export function holdLiveFrames(root) {
  replaceFences(root, () => {
    const hold = document.createElement("div");
    hold.className = "live-pending";
    return hold;
  });
}

// An opaque origin has no name to target; contentWindow fixes the recipient.
addEventListener("message", (e) => {
  const frame = frameOf(e.source);
  if (!frame) return;
  if (e.data === "ready") frame.contentWindow?.postMessage(pages.get(frame), "*");
  else if (Number.isFinite(e.data?.height)) {
    frame.style.height = `${Math.min(MAX_PX, Math.max(MIN_PX, Math.ceil(e.data.height)))}px`;
    pinTranscript();
  }
});

/** @param {MessageEventSource | null} source */
function frameOf(source) {
  /** @type {NodeListOf<HTMLIFrameElement>} */
  const frames = document.querySelectorAll("iframe.live-frame");
  for (const frame of frames) {
    if (frame.contentWindow === source) return frame;
  }
  return null;
}
