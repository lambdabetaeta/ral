# Streaming markdown in the terminal

The current implementation streams raw rainbow text and repaints once
at the end with `termimad`.  Cheap, no parser, gets the "something is
happening" feeling without the bookkeeping.  The notes below sketch
the better approach for when the final-only repaint becomes a visible
jump rather than a satisfying reveal.

## Reference

Will McGugan's writeup for Rich/Textual gives the algorithm we'd
crib from:

  https://willmcgugan.github.io/streaming-markdown/

Streak's blog covers the inline-syntax repair pass that web frontends
like ChatGPT layer on top:

  https://engineering.streak.com/p/preventing-unstyled-markdown-streaming-ai

## The block trick

Markdown documents split into top-level blocks — heading, paragraph,
fenced code, list, blockquote, table.  When you append, only the
*last* block can change.  Everything else is final.

The renderer becomes:

1. Buffer deltas; classify the in-progress block from its first
   non-blank line — `# ` heading, ```` ``` ```` code fence (until matching
   close), `- ` / `* ` / `1. ` list item, blank line ends the block,
   anything else is paragraph.
2. As deltas land, re-render the in-progress block by walking the
   cursor up over its previously-emitted height (`\x1b[<n>A\x1b[0J`),
   running `termimad` on the buffer, and reprinting.
3. On block close (blank line, fence close, end-of-stream), freeze
   the block — emit it once, never repaint.
4. Light coalescing buffer (~16ms) so a 20-token burst doesn't
   trigger 20 repaints.

## Optional polish (skipped)

Web frontends preprocess the in-progress buffer to close dangling
markers (`**` → `**bold**`, `` ` `` → `` `code` ``, `[x](` → strip)
so half-typed inline syntax doesn't flash as raw asterisks.  Skip
unless flashes prove distracting.

## When to upgrade

Move to the block-incremental approach when:

- responses regularly exceed a screen, so the final repaint scrolls
  noticeably; or
- users complain that they can't read the streamed text because the
  rainbow is "too much" (the colour is the only way to tell live
  text from finished, and McGugan's approach lets us drop it).
