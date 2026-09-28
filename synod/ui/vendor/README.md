# Vendored libraries

Synod's window ships no bundler and loads nothing from the network: its content
security policy (`synod/tauri.conf.json`) admits only files from the app itself.
The three libraries it renders assistant prose with are therefore copied here,
unmodified, as their authors published them to npm, each beside its licence.

| Library | Version | Files | Licence | Source |
|---|---|---|---|---|
| [DOMPurify](https://github.com/cure53/DOMPurify) | 3.4.16 | `dompurify/purify.min.js` | Apache-2.0 OR MPL-2.0 (used under Apache-2.0) | `dompurify@3.4.16`, `dist/` |
| [marked](https://github.com/markedjs/marked) | 12.0.2 | `marked/marked.min.js` | MIT, with the BSD-style notice of the original `Markdown.pl` | `marked@12.0.2`, root |
| [KaTeX](https://github.com/KaTeX/KaTeX) | 0.18.1 | `katex/katex.min.js`, `katex/katex.min.css`, `katex/fonts/*.woff2` | MIT (code and fonts) | `katex@0.18.1`, `dist/` |

- **Licences.** Each directory holds its library's licence file, copied from the
  same npm release; the minified files also keep their own licence headers.
  All three are permissive and compatible with this repository's MIT OR
  Apache-2.0 licensing. None ships a `NOTICE` file.
  - DOMPurify offers Apache-2.0 or MPL-2.0 (its header says so); its npm
    release's `LICENSE` is the Apache-2.0 text, the licence it is used under
    here, and its copyright notice is the header's "(c) Cure53 and other
    contributors".
  - marked's `LICENSE.md` carries, beside its own MIT terms, John Gruber's
    BSD-style licence for `Markdown.pl`, whose notice must travel with any
    redistribution; it does, in that file.
- **KaTeX is a subset.** Only the `woff2` faces are kept: `katex.min.css` also
  names `woff` and `ttf` fallbacks, which every engine Synod runs on (WebView2,
  WebKit) never requests, since it takes the `woff2` source listed first.
- **Integrity.** `SHA256SUMS` lists every file here except this README and
  itself. `.gitattributes` marks the directory `-text`, so a checkout never
  rewrites line endings and the sums hold on every platform. Verify with
  `sha256sum -c SHA256SUMS` from this directory.

## Updating one

1. Download the new release's files from npm (for example
   `https://cdn.jsdelivr.net/npm/<name>@<version>/<path>`) over the old ones,
   together with its licence file.
2. Update the table above and regenerate the sums:
   `find . -type f ! -name README.md ! -name SHA256SUMS | sort | xargs sha256sum > SHA256SUMS`
3. Check the page still renders prose, code and mathematics, and that
   `DOMPurify`'s `SCRUB` configuration in `js/math.js` still behaves as before.
