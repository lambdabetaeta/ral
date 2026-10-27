import { $ } from "./dom.js";
import { state } from "./core.js";
import { openFile } from "./ipc.js";

// Changes are already real; timestamp-only touches cannot prove that contents differ.
/** @type {Record<import("./bindings/ChangeKind.ts").ChangeKind, {glyph: string, verb: string}>} */
const KINDS = {
  created: { glyph: "+", verb: "Created" },
  modified: { glyph: "~", verb: "Modified" },
  touched: { glyph: "≈", verb: "Written to" },
  deleted: { glyph: "−", verb: "Deleted" },
  renamed: { glyph: "→", verb: "Renamed" },
};

const TOUCHED_LEGEND = "≈ means something wrote to the file without changing its size. "
  + "Synod does not read your files, so it cannot tell you whether the contents differ.";

const UNAVAILABLE = "Synod could not look over your folder after the last message, so this list cannot say what has changed.";

/** @type {import("./bindings/WindowReport.ts").WindowReport} */
const EMPTY = Object.freeze({ files: [], unreadable: [] });

/** @type {import("./bindings/WindowReport.ts").WindowReport | null} */
let report = EMPTY;

// Retain shown-row identities so refreshes do not replay arrival animations.
let shownRows = new Set();

export function resetReport() {
  report = EMPTY;
  shownRows = new Set();
  renderReport();
}

/** @param {import("./bindings/WindowReport.ts").WindowReport | null} next */
export function showReport(next) {
  report = next;
  renderReport();
}

function showBanner(text) {
  const b = $("review-banner");
  b.textContent = text;
  b.classList.add("show");
}

// Disk names become text nodes, never markup.
function renderGap(names) {
  const gap = $("report-gap");
  gap.textContent = "";
  gap.classList.toggle("show", names.length > 0);
  if (names.length === 0) return;
  gap.append("Synod could not read ");
  names.forEach((name, i) => {
    if (i > 0) gap.append(i === names.length - 1 ? " and " : ", ");
    const code = document.createElement("code");
    code.textContent = name;
    gap.append(code);
  });
  gap.append(" while looking at the folder, so anything in "
    + (names.length === 1 ? "it" : "them")
    + " is not shown here.");
}

function renderReport() {
  $("review-banner").classList.remove("show");
  const files = report?.files ?? [];

  // An unavailable account must never be displayed as no changes.
  const empty = $("report-empty");
  const failed = $("report-error");
  failed.textContent = report === null ? UNAVAILABLE : "";
  failed.classList.toggle("show", report === null);
  renderGap(report?.unreadable ?? []);
  empty.style.display = report !== null && files.length === 0 ? "" : "none";

  const legend = $("report-legend");
  const anyTouched = files.some((f) => f.kind === "touched");
  legend.textContent = anyTouched ? TOUCHED_LEGEND : "";
  legend.classList.toggle("show", anyTouched);

  $("bulk").style.display = files.length === 0 ? "none" : "";
  $("review-count").textContent = files.length === 1
    ? "1 change in this folder"
    : files.length + " changes in this folder";

  const shown = shownRows;
  shownRows = new Set(files.map((f) => f.path));
  const list = $("cards");
  list.innerHTML = "";
  // Group by folder but preserve original report indices for button requests.
  const rows = files.map((file, index) => ({ file, index }))
    .sort((a, b) => dirOf(a.file.path).localeCompare(dirOf(b.file.path))
      || baseOf(a.file.path).localeCompare(baseOf(b.file.path)));
  let group = null;
  let surface = list;
  for (const { file, index } of rows) {
    const dir = dirOf(file.path);
    if (dir !== group) {
      group = dir;
      surface = document.createElement("div");
      surface.className = "group-list";
      list.append(groupHeading(dir), surface);
    }
    surface.appendChild(renderCard(file, index, shown.has(file.path)));
  }
}

const dirOf = (path) => path.slice(0, path.lastIndexOf("/") + 1);
const baseOf = (path) => path.slice(path.lastIndexOf("/") + 1);

function groupHeading(dir) {
  const el = document.createElement("div");
  el.className = "group";
  const folder = /** @type {import("./bindings/PickedFolder.ts").PickedFolder} */ (state.folder).label;
  el.textContent = dir || baseOf(folder.replace(/\/+$/, "")) + "/";
  el.title = dir || folder;
  return el;
}

/** @param {import("./bindings/ChangeFile.ts").ChangeFile} file */
function renderCard(file, index, settled) {
  const k = KINDS[file.kind];
  const row = document.createElement(file.open ? "button" : "div");
  row.className = "card" + (file.kind === "touched" ? " touched" : "") + (settled ? " settled" : "")
    + (file.open ? " actionable" : "");
  row.title = file.kind === "renamed" && file.rename_from
    ? "Renamed " + file.rename_from + " to " + file.path
    : k.verb + " " + file.path;

  const glyph = document.createElement("span");
  glyph.className = "glyph " + file.kind;
  glyph.textContent = k.glyph;
  glyph.setAttribute("role", "img");
  glyph.setAttribute("aria-label", k.verb);

  row.append(glyph, nameCell(file));

  // The shell checks the file; the row names only the report index and shown name.
  if (file.open) {
    const label = file.open === "open" ? "Open" : "Show in folder";
    row.setAttribute("type", "button");
    row.title += "\n" + label;
    row.setAttribute("aria-label", label + " " + file.path + ", " + k.verb.toLowerCase());
    row.addEventListener("click", async () => {
      try { await openFile(index, file.path); }
      catch (err) { showBanner(String(err)); }
    });
  }
  return row;
}

function nameCell(file) {
  const cell = document.createElement("span");
  cell.className = "name";
  if (file.kind === "renamed" && file.rename_from) {
    const lead = document.createElement("span");
    lead.className = "lead";
    lead.textContent = baseOf(file.rename_from) + " → ";
    cell.appendChild(lead);
  }
  const base = document.createElement("span");
  base.className = "base";
  base.textContent = baseOf(file.path);
  cell.appendChild(base);
  return cell;
}
