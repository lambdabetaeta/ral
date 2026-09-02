import { $, invoke, state } from "./core.js";
import { actionButton } from "./ui-helpers.js";

// ---- The change-report panel ----------------------------------------
// The kind is marked by shape; the verb names the glyph to a reader who
// hovers the row, and to a screen reader.
const KINDS = {
  created: { glyph: "+", verb: "Created" },
  modified: { glyph: "~", verb: "Modified" },
  deleted: { glyph: "−", verb: "Deleted" },
  renamed: { glyph: "→", verb: "Renamed" },
};
const UNKNOWN_KIND = { glyph: "·", verb: "Changed" };

// Server commands (job_report, undo_file, undo_all) replace this
// wholesale; "Keep mine" at a conflict is a local acknowledgement, so it
// mutates this copy directly.
/** @type {import("./bindings/WindowReport.ts").WindowReport} */
let report = { files: [], unreadable: [] };

// Set when `job_report` itself failed — the store or a checkpoint record
// could not be read — as opposed to succeeding with an empty change set.
// `renderReport` must never call the former "nothing has changed".
let reportError = null;

// The rows already on screen: a revert rebuilds the whole list, and the
// survivors flashing in again would read as fresh work.
let shownRows = new Set();

// Wipe the report back to empty and render, for a fresh conversation.
// Called by conversation.js's resetTranscript.
export function resetReport() {
  report = { files: [], unreadable: [] };
  reportError = null;
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

// Names the copy listed but could not read. Built as nodes, never as
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
  gap.append(" while taking its copy, so anything in "
    + (names.length === 1 ? "it" : "them")
    + " is not shown here and cannot be put back.");
}

// The column answers one question: what stands changed in the folder
// right now. A reverted file is no longer changed, so its row leaves.
function renderReport() {
  $("review-banner").classList.remove("show");
  const files = report.files || [];
  const inPlace = files.filter((f) => f.status === "applied");

  // A report that could not be taken is not an empty one: saying "nothing
  // has changed" here would promise a safety net that is in fact broken.
  const empty = $("report-empty");
  const failed = $("report-error");
  failed.textContent = reportError ?? "";
  failed.classList.toggle("show", reportError !== null);
  renderGap(reportError ? [] : report.unreadable || []);
  empty.style.display = reportError === null && inPlace.length === 0 ? "" : "none";
  empty.textContent = files.length === 0
    ? "Nothing has changed in this folder yet."
    : "Everything has been reverted.";

  $("bulk").style.display = inPlace.length === 0 ? "none" : "";
  $("review-count").textContent = inPlace.length === 1
    ? "1 change is in place"
    : inPlace.length + " changes are in place";

  const shown = shownRows;
  shownRows = new Set(inPlace.map((f) => f.id));
  const list = $("cards");
  list.innerHTML = "";
  // Sorted by folder, then by name, so each folder is named exactly once.
  inPlace.sort((a, b) => dirOf(a.path).localeCompare(dirOf(b.path))
    || baseOf(a.path).localeCompare(baseOf(b.path)));
  let group = null;
  for (const file of inPlace) {
    const dir = dirOf(file.path);
    if (dir !== group) {
      group = dir;
      list.appendChild(groupHeading(dir));
    }
    list.appendChild(renderCard(file, shown.has(file.id)));
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
  row.className = "card" + (file.conflict ? " conflict" : "") + (settled ? " settled" : "");
  row.title = file.kind === "renamed" && file.rename_from
    ? "Renamed " + file.rename_from + " to " + file.path
    : k.verb + " " + file.path;

  const glyph = document.createElement("span");
  glyph.className = "glyph";
  glyph.textContent = k.glyph;
  glyph.setAttribute("role", "img");
  glyph.setAttribute("aria-label", k.verb);

  row.append(glyph, nameCell(file));

  if (file.conflict) {
    const note = document.createElement("p");
    note.className = "conflict-note";
    note.textContent = "You've changed this file yourself since the assistant finished. "
      + "Reverting would replace your newer version with the older one.";
    row.appendChild(note);
  }

  if (file.current_path) row.appendChild(openButton("Open", "open_file", { path: file.current_path }));
  if (file.conflict) {
    if (file.before_path) {
      row.appendChild(openButton("Open the older one", "open_earlier",
        { folder: state.folder, path: file.before_path }));
    }
    row.appendChild(actionButton("Keep mine", "btn-mini keep", () => keepMine(file.id)));
    row.appendChild(actionButton("Revert anyway", "btn-mini", () => undo("undo_file", file.id, true)));
  } else {
    row.appendChild(actionButton("Revert", "btn-mini", () => undo("undo_file", file.id, false)));
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

function openButton(label, cmd, args) {
  return actionButton(label, "btn-mini", async () => {
    try { await invoke(cmd, args); }
    catch (err) { showBanner(String(err)); }
  });
}

// Put a file (or everything) back — a server round trip, so the
// returned report is the new truth.  `force` is the explicit "put back
// the older one" choice at a conflict; a plain undo asks gently and a
// file the user edited since the exchange comes back marked conflicted.
async function undo(cmd, id, force) {
  try {
    const args = { folder: state.folder, force: Boolean(force) };
    if (id !== null) args.id = id;
    report = await invoke(cmd, args);
    renderReport();
  } catch (err) { showBanner(String(err)); }
}

// Keep the user's own version at a conflict: leave the file exactly as
// it is and simply clear the conflict on the card.  Nothing to undo, so
// this stays local until the next report refresh re-derives it.
function keepMine(id) {
  const file = report.files.find((f) => f.id === id);
  if (file) { file.conflict = false; renderReport(); }
}

$("undo-all").addEventListener("click", () => undo("undo_all", null, false));
