new ResizeObserver(() => {
  globalThis.parent.postMessage({ height: document.documentElement.getBoundingClientRect().height }, "*");
}).observe(document.documentElement);
