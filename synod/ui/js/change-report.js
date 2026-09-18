import { $, invoke, state } from "./core.js";
import { actionButton } from "./ui-helpers.js";

// ---- The change-report panel ----------------------------------------
// An account of what the assistant did, and nothing more: every change
// listed here is already real in the user's folder, and nothing on this
// panel puts anything back.
//
// The kind is marked by shape; the verb names the glyph to a reader who
// hovers the row, and to a screen reader. `touched` is deliberately not
// `modified`: synod reads no file's bytes, so a file whose timestamp moved
// while its size did not was written to by something, and whether the
// contents differ is not a thing the report can honestly claim.
const KINDS = {
  created: { glyph: "+", verb: "Created" },
  modified: { glyph: "~", verb: "Modified" },
  touched: { glyph: "≈", verb: "Written to" },
  deleted: { glyph: "−", verb: "Deleted" },
  renamed: { glyph: "→", verb: "Renamed" },
};
const UNKNOWN_KIND = { glyph: "·", verb: "Changed" };

// What the `≈` rows mean, said once under the heading rather than on
// every row that earns it.
const TOUCHED_LEGEND = "≈ means something wrote to the file without changing its size. "
  + "Synod does not read your files, so it cannot tell you whether the contents differ.";

/** @type {import("./bindings/WindowReport.ts").WindowReport} */
let report = { files: [], unreadable: [] };

// Set when `job_report` itself failed — the folder could not be walked, or
// the window and the shell disagree about which folder is live — as opposed
// to succeeding with an empty change set. `renderReport` must never call
// the former "nothing has changed".
let reportError = null;

// The rows already on screen, so the survivors of a refresh do not flash
// in again as though they were fresh work.
let shownRows = new Set();

// Wipe the report back to empty and render, for a fresh conversation.
// Called by conversation.js's resetTranscript.
export function resetReport() {
  report = { files: [], unreadable: [] };
  reportError = null;
  shownRows = new Set();
  renderReport();
}

export async function refreshReport() {
  try {
    report = await invoke("job_report", { folder: state.folder });
    reportError = null;
  } catch (err) {
    report = { files: [], unreadable: [] };
    reportError = String(err);
  }
  renderReport();
}

function showBanner(text) {
  const b = $("review-banner");
  b.textContent = text;
  b.classList.add("show");
}

// Names what a walk listed but could not read. Built as nodes, never as
// markup: these are names off the user's own disk.
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

// The column answers one question: what has this conversation changed in
// the folder so far.
function renderReport() {
  $("review-banner").classList.remove("show");
  const files = report.files || [];

  // A report that could not be taken is not an empty one: saying "nothing
  // has changed" here would claim an account synod does not in fact have.
  const empty = $("report-empty");
  const failed = $("report-error");
  failed.textContent = reportError ?? "";
  failed.classList.toggle("show", reportError !== null);
  renderGap(reportError ? [] : report.unreadable || []);
  empty.style.display = reportError === null && files.length === 0 ? "" : "none";

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
  // Sorted by folder, then by name, so each folder is named exactly once.
  const rows = files.slice().sort((a, b) => dirOf(a.path).localeCompare(dirOf(b.path))
    || baseOf(a.path).localeCompare(baseOf(b.path)));
  let group = null;
  for (const file of rows) {
    const dir = dirOf(file.path);
    if (dir !== group) {
      group = dir;
      list.appendChild(groupHeading(dir));
    }
    list.appendChild(renderCard(file, shown.has(file.path)));
  }
}

const dirOf = (path) => path.slice(0, path.lastIndexOf("/") + 1);
const baseOf = (path) => path.slice(path.lastIndexOf("/") + 1);

// The chosen folder's own name stands for the top level, so a file lying
// loose in it is not left under a heading that says nothing.
function groupHeading(dir) {
  const el = document.createElement("div");
  el.className = "group";
  // A card only renders once a conversation (and so state.folder) exists.
  const folder = /** @type {string} */ (state.folder);
  el.textContent = dir || baseOf(folder.replace(/\/+$/, "")) + "/";
  el.title = dir || folder;
  return el;
}

// `settled` says the row was already on screen before this render.
function renderCard(file, settled) {
  const k = KINDS[file.kind] || UNKNOWN_KIND;
  const row = document.createElement("div");
  row.className = "card" + (file.kind === "touched" ? " touched" : "") + (settled ? " settled" : "");
  row.title = file.kind === "renamed" && file.rename_from
    ? "Renamed " + file.rename_from + " to " + file.path
    : k.verb + " " + file.path;

  const glyph = document.createElement("span");
  glyph.className = "glyph";
  glyph.textContent = k.glyph;
  glyph.setAttribute("role", "img");
  glyph.setAttribute("aria-label", k.verb);

  row.append(glyph, nameCell(file));

  if (file.current_path) {
    row.appendChild(actionButton("Open", "btn-mini", async () => {
      try { await invoke("open_file", { path: file.current_path }); }
      catch (err) { showBanner(String(err)); }
    }));
  }
  return row;
}

// The file's own name — its folder is the heading above it. A rename
// carries the name it had before, which gives way first if the row is
// short of room.
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
