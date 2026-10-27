// Vendored script globals and the injected Tauri bridge need declarations without a bundler.
declare const marked: any;
declare const DOMPurify: any;
declare const katex: any;

interface Window {
  __TAURI__?: any;
}
