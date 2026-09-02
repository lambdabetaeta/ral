// ---- The "a little Bertin-pilled" card renderer ---------------------
// Named `renderMarkCard` rather than `renderCard` because the report
// column below already owns that name for a file-change card — a
// different kind of card entirely.
/** @param {import("./bindings/MarkDto.ts").MarkDto[]} marks */
export function renderMarkCard(marks) {
  const card = document.createElement("div");
  card.className = "mark-card";
  for (const m of marks || []) card.appendChild(renderMark(m));
  return card;
}

/** @param {import("./bindings/MarkDto.ts").MarkDto} m */
function renderMark(m) {
  switch (m.mark) {
    case "text": return renderTextMark(m);
    case "measure": return renderMeasureMark(m);
    case "fields": return renderFieldsMark(m);
    case "diff": return renderDiffMark(m);
    case "raw": return renderRawMark(m);
    // Unreachable once every variant above is handled — which `never` is what
    // asserts, so a new Rust mark the window forgets fails the build.  The
    // dump below stays as the runtime floor: a mark from a newer backend than
    // this window should still show something rather than nothing.
    default: {
      /** @type {never} */
      const _unhandled = m;
      const el = document.createElement("pre");
      el.className = "raw-mark";
      el.textContent = JSON.stringify(m);
      return el;
    }
  }
}

function roleSpan(span) {
  const el = document.createElement("span");
  el.className = "role-" + (span.role || "plain");
  el.textContent = span.text;
  return el;
}

function renderTextMark(m) {
  const p = document.createElement("p");
  p.className = "mark-text";
  for (const span of m.spans || []) p.appendChild(roleSpan(span));
  return p;
}

// A measure standing alone names itself; one in a fields row does not
// — the row's label column already names it, exactly the distinction
// the TUI's `measure_value_spans` draws.
// The measure's payload is exarch's shape, unchecked by design — the union
// checks the `mark` tag and stops there.
/** @param {Record<string, any>} m */
function renderMeasureMark(m) {
  const row = document.createElement("div");
  row.className = "measure-row";
  const label = document.createElement("span");
  label.className = "measure-label";
  label.textContent = m.label;
  row.append(label, ...measureValue(m));
  return row;
}

// The meter and readout, without the measure's own label.
function measureValue(m) {
  const meter = document.createElement("span");
  meter.className = "meter";
  const fill = document.createElement("span");
  fill.className = "meter-fill";
  const pct = m.max
    ? Math.min(100, 100 * m.value / m.max)
    : Math.min(100, 100 * Math.log2(m.value + 1) / 32);
  fill.style.width = pct + "%";
  meter.appendChild(fill);

  const readout = document.createElement("span");
  readout.className = "measure-readout";
  readout.textContent = String(m.value) + (m.max ? "/" + m.max : "") + (m.unit ? " " + m.unit : "");
  return [meter, readout];
}

function renderFieldsMark(m) {
  const grid = document.createElement("div");
  grid.className = "fields";
  for (const row of m.rows || []) {
    const label = document.createElement("div");
    label.className = "f-label";
    label.textContent = row.label;
    grid.appendChild(label);

    const value = document.createElement("div");
    value.className = "f-value";
    if (row.value && row.value.inline) {
      for (const span of row.value.inline) value.appendChild(roleSpan(span));
    } else if (row.value && row.value.measure) {
      const bare = document.createElement("div");
      bare.className = "measure-row";
      bare.append(...measureValue(row.value.measure));
      value.appendChild(bare);
    }
    grid.appendChild(value);
  }
  return grid;
}

function renderDiffMark(m) {
  const container = document.createElement("div");
  container.className = "diff";

  let adds = 0, dels = 0;
  for (const hunk of m.hunks || []) {
    for (const row of hunk.rows || []) {
      if (row.tag === "add") adds++;
      else if (row.tag === "del") dels++;
    }
  }

  const head = document.createElement("div");
  head.className = "diff-head";
  const path = document.createElement("span");
  path.className = "role-path";
  path.textContent = m.path;
  head.appendChild(path);
  const counts = document.createElement("span");
  counts.className = "diff-counts";
  counts.textContent = "+" + adds + " −" + dels;
  head.appendChild(counts);
  container.appendChild(head);

  for (const hunk of m.hunks || []) {
    let oldNo = hunk.start;
    let newNo = hunk.start;
    for (const row of hunk.rows || []) {
      const line = document.createElement("div");
      line.className = "diff-row " + (row.tag === "add" ? "add" : row.tag === "del" ? "del" : "ctx");

      const gutter = document.createElement("span");
      gutter.className = "gutter";
      if (row.tag === "del") {
        gutter.textContent = oldNo + "−";
        oldNo++;
      } else if (row.tag === "add") {
        gutter.textContent = newNo + "+";
        newNo++;
      } else {
        gutter.textContent = oldNo + " ";
        oldNo++;
        newNo++;
      }
      line.appendChild(gutter);

      const segs = document.createElement("span");
      segs.className = "diff-segs";
      for (const seg of row.segs || []) {
        if (seg.emph) {
          const em = document.createElement("em");
          em.className = "seg-emph";
          em.textContent = seg.text;
          segs.appendChild(em);
        } else {
          segs.appendChild(document.createTextNode(seg.text));
        }
      }
      line.appendChild(segs);
      container.appendChild(line);
    }
  }
  return container;
}

function renderRawMark(m) {
  const pre = document.createElement("pre");
  pre.className = "raw-mark";
  pre.textContent = m.text;
  return pre;
}
