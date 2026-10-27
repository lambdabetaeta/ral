const DIFF_PEEK_ROWS = 10;

/**
 * @param {import("./bindings/MarkDto.ts").MarkDto[]} marks
 * @param {number} rung
 * @param {(() => void) | null} onCycle
 */
export function renderMarkCard(marks, rung = 2, onCycle = null) {
  const card = document.createElement("div");
  card.className = "mark-card";
  let room = rung === 0 ? 0 : rung === 1 ? DIFF_PEEK_ROWS : Infinity;
  for (const m of marks || []) {
    if (m.mark === "diff") {
      const el = renderDiffHeader(m, rung, onCycle);
      if (rung > 0) room -= appendDiffRows(el, m, room);
      card.appendChild(el);
    } else {
      card.appendChild(renderMark(m));
    }
  }
  return card;
}

/** @param {Exclude<import("./bindings/MarkDto.ts").MarkDto, { mark: "diff" }>} m */
function renderMark(m) {
  switch (m.mark) {
    case "text": return renderTextMark(m);
    case "measure": return renderMeasureMark(m);
    case "fields": return renderFieldsMark(m);
    case "raw": return renderRawMark(m);
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

const diffRows = (m) => (m.hunks || []).flatMap((h) => h.rows || []);

function renderDiffHeader(m, rung, onCycle) {
  const container = document.createElement("div");
  container.className = "diff";

  const rows = diffRows(m);
  const count = (tag) => rows.filter((r) => r.tag === tag).length;
  if (rows.every((r) => r.tag === rows[0].tag)) container.classList.add("uniform");

  const head = document.createElement(onCycle ? "button" : "div");
  head.className = "diff-head";
  if (onCycle) {
    head.setAttribute("type", "button");
    head.setAttribute("aria-expanded", String(rung > 0));
    head.addEventListener("click", onCycle);
  }
  const path = document.createElement("span");
  path.className = "role-path";
  path.textContent = m.path;
  head.appendChild(path);
  const counts = document.createElement("span");
  counts.className = "diff-counts";
  counts.textContent = "+" + count("add") + " −" + count("del");
  head.appendChild(counts);
  container.appendChild(head);
  return container;
}

function appendDiffRows(container, m, room) {
  let shown = 0;
  let maxNo = 0;
  for (const [i, hunk] of (m.hunks || []).entries()) {
    if (shown === room) break;
    if (i > 0) container.appendChild(elisionRow());
    let oldNo = hunk.old;
    let newNo = hunk.new;
    for (const row of hunk.rows || []) {
      if (shown === room) break;
      const line = document.createElement("div");
      line.className = "diff-row " + (row.tag === "add" ? "add" : row.tag === "del" ? "del" : "ctx");

      const gutter = document.createElement("span");
      gutter.className = "gutter";
      maxNo = Math.max(maxNo, row.tag === "del" ? oldNo : newNo);
      if (row.tag === "del") {
        gutter.textContent = oldNo + "−";
        oldNo++;
      } else if (row.tag === "add") {
        gutter.textContent = newNo + "+";
        newNo++;
      } else {
        gutter.textContent = newNo + " ";
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
      shown++;
    }
  }
  if (shown < diffRows(m).length) container.appendChild(elisionRow());
  container.style.setProperty("--gutter-digits", String(String(maxNo).length));
  return shown;
}

function elisionRow() {
  const row = document.createElement("div");
  row.className = "diff-row elision";
  const gutter = document.createElement("span");
  gutter.className = "gutter";
  gutter.textContent = "⋮";
  row.appendChild(gutter);
  return row;
}

function renderRawMark(m) {
  const pre = document.createElement("pre");
  pre.className = "raw-mark";
  pre.textContent = m.text;
  return pre;
}
