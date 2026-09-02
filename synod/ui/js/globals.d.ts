// The three libraries index.html loads as plain <script src> tags, and the
// bridge withGlobalTauri puts on the window.  They are globals by choice —
// the window has no bundler — so this is where their shapes are declared,
// loosely: none of them is the seam this check exists to police.
declare const marked: any;
declare const DOMPurify: any;
declare const katex: any;

interface Window {
  __TAURI__?: any;
}
