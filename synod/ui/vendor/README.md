# Vendored libraries

Synod's window ships no bundler and loads nothing from the network: its content
security policy (`synod/tauri.conf.json`) admits only files from the app itself.
The libraries it renders with are therefore copied here, unmodified, as their
authors published them to npm, each beside its licence.

| Library | Version | Files | Licence | Source |
|---|---|---|---|---|
| [DOMPurify](https://github.com/cure53/DOMPurify) | 3.4.16 | `dompurify/purify.min.js` | Apache-2.0 OR MPL-2.0 (used under Apache-2.0) | `dompurify@3.4.16`, `dist/` |
| [marked](https://github.com/markedjs/marked) | 18.1.0 | `marked/marked.umd.js` | MIT, with the BSD-style notice of the original `Markdown.pl` | `marked@18.1.0`, `lib/` |
| [KaTeX](https://github.com/KaTeX/KaTeX) | 0.19.0 | `katex/katex.min.js`, `katex/katex.min.css`, `katex/fonts/*.woff2` | MIT (code and fonts) | `katex@0.19.0`, `dist/` |
| [three.js](https://github.com/mrdoob/three.js) | 0.170.0 | `three/three.module.min.js`, `three/AsciiEffect.js` | MIT | `three@0.170.0`, `build/` and `examples/jsm/effects/` |
| [Chart.js](https://github.com/chartjs/Chart.js) | 4.5.1 | `chartjs/chart.umd.min.js` | MIT | `chart.js@4.5.1`, `dist/` |
| [D3](https://github.com/d3/d3) | 7.9.0 | `d3/d3.min.js` | ISC | `d3@7.9.0`, `dist/` |
| [Mermaid](https://github.com/mermaid-js/mermaid) | 12.1.0 | `mermaid/mermaid.min.js` | MIT, with the bundled notices of its dependencies | `mermaid@12.1.0`, `dist/` |
| [highlight.js](https://github.com/highlightjs/highlight.js) | 11.12.0 | `highlight/highlight.min.js` | BSD-3-Clause | `@highlightjs/cdn-assets@11.12.0`, package root |

- **Licences.** Each directory holds its library's licence file, copied from the
  same npm release; the minified files also keep their own licence headers.
  All eight are permissive and compatible with this repository's MIT OR
  Apache-2.0 licensing. None ships a `NOTICE` file.
  - DOMPurify offers Apache-2.0 or MPL-2.0 (its header says so); its npm
    release's `LICENSE` is the Apache-2.0 text, the licence it is used under
    here, and its copyright notice is the header's "(c) Cure53 and other
    contributors".
  - marked's `LICENSE` carries, beside its own MIT terms, John Gruber's
    BSD-style licence for `Markdown.pl`, whose notice must travel with any
    redistribution; it does, in that file.
  - Chart.js's licence file is `LICENSE.md`. D3 is ISC, not MIT.
  - Mermaid's bundle inlines its dependencies; their notices (DOMPurify 3.4.12,
    Apache-2.0 OR MPL-2.0; lodash-es and cytoscape, MIT, as are the helpers
    cytoscape carries) sit in the trailing `Bundled license information`
    comment of `mermaid.min.js`, which travels with the file. The npm release
    ships no separate notice file.
- **KaTeX is a subset.** Only the `woff2` faces are kept: `katex.min.css` also
  names `woff` and `ttf` fallbacks, which every engine Synod runs on (WebView2,
  WebKit) never requests, since it takes the `woff2` source listed first.
- **highlight.js is the common-languages build.** Only the grammars in
  `highlight.min.js` exist; a fence naming any other language stays plain. Its
  theme CSS is not vendored: `css/document.css` colours the scopes.
- **three.js is a subset.** Only the module build and the one example effect
  that draws the armillary (`js/armillary.js`) are kept, both flattened into
  `three/`; `AsciiEffect.js` imports nothing, so no import map is needed. Neither
  ships types: `js/three.d.ts` and `js/ascii-effect.d.ts` declare the slice
  `armillary.js` uses, for `deno check`.
- **Live pages see all of it.** They load `vendor/` through the `synod-live`
  scheme (`synod/src/window/live.rs`), so every file here is reachable from a
  page.
- **No `'unsafe-eval'`.** Pages run without it. D3's
  `csvParse`/`tsvParse`/`dsvFormat` row-object conversion uses `new Function`
  and fails there. Vega, Alpine and Babel are unusable for the same reason.
- **Integrity.** `SHA256SUMS` lists every file here except this README and
  itself. `.gitattributes` marks the directory `-text`, so a checkout never
  rewrites line endings and the sums hold on every platform. Verify with
  `sha256sum -c SHA256SUMS` from this directory.

## Updating one

1. Download the new release's files from npm (for example
   `https://cdn.jsdelivr.net/npm/<name>@<version>/<path>`) over the old ones,
   together with its licence file.
2. Update the table above and regenerate the sums:
   `find . -type f ! -name README.md ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort -f | xargs sha256sum -b > SHA256SUMS`
3. Check the page still renders prose, code and mathematics, and that
   `DOMPurify`'s `SCRUB` configuration in `js/math.js` still behaves as before.
   Also run the `live-html` smoke page in the "Smoke page" section of
   `docs/ral-wiki/design/live-page-egress.md`: every library on
   it must render.
