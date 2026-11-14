// Sheets: spreadsheets kept in ChloeDB, a page of its command center
// (#/app/sheets). The command center loads this file through chloedb-server,
// which forwards /v1/apps/sheets/... to chloedb-sheets, and runs it with its
// own helpers: render(el, ctx) gets ctx.core (h, menu, modal, toast, ...),
// ctx.charts, ctx.api (chloedb-sheets' API: the workbooks) and ctx.core.api
// (the server's: the cluster's tables, read and written with the signed-in
// key). It imports nothing, so chloedb-sheets serves it as one file.
//
//   #/app/sheets                 workbooks, imports (xlsx, CSV, SQLite), search, the cluster's tables
//   #/app/sheets/b/<id>[/s/A1]   a workbook: its grid, formulas, formats, charts, queries
//   #/app/sheets/t/<table>       a table of the cluster as a sheet: paged, sorted, filtered, edited

export default { render };

const MAX_ROW = 1048576, MAX_COL = 16384;
const HH = 24; // the column header's height
const TR = 64, TC = 26; // cells are read in tiles of TR rows and TC columns

const CSS = `
.sx-book { display: flex; flex-direction: column; height: 100%; min-height: 420px; background: var(--surface); }
.sx-top { display: flex; align-items: center; gap: 8px; padding: 8px 12px 6px; border-bottom: 1px solid var(--line); flex-wrap: wrap; }
.sx-top .sx-name { font-size: 16px; font-weight: 600; border: 1px solid transparent; background: none; padding: 3px 6px; border-radius: 6px; min-width: 80px; max-width: 420px; }
.sx-top .sx-name:hover { border-color: var(--line-2); }
.sx-top .sx-name:focus { border-color: var(--accent); outline: none; box-shadow: 0 0 0 3px var(--focus); background: var(--surface); }
.sx-saved { font-size: 12px; color: var(--muted); white-space: nowrap; }
.sx-saved.bad { color: var(--crit-ink); }
.sx-tools { display: flex; align-items: center; gap: 2px; padding: 4px 10px; border-bottom: 1px solid var(--line); flex-wrap: wrap; background: var(--surface); }
.sx-tools .sep { width: 1px; height: 20px; background: var(--line); margin: 0 5px; flex: none; }
.sx-tb { display: inline-flex; align-items: center; justify-content: center; gap: 3px; min-width: 26px; height: 28px; padding: 0 4px; border: 1px solid transparent; border-radius: 6px; background: none; cursor: pointer; color: var(--text-2); font-size: 13px; }
.sx-tb:hover { background: var(--surface-2); border-color: var(--line); }
.sx-tb[aria-pressed="true"] { background: var(--accent-soft); color: var(--accent); border-color: color-mix(in srgb, var(--accent) 35%, transparent); }
.sx-tb:disabled { opacity: .4; cursor: default; }
.sx-tb svg { width: 16px; height: 16px; stroke: currentColor; fill: none; stroke-width: 1.8; stroke-linecap: round; stroke-linejoin: round; }
.sx-tb .lt { font: 600 14px/1 Georgia, 'Times New Roman', serif; min-width: 12px; text-align: center; }
.sx-tb .lt.s { font: 600 12.5px/1 var(--sans); }
.sx-tb .sw { display: block; width: 14px; height: 3px; border-radius: 1px; margin-top: -2px; }
.sx-tb.col { flex-direction: column; gap: 0; padding-top: 2px; }
.sx-tb select, .sx-tools select { border: 1px solid var(--line-2); border-radius: 6px; background: var(--surface); height: 26px; font-size: 12.5px; padding: 0 4px; }
.sx-fbar { display: grid; grid-template-columns: 110px auto minmax(0, 1fr); align-items: stretch; border-bottom: 1px solid var(--line); min-height: 30px; }
.sx-fbar .sx-ref { border: 0; border-right: 1px solid var(--line); background: var(--surface); padding: 0 8px; font: 12.5px var(--mono); outline: none; }
.sx-fbar .sx-ref:focus { box-shadow: inset 0 0 0 2px var(--accent); }
.sx-fbar .sx-fx { display: flex; align-items: center; padding: 0 8px; color: var(--muted); font-style: italic; font-family: Georgia, serif; border-right: 1px solid var(--line); }
.sx-fbar textarea { border: 0; resize: none; background: var(--surface); padding: 6px 10px; font: 13px var(--mono); outline: none; height: 30px; line-height: 18px; overflow: hidden; white-space: pre; }
.sx-fbar textarea:focus { box-shadow: inset 0 0 0 2px var(--accent); height: auto; min-height: 30px; }
.sx-grid { position: relative; flex: 1 1 auto; min-height: 120px; overflow: hidden; background: var(--surface); }
.sx-grid canvas { position: absolute; left: 0; top: 0; }
.sx-scroll { position: absolute; inset: 0; overflow: auto; outline: none; }
.sx-spacer { position: relative; }
.sx-editor { position: absolute; z-index: 5; display: none; border: 2px solid var(--accent); box-shadow: 0 2px 10px rgba(0,0,0,.15); resize: none; outline: none; padding: 2px 4px; background: var(--surface); color: var(--text); overflow: hidden; white-space: pre-wrap; line-height: 1.25; }
.sx-ac { position: absolute; z-index: 30; background: var(--surface); border: 1px solid var(--line-2); border-radius: 8px; box-shadow: var(--shadow-2); max-height: 240px; overflow: auto; min-width: 260px; padding: 4px; font-size: 12.5px; }
.sx-ac div { padding: 4px 8px; border-radius: 5px; cursor: pointer; display: flex; gap: 10px; justify-content: space-between; }
.sx-ac div b { font-family: var(--mono); font-weight: 600; }
.sx-ac div span { color: var(--muted); }
.sx-ac div.on { background: var(--accent-soft); }
.sx-hint { position: absolute; z-index: 29; background: var(--surface); border: 1px solid var(--line-2); border-radius: 6px; padding: 3px 8px; font: 12px var(--mono); box-shadow: var(--shadow-1); pointer-events: none; color: var(--text-2); }
.sx-hint b { color: var(--accent); }
.sx-foot { display: flex; align-items: stretch; border-top: 1px solid var(--line); min-height: 34px; background: var(--surface-2); }
.sx-tabs { display: flex; align-items: stretch; gap: 1px; overflow-x: auto; flex: 1 1 auto; scrollbar-width: thin; padding-left: 4px; }
.sx-tab { border: 0; background: none; padding: 0 14px; cursor: pointer; color: var(--text-2); white-space: nowrap; border-bottom: 3px solid transparent; font-size: 13px; position: relative; }
.sx-tab:hover { background: var(--surface-3); }
.sx-tab.on { background: var(--surface); color: var(--text); font-weight: 600; box-shadow: inset 0 2px 0 var(--accent); }
.sx-tab .dot { display: inline-block; width: 8px; height: 8px; border-radius: 2px; margin-right: 6px; vertical-align: 0; }
.sx-tab input { width: 120px; font: inherit; border: 1px solid var(--accent); border-radius: 4px; padding: 1px 4px; }
.sx-stats { display: flex; gap: 14px; align-items: center; padding: 0 12px; font-size: 12px; color: var(--text-2); white-space: nowrap; }
.sx-stats b { font-weight: 600; color: var(--text); font-variant-numeric: tabular-nums; }
.sx-chart { position: absolute; z-index: 3; background: var(--surface); border: 1px solid var(--line-2); border-radius: 8px; box-shadow: var(--shadow-1); display: flex; flex-direction: column; overflow: hidden; }
.sx-chart.on { border-color: var(--accent); box-shadow: 0 0 0 2px var(--focus), var(--shadow-2); }
.sx-chart .hd { display: flex; align-items: center; gap: 6px; padding: 5px 6px 2px 10px; cursor: move; font-size: 12.5px; font-weight: 600; }
.sx-chart .hd .t { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sx-chart .bd { flex: 1; min-height: 0; padding: 0 6px 6px; }
.sx-chart .bd svg { width: 100%; height: 100%; display: block; overflow: visible; }
.sx-chart .bd svg text { font-size: 10.5px; fill: var(--muted); font-family: var(--sans); }
.sx-chart .hd .btn svg { width: 14px; height: 14px; }
.sx-chart .rs { position: absolute; right: 0; bottom: 0; width: 14px; height: 14px; cursor: nwse-resize; background: linear-gradient(135deg, transparent 50%, var(--line-2) 50%); }
.sx-panel { position: absolute; right: 10px; top: 10px; z-index: 20; width: min(380px, calc(100% - 20px)); max-height: calc(100% - 20px); overflow: auto; background: var(--surface); border: 1px solid var(--line-2); border-radius: 10px; box-shadow: var(--shadow-2); padding: 12px; }
.sx-panel h4 { margin: 0 0 8px; font-size: 13px; display: flex; align-items: center; gap: 6px; }
.sx-find { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }
.sx-find .input { flex: 1 1 140px; }
.sx-palette { display: grid; grid-template-columns: repeat(10, 18px); gap: 4px; padding: 8px; }
.sx-palette button { width: 18px; height: 18px; border-radius: 4px; border: 1px solid rgba(0,0,0,.15); cursor: pointer; padding: 0; }
.sx-pop { position: fixed; z-index: 90; background: var(--surface); border: 1px solid var(--line-2); border-radius: 10px; box-shadow: var(--shadow-2); }
.sx-form { display: grid; grid-template-columns: 120px minmax(0, 1fr); gap: 8px 12px; align-items: center; }
.sx-form label { font-size: 12.5px; color: var(--text-2); }
.sx-form .wide { grid-column: 1 / -1; }
.sx-checks { display: flex; flex-wrap: wrap; gap: 6px 14px; max-height: 220px; overflow: auto; }
.sx-checks label { display: inline-flex; gap: 6px; align-items: center; font-size: 13px; }
.sx-values { max-height: 260px; overflow: auto; border: 1px solid var(--line); border-radius: 8px; padding: 4px 8px; }
.sx-values label { display: flex; gap: 8px; align-items: center; padding: 2px 0; font-size: 13px; }
.sx-home-grid { display: grid; grid-template-columns: minmax(0, 1.25fr) minmax(0, 1fr); gap: 16px; align-items: start; }
.sx-home-grid > * { min-width: 0; }
.sx-hscroll { overflow-x: auto; max-width: 100%; }
.sx-hscroll table { min-width: 100%; }
.sx-whose { display: inline-flex; border: 1px solid var(--line); border-radius: 7px; overflow: hidden; }
.sx-whose button { border: 0; background: transparent; padding: 3px 10px; font-size: 12.5px; color: inherit; cursor: pointer; }
.sx-whose button[aria-pressed="true"] { background: var(--line); font-weight: 600; }
.sx-hits { list-style: none; margin: 0; padding: 0; }
.sx-hits li { padding: 7px 0; border-top: 1px solid var(--line); display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 8px; font-size: 13px; }
.sx-hits li:first-child { border-top: 0; }
.sx-hits mark { background: var(--warn-soft); color: inherit; border-radius: 2px; padding: 0 1px; }
.sx-book-ic { width: 30px; height: 30px; border-radius: 7px; display: inline-flex; align-items: center; justify-content: center; background: color-mix(in srgb, var(--s3) 16%, transparent); color: var(--s3); flex: none; }
.sx-book-ic.t { background: color-mix(in srgb, var(--s1) 16%, transparent); color: var(--s1); }
.sx-note { padding: 6px 12px; border-bottom: 1px solid var(--line); font-size: 12.5px; background: rgba(237, 161, 0, .12); }
.sx-note[hidden] { display: none; }
.sx-tbar { display: flex; align-items: center; gap: 8px; padding: 8px 12px; border-bottom: 1px solid var(--line); flex-wrap: wrap; }
.sx-tbar .input { min-width: 120px; }
.sx-sql { font: 12.5px var(--mono); width: 100%; min-height: 110px; resize: vertical; }
@media (max-width: 900px) {
  .sx-home-grid { grid-template-columns: minmax(0, 1fr); }
  .sx-fbar { grid-template-columns: 80px auto minmax(0, 1fr); }
  .sx-stats .opt { display: none; }
}
/* A phone: the formatting tools in one row that scrolls sideways, not four
   rows over the grid; a card's tools under its title, not past its edge. */
@media (max-width: 600px) {
  .sx-tools { flex-wrap: nowrap; overflow-x: auto; scrollbar-width: thin; }
  .sx-tools > * { flex: none; }
  .sx-home-grid .card > .hd { flex-wrap: wrap; row-gap: 8px; }
  /* the workbooks' name, owner, when changed and menu in view; where one
     came from and its cells are on the workbook */
  .sx-books th:nth-child(2), .sx-books td:nth-child(2), .sx-books th:nth-child(4), .sx-books td:nth-child(4) { display: none; }
}`;

// ---------------------------------------------------------------- small things

const colName = (c) => { let s = ""; while (c > 0) { const r = (c - 1) % 26; s = String.fromCharCode(65 + r) + s; c = Math.floor((c - 1) / 26); } return s; };
const cellRef = (r, c) => colName(c) + r;
const norm = (a) => ({ r0: Math.min(a.r0, a.r1), r1: Math.max(a.r0, a.r1), c0: Math.min(a.c0, a.c1), c1: Math.max(a.c0, a.c1) });
const areaRef = (a) => (a.r0 === a.r1 && a.c0 === a.c1 ? cellRef(a.r0, a.c0) : a.r0 === 1 && a.r1 >= MAX_ROW ? `${colName(a.c0)}:${colName(a.c1)}` : a.c0 === 1 && a.c1 >= MAX_COL ? `${a.r0}:${a.r1}` : `${cellRef(a.r0, a.c0)}:${cellRef(a.r1, a.c1)}`);
const colNum = (s) => [...s.toUpperCase()].reduce((a, ch) => a * 26 + ch.charCodeAt(0) - 64, 0);
const quoteSheet = (n) => (/^[A-Za-z_][A-Za-z0-9_.]*$/.test(n) ? n : `'${n.replace(/'/g, "''")}'`);
const qid = (n) => `"${String(n).replace(/"/g, '""')}"`;
const lit = (v) => `'${String(v).replace(/'/g, "''")}'`;

/** A reference typed in the name box: A1, B2:D9, C:C, 3:5 (a sheet name before ! is left to the caller). */
function parseRef(s) {
  s = String(s || "").trim().replace(/\$/g, "");
  const bang = s.lastIndexOf("!");
  const sheet = bang >= 0 ? s.slice(0, bang).replace(/^'|'$/g, "").replace(/''/g, "'") : null;
  s = bang >= 0 ? s.slice(bang + 1) : s;
  let m = s.match(/^([A-Za-z]{1,3})(\d{1,7})(?::([A-Za-z]{1,3})(\d{1,7}))?$/);
  if (m) { const a = { r0: +m[2], c0: colNum(m[1]), r1: +(m[4] || m[2]), c1: colNum(m[3] || m[1]) }; return valid(a) ? { ...norm(a), sheet } : null; }
  m = s.match(/^([A-Za-z]{1,3}):([A-Za-z]{1,3})$/);
  if (m) { const a = { r0: 1, r1: MAX_ROW, c0: colNum(m[1]), c1: colNum(m[2]) }; return valid(a) ? { ...norm(a), sheet } : null; }
  m = s.match(/^(\d{1,7}):(\d{1,7})$/);
  if (m) { const a = { r0: +m[1], r1: +m[2], c0: 1, c1: MAX_COL }; return valid(a) ? { ...norm(a), sheet } : null; }
  return null;
}
const valid = (a) => a.r0 >= 1 && a.r1 >= 1 && a.c0 >= 1 && a.c1 >= 1 && a.r0 <= MAX_ROW && a.r1 <= MAX_ROW && a.c0 <= MAX_COL && a.c1 <= MAX_COL;

const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent || "");
const mod = (e) => (isMac ? e.metaKey : e.ctrlKey);

function svgIcon(core, d) {
  const svg = core.s("svg", { viewBox: "0 0 24 24", "aria-hidden": "true", width: 16, height: 16, fill: "none", stroke: "currentColor", "stroke-width": 1.8, "stroke-linecap": "round", "stroke-linejoin": "round", class: "sx-ic" });
  svg.innerHTML = d;
  return svg;
}

// Toolbar glyphs (24x24, stroked).
const G = {
  undo: '<path d="M9 14 4 9l5-5"/><path d="M4 9h11a5 5 0 0 1 0 10h-3"/>',
  redo: '<path d="m15 14 5-5-5-5"/><path d="M20 9H9a5 5 0 0 0 0 10h3"/>',
  left: '<path d="M4 6h16M4 10h10M4 14h16M4 18h10"/>',
  center: '<path d="M4 6h16M7 10h10M4 14h16M7 18h10"/>',
  right: '<path d="M4 6h16M10 10h10M4 14h16M10 18h10"/>',
  top: '<path d="M4 4h16M12 8v12M8 12l4-4 4 4"/>',
  middle: '<path d="M4 12h16M12 3v5M12 16v5M9 6l3 2 3-2M9 18l3-2 3 2"/>',
  bottom: '<path d="M4 20h16M12 4v12M8 12l4 4 4-4"/>',
  wrap: '<path d="M4 6h16M4 12h13a3 3 0 0 1 0 6h-4M4 18h5"/><path d="m15 16-2 2 2 2"/>',
  border: '<rect x="4" y="4" width="16" height="16" rx="1"/><path d="M4 12h16M12 4v16" stroke-dasharray="2 2"/>',
  merge: '<rect x="3" y="5" width="18" height="14" rx="1"/><path d="M8 12h8M14 9l3 3-3 3M10 9l-3 3 3 3"/>',
  freeze: '<rect x="3" y="3" width="18" height="18" rx="1"/><path d="M3 9h18M9 3v18"/><path d="M3 9h6V3" fill="currentColor" fill-opacity=".25"/>',
  asc: '<path d="M7 4v16M3 16l4 4 4-4"/><path d="M14 5h6l-6 7h6M14 15h6M14 19h6"/>',
  desc: '<path d="M7 4v16M3 8l4-4 4 4"/><path d="M14 5h6l-6 7h6M14 15h6M14 19h6"/>',
  filter: '<path d="M3 4h18l-7 8.5V19l-4 2v-8.5z"/>',
  chart: '<path d="M4 20V10M10 20V4M16 20v-7M22 20H2"/>',
  pivot: '<rect x="3" y="3" width="18" height="18" rx="1"/><path d="M3 9h18M9 3v18M13 13h5M15.5 10.5v5"/>',
  sql: '<ellipse cx="12" cy="5" rx="8" ry="3"/><path d="M4 5v14c0 1.7 3.6 3 8 3s8-1.3 8-3V5M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3"/>',
  cf: '<rect x="3" y="4" width="18" height="16" rx="1"/><path d="M3 10h18M3 15h18"/><rect x="5" y="11.5" width="9" height="2" fill="currentColor"/><rect x="5" y="16.5" width="5" height="2" fill="currentColor"/>',
  sigma: '<path d="M18 5H6l6 7-6 7h12"/>',
  clear: '<path d="m4 20 6-6M14 4l6 6-9 9H6v-5z"/>',
  dec_up: '<text x="1" y="16" font-size="11" fill="currentColor" stroke="none" font-family="sans-serif">.0</text><path d="M16 8h6M19 5v6"/>',
  dec_down: '<text x="1" y="16" font-size="11" fill="currentColor" stroke="none" font-family="sans-serif">.0</text><path d="M16 8h6"/>',
  find: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>',
  name: '<path d="M4 7V5h16v2M9 19h6M12 5v14"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  paint: '<path d="M4 15 15 4l5 5L9 20H4z"/><path d="m13 6 5 5"/>',
  brush: '<rect x="4" y="3" width="14" height="6" rx="1"/><path d="M18 6h2v5h-8v3"/><rect x="10.5" y="14" width="3" height="7" rx="1"/>',
};

const PALETTE = ["#000000", "#434343", "#666666", "#999999", "#b7b7b7", "#cccccc", "#d9d9d9", "#efefef", "#f3f3f3", "#ffffff",
  "#980000", "#ff0000", "#ff9900", "#ffff00", "#00ff00", "#00ffff", "#4a86e8", "#0000ff", "#9900ff", "#ff00ff",
  "#e6b8af", "#f4cccc", "#fce5cd", "#fff2cc", "#d9ead3", "#d0e0e3", "#c9daf8", "#cfe2f3", "#d9d2e9", "#ead1dc",
  "#cc4125", "#e06666", "#f6b26b", "#ffd966", "#93c47d", "#76a5af", "#6d9eeb", "#6fa8dc", "#8e7cc3", "#c27ba0",
  "#85200c", "#990000", "#b45f06", "#bf9000", "#38761d", "#134f5c", "#1155cc", "#0b5394", "#351c75", "#741b47"];

const FORMATS = [
  ["general", "Automatic", "1000.5"],
  ["0", "Number, no decimals", "1001"],
  ["#,##0.00", "Number", "1,000.50"],
  ["$#,##0.00", "Currency", "$1,000.50"],
  ["$#,##0.00;[Red]-$#,##0.00", "Currency, negatives red", "-$1,000.50"],
  ["#,##0.00 \"€\"", "Euro", "1,000.50 €"],
  ["0%", "Percent", "50%"],
  ["0.00%", "Percent, 2 decimals", "50.00%"],
  ["0.00E+00", "Scientific", "1.00E+03"],
  ["yyyy-mm-dd", "Date", "2026-11-08"],
  ["dd/mm/yyyy", "Date (day first)", "08/11/2026"],
  ["mmm d, yyyy", "Date (long)", "Nov 8, 2026"],
  ["hh:mm:ss", "Time", "21:30:05"],
  ["yyyy-mm-dd hh:mm", "Date and time", "2026-11-08 21:30"],
  ["#,##0", "Thousands", "1,001"],
  ["@", "Plain text", "1000.5"],
];

// Functions the formula bar offers as you type (IronCalc has these and more).
const FUNCS = `ABS(number) ACOS(number) ADDRESS(row,col,[abs]) AND(a,b,...) AVERAGE(a,b,...) AVERAGEA(a,...) AVERAGEIF(range,criteria,[avg_range]) AVERAGEIFS(avg_range,range1,crit1,...)
BIN2DEC(bin) CEILING(number,significance) CHAR(code) CHOOSE(i,v1,v2,...) CLEAN(text) CODE(text) COLUMN([ref]) COLUMNS(array) CONCAT(a,b,...) CONCATENATE(a,b,...) CORREL(a,b) COS(number) COUNT(a,...) COUNTA(a,...) COUNTBLANK(range) COUNTIF(range,criteria) COUNTIFS(range1,crit1,...) COVARIANCE.P(a,b)
DATE(y,m,d) DATEDIF(start,end,unit) DATEVALUE(text) DAY(date) DAYS(end,start) DEC2BIN(n) DEC2HEX(n) DEGREES(angle) DOLLAR(n,[decimals]) EDATE(start,months) EOMONTH(start,months) EVEN(n) EXACT(a,b) EXP(n)
FACT(n) FALSE() FILTER(array,include,[if_empty]) FIND(find,within,[start]) FIXED(n,[decimals]) FLOOR(n,significance) FORECAST(x,known_y,known_x) FV(rate,nper,pmt,[pv],[type]) GCD(a,b,...) GEOMEAN(a,...) HLOOKUP(value,table,row,[approx]) HOUR(time) HYPERLINK(url,[name])
IF(test,then,[else]) IFERROR(value,if_error) IFNA(value,if_na) IFS(test1,value1,...) INDEX(array,row,[col]) INDIRECT(ref) INT(n) IPMT(rate,per,nper,pv) IRR(values,[guess]) ISBLANK(v) ISERROR(v) ISEVEN(n) ISNUMBER(v) ISODD(n) ISTEXT(v)
LARGE(array,k) LCM(a,b,...) LEFT(text,[n]) LEN(text) LET(name,value,calc) LN(n) LOG(n,[base]) LOG10(n) LOOKUP(value,lookup,[result]) LOWER(text) MATCH(value,array,[type]) MAX(a,...) MAXIFS(max_range,range1,crit1,...) MEDIAN(a,...) MID(text,start,n) MIN(a,...) MINIFS(min_range,range1,crit1,...) MINUTE(time) MOD(n,d) MODE(a,...) MONTH(date) MROUND(n,multiple)
NETWORKDAYS(start,end,[holidays]) NOT(v) NOW() NPER(rate,pmt,pv) NPV(rate,v1,...) ODD(n) OR(a,b,...) PERCENTILE(array,k) PERCENTRANK(array,x) PI() PMT(rate,nper,pv,[fv],[type]) POWER(n,p) PPMT(rate,per,nper,pv) PRODUCT(a,...) PROPER(text) PV(rate,nper,pmt)
QUARTILE(array,q) RADIANS(angle) RAND() RANDBETWEEN(lo,hi) RANK(n,ref,[order]) RATE(nper,pmt,pv) REPLACE(text,start,n,new) REPT(text,n) RIGHT(text,[n]) ROUND(n,digits) ROUNDDOWN(n,digits) ROUNDUP(n,digits) ROW([ref]) ROWS(array)
SEARCH(find,within,[start]) SECOND(time) SEQUENCE(rows,[cols],[start],[step]) SIGN(n) SIN(n) SLOPE(known_y,known_x) SMALL(array,k) SORT(array,[index],[order]) SORTBY(array,by,[order]) SQRT(n) STDEV(a,...) STDEV.P(a,...) STDEV.S(a,...) SUBSTITUTE(text,old,new,[n]) SUBTOTAL(fn,ref) SUM(a,...) SUMIF(range,criteria,[sum_range]) SUMIFS(sum_range,range1,crit1,...) SUMPRODUCT(a,b,...) SWITCH(expr,v1,r1,...)
TAN(n) TEXT(value,format) TEXTJOIN(delim,ignore_empty,a,...) TIME(h,m,s) TODAY() TRANSPOSE(array) TRIM(text) TRUE() TRUNC(n,[digits]) TYPE(v) UNIQUE(array) UPPER(text) VALUE(text) VAR(a,...) VAR.P(a,...) VLOOKUP(value,table,col,[approx]) WEEKDAY(date,[type]) WEEKNUM(date,[type]) WORKDAY(start,days) XLOOKUP(value,lookup,return,[if_not_found]) XOR(a,b,...) YEAR(date) YEARFRAC(start,end)`
  .split(/\s+/).filter(Boolean).map((f) => { const i = f.indexOf("("); return { name: f.slice(0, i), args: f.slice(i) }; });
const FUNC = new Map(FUNCS.map((f) => [f.name, f]));

/** A rule's colors as the theme has them now. */
function themeColors() {
  const cs = getComputedStyle(document.documentElement);
  const v = (n, d) => cs.getPropertyValue(n).trim() || d;
  return {
    bg: v("--surface", "#fff"), head: v("--surface-2", "#f4f5f7"), headOn: v("--accent-soft", "#e6ecfb"), line: v("--grid", "#e8e9ec"), line2: v("--line-2", "#cfd4dc"),
    text: v("--text", "#0f1216"), text2: v("--text-2", "#48505b"), muted: v("--muted", "#77808b"), accent: v("--accent", "#2f5bd3"),
    sel: "rgba(47, 91, 211, .10)", crit: v("--crit-ink", "#b42323"), dark: (cs.getPropertyValue("color-scheme") || "").includes("dark"),
  };
}

// ---------------------------------------------------------------- sizes along an axis

/** Positions along rows or columns: a default size, and the sizes that differ (0: hidden). */
class Axis {
  constructor(def) { this.def = def; this.set(new Map()); }
  set(map) {
    this.sizes = map;
    this.keys = [...map.keys()].sort((a, b) => a - b);
    let acc = 0;
    this.pre = this.keys.map((k) => { const p = acc; acc += map.get(k) - this.def; return p; });
    this.delta = acc;
  }
  size(i) { const v = this.sizes.get(i); return v === undefined ? this.def : v; }
  /** Where index i (from 1) starts. */
  pos(i) {
    let lo = 0, hi = this.keys.length;
    while (lo < hi) { const m = (lo + hi) >> 1; if (this.keys[m] < i) lo = m + 1; else hi = m; }
    const d = lo === 0 ? 0 : this.pre[lo - 1] + this.sizes.get(this.keys[lo - 1]) - this.def;
    return (i - 1) * this.def + d;
  }
  /** The index at offset x. */
  at(x, max) {
    let lo = 1, hi = max;
    while (lo < hi) { const m = (lo + hi + 1) >> 1; if (this.pos(m) <= x) lo = m; else hi = m - 1; }
    return lo;
  }
  end(n) { return this.pos(n + 1); }
}

// ---------------------------------------------------------------- the grid

/**
 * A grid drawn on a canvas, of rows and columns a source gives:
 *   src.rows(), src.cols()       how many to show (more as one scrolls to the end: src.grow(dr, dc))
 *   src.rowAxis, src.colAxis     their sizes (Axis)
 *   src.frozen()                 {rows, cols}
 *   src.cell(r, c)               {text, st, type, x} or null (st: the style, as the server writes it)
 *   src.need(r0, r1, c0, c1)     asks for the cells of an area; src calls grid.draw() when they come
 *   src.colLabel(c), src.rowLabel(r), src.merges(), src.gridLines(), src.colStyle(c), src.rowStyle(r)
 * hooks: select(sel), edit(r, c, text, how), editStart(r, c) -> text, key(e) -> handled, context(ev, hit),
 *   colWidth(cols, w), rowHeight(rows, h), autofit(col), fill(sel, to), headerClick(hit, ev), dblHeader(c)
 */
class Grid {
  constructor(core, src, hooks = {}) {
    const { h } = core;
    this.core = core; this.src = src; this.hooks = hooks;
    this.canvas = h("canvas");
    this.spacer = h("div", { class: "sx-spacer" });
    this.layer = h("div", { style: { position: "absolute", left: 0, top: 0, width: 0, height: 0 } });
    this.spacer.append(this.layer);
    this.scroll = h("div", { class: "sx-scroll", tabIndex: 0, "aria-label": "Cells", role: "grid" }, this.spacer);
    this.editor = h("textarea", { class: "sx-editor", spellcheck: "false", "aria-label": "Edit the cell" });
    this.el = h("div", { class: "sx-grid" }, this.canvas, this.scroll, this.editor);
    this.sel = { r: 1, c: 1, r0: 1, c0: 1, r1: 1, c1: 1 };
    this.colors = themeColors();
    this.editing = null;
    this.copyArea = null;
    this.marks = [];
    this.HW = 46;
    this.scroll.addEventListener("scroll", () => this.onScroll());
    this.scroll.addEventListener("pointerdown", (e) => this.onDown(e));
    this.scroll.addEventListener("pointermove", (e) => this.onMove(e));
    this.scroll.addEventListener("dblclick", (e) => this.onDbl(e));
    this.scroll.addEventListener("contextmenu", (e) => this.onContext(e));
    this.scroll.addEventListener("keydown", (e) => this.onKey(e));
    this.scroll.addEventListener("wheel", (e) => { if (e.ctrlKey) e.preventDefault(); }, { passive: false });
    this.editor.addEventListener("keydown", (e) => this.onEditorKey(e));
    this.editor.addEventListener("input", () => { this.fitEditor(); this.hooks.editInput?.(this.editor.value, this.editor); });
    this.editor.addEventListener("blur", () => { if (this.editing && !this.pointing) setTimeout(() => { if (this.editing && document.activeElement !== this.editor && !this.hooks.keepEditing?.()) this.commit("blur"); }, 0); });
    this.ro = new ResizeObserver(() => this.resize());
    this.ro.observe(this.el);
    this.offTheme = core.bus?.on ? core.bus.on("theme", () => { this.colors = themeColors(); this.draw(); }) : null;
  }

  destroy() { this.ro.disconnect(); if (typeof this.offTheme === "function") this.offTheme(); cancelAnimationFrame(this.raf); }

  resize() {
    const w = this.el.clientWidth, ht = this.el.clientHeight;
    if (!w || !ht) return;
    const dpr = window.devicePixelRatio || 1;
    this.dpr = dpr;
    this.canvas.width = Math.round(w * dpr); this.canvas.height = Math.round(ht * dpr);
    this.canvas.style.width = w + "px"; this.canvas.style.height = ht + "px";
    this.sizeSpacer();
    this.draw(true);
  }

  sizeSpacer() {
    const s = this.src;
    const digits = String(Math.max(s.rows(), 100)).length;
    this.HW = Math.max(40, 14 + digits * 8);
    this.spacer.style.width = this.HW + s.colAxis.end(s.cols()) + "px";
    this.spacer.style.height = HH + s.rowAxis.end(s.rows()) + "px";
    this.layer.style.left = this.HW + "px";
    this.layer.style.top = HH + "px";
  }

  // where things are on the screen (in CSS pixels, from the grid's top left)
  frozen() { const f = this.src.frozen?.() || { rows: 0, cols: 0 }; return { rows: f.rows || 0, cols: f.cols || 0, fw: this.src.colAxis.end(f.cols || 0), fh: this.src.rowAxis.end(f.rows || 0) }; }
  sx(c, f = this.frozen()) { return this.HW + this.src.colAxis.pos(c) - (c > f.cols ? this.scroll.scrollLeft : 0); }
  sy(r, f = this.frozen()) { return HH + this.src.rowAxis.pos(r) - (r > f.rows ? this.scroll.scrollTop : 0); }
  rect(a, f = this.frozen()) {
    const x0 = this.sx(a.c0, f), y0 = this.sy(a.r0, f);
    const x1 = this.sx(a.c1, f) + this.src.colAxis.size(a.c1), y1 = this.sy(a.r1, f) + this.src.rowAxis.size(a.r1);
    return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
  }
  /** The cell, header or corner at a point of the grid. */
  hit(x, y) {
    const s = this.src, f = this.frozen();
    const col = x < this.HW ? 0 : (x - this.HW < f.fw ? s.colAxis.at(x - this.HW, Math.max(1, f.cols)) : s.colAxis.at(x - this.HW + this.scroll.scrollLeft, MAX_COL));
    const row = y < HH ? 0 : (y - HH < f.fh ? s.rowAxis.at(y - HH, Math.max(1, f.rows)) : s.rowAxis.at(y - HH + this.scroll.scrollTop, MAX_ROW));
    const r = { row, col, x, y };
    if (row === 0 && col > 0) {
      const left = this.sx(col, f), right = left + s.colAxis.size(col);
      if (right - x <= 4) r.colEdge = col; else if (x - left <= 3 && col > 1) r.colEdge = prevVisible(s.colAxis, col);
    }
    if (col === 0 && row > 0) {
      const top = this.sy(row, f), bottom = top + s.rowAxis.size(row);
      if (bottom - y <= 3) r.rowEdge = row; else if (y - top <= 2 && row > 1) r.rowEdge = prevVisible(s.rowAxis, row);
    }
    const sel = this.sel, sr = this.rect(sel, f);
    if (Math.abs(x - (sr.x + sr.w)) <= 5 && Math.abs(y - (sr.y + sr.h)) <= 5 && this.hooks.fill) r.handle = true;
    return r;
  }
  pt(e) { const b = this.el.getBoundingClientRect(); return { x: e.clientX - b.left, y: e.clientY - b.top }; }

  onScroll() {
    const s = this.src, sc = this.scroll;
    if (sc.scrollTop + sc.clientHeight > sc.scrollHeight - 300 && s.rows() < MAX_ROW) { s.grow?.(500, 0); this.sizeSpacer(); }
    if (sc.scrollLeft + sc.clientWidth > sc.scrollWidth - 300 && s.cols() < MAX_COL) { s.grow?.(0, 20); this.sizeSpacer(); }
    if (this.editing) this.placeEditor();
    this.draw();
  }

  /** Draws on the next frame (now with `now`). */
  draw(now) {
    if (now) { cancelAnimationFrame(this.raf); this.raf = 0; this.paint(); return; }
    if (this.raf) return;
    this.raf = requestAnimationFrame(() => { this.raf = 0; this.paint(); });
  }

  visible(f) {
    const s = this.src, sc = this.scroll, W = this.el.clientWidth, H = this.el.clientHeight;
    const c0 = f.cols + 1 <= s.cols() ? s.colAxis.at(f.fw + sc.scrollLeft, MAX_COL) : 1;
    const r0 = f.rows + 1 <= s.rows() ? s.rowAxis.at(f.fh + sc.scrollTop, MAX_ROW) : 1;
    const c1 = Math.min(s.cols(), s.colAxis.at(sc.scrollLeft + W - this.HW, MAX_COL));
    const r1 = Math.min(s.rows(), s.rowAxis.at(sc.scrollTop + H - HH, MAX_ROW));
    return { r0: Math.max(r0, f.rows + 1), r1, c0: Math.max(c0, f.cols + 1), c1 };
  }

  paint() {
    const s = this.src, g = this.canvas.getContext("2d"), K = this.colors;
    const W = this.el.clientWidth, H = this.el.clientHeight;
    if (!W || !H) return;
    g.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    g.fillStyle = K.bg; g.fillRect(0, 0, W, H);
    const f = this.frozen();
    const v = this.visible(f);
    s.need?.(Math.max(1, Math.min(v.r0, f.rows ? 1 : v.r0)), v.r1, Math.max(1, Math.min(v.c0, f.cols ? 1 : v.c0)), v.c1);
    const rowsSets = [], colsSets = [];
    if (f.rows) rowsSets.push([1, f.rows, HH, HH + f.fh]);
    rowsSets.push([v.r0, v.r1, HH + f.fh, H]);
    if (f.cols) colsSets.push([1, f.cols, this.HW, this.HW + f.fw]);
    colsSets.push([v.c0, v.c1, this.HW + f.fw, W]);
    const merges = (s.merges?.() || []);
    for (const [ra, rb, y0, y1] of rowsSets) for (const [ca, cb, x0, x1] of colsSets) {
      if (rb < ra || cb < ca) continue;
      g.save(); g.beginPath(); g.rect(x0, y0, x1 - x0, y1 - y0); g.clip();
      this.paintCells(g, { r0: ra, r1: rb, c0: ca, c1: cb }, f, merges);
      g.restore();
    }
    this.paintMarks(g, f, W, H);
    this.paintHeaders(g, f, v, W, H);
    // the lines of the frozen panes
    if (f.rows) { g.fillStyle = K.line2; g.fillRect(0, HH + f.fh - 1, W, 2); }
    if (f.cols) { g.fillStyle = K.line2; g.fillRect(this.HW + f.fw - 1, 0, 2, H); }
    this.hooks.painted?.(v, f);
  }

  paintCells(g, a, f, merges) {
    const s = this.src, K = this.colors;
    const rows = [], cols = [];
    for (let r = a.r0; r <= a.r1; r++) { const hgt = s.rowAxis.size(r); if (hgt > 0) rows.push([r, this.sy(r, f), hgt]); }
    for (let c = a.c0; c <= a.c1; c++) { const w = s.colAxis.size(c); if (w > 0) cols.push([c, this.sx(c, f), w]); }
    if (!rows.length || !cols.length) return;
    const covered = new Map();
    const mine = [];
    for (const m of merges) {
      const [mr, mc, mw, mh] = m;
      if (mr > a.r1 || mr + mh - 1 < a.r0 || mc > a.c1 || mc + mw - 1 < a.c0) continue;
      mine.push(m);
      for (let r = mr; r < mr + mh; r++) for (let c = mc; c < mc + mw; c++) covered.set(r * 20000 + c, m);
    }
    // fills
    for (const [r, y, hh] of rows) {
      const rs = s.rowStyle?.(r);
      for (const [c, x, w] of cols) {
        const cell = s.cell(r, c);
        const st = cell?.x?.cf || cell?.st || rs || s.colStyle?.(c);
        const bg = (cell?.x?.cf && cell.x.cf.bg) || st?.bg;
        if (bg) { g.fillStyle = bg; g.fillRect(x, y, w, hh); }
      }
    }
    // grid lines
    if (s.gridLines?.() !== false) {
      g.fillStyle = K.line;
      const left = cols[0][1], right = cols[cols.length - 1][1] + cols[cols.length - 1][2];
      const top = rows[0][1], bottom = rows[rows.length - 1][1] + rows[rows.length - 1][2];
      for (const [, y, hh] of rows) g.fillRect(left, y + hh - 1, right - left, 1);
      for (const [, x, w] of cols) g.fillRect(x + w - 1, top, 1, bottom - top);
    }
    // merged cells: one cell, drawn over the lines
    for (const [mr, mc, mw, mh] of mine) {
      const rc = this.rect({ r0: mr, c0: mc, r1: mr + mh - 1, c1: mc + mw - 1 }, f);
      const cell = s.cell(mr, mc);
      g.fillStyle = cell?.st?.bg || K.bg; g.fillRect(rc.x, rc.y, rc.w - 1, rc.h - 1);
      if (cell) this.paintText(g, cell, rc.x, rc.y, rc.w, rc.h, null);
    }
    // text
    for (const [r, y, hh] of rows) {
      for (let i = 0; i < cols.length; i++) {
        const [c, x, w] = cols[i];
        if (covered.has(r * 20000 + c)) continue;
        const cell = s.cell(r, c);
        if (!cell || (cell.text === "" && !cell.x?.bar && !cell.x?.icon)) continue;
        this.paintText(g, cell, x, y, w, hh, { r, i, cols, covered });
      }
    }
    // borders
    for (const [r, y, hh] of rows) {
      for (const [c, x, w] of cols) {
        const st = s.cell(r, c)?.st;
        if (!st || !(st.bt || st.br || st.bb || st.bl)) continue;
        if (st.bt) line(g, x, y, x + w, y, st.bt);
        if (st.bb) line(g, x, y + hh - 1, x + w, y + hh - 1, st.bb);
        if (st.bl) line(g, x, y, x, y + hh, st.bl);
        if (st.br) line(g, x + w - 1, y, x + w - 1, y + hh, st.br);
      }
    }
  }

  paintText(g, cell, x, y, w, hh, over) {
    const K = this.colors;
    const st = { ...(cell.st || {}), ...(cell.x?.cf || {}) };
    const px = Math.round((st.sz || 11) * 1.05 + 1.5);
    const fam = st.ff && !/^(Inter|Calibri|Aptos)$/i.test(st.ff) ? `"${st.ff}", system-ui, sans-serif` : "system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif";
    g.font = `${st.i ? "italic " : ""}${st.b ? "600 " : ""}${px}px ${fam}`;
    const t = cell.type || "s";
    let color = cell.x?.color || st.c || (t[0] === "e" ? K.crit : K.text);
    if (color && !K.dark || !color) { /* keep */ } else if (isDarkInk(color) && !st.bg) color = K.text;
    // automatic text on a light fill, in the dark theme: dark, so it reads
    else if (K.dark && st.bg && !cell.x?.color && !st.c && t[0] !== "e" && luma(st.bg) > 150) color = "#1b1f24";
    let ha = st.ha && st.ha !== "general" ? st.ha : t[0] === "n" ? "right" : t[0] === "b" || t[0] === "e" ? "center" : "left";
    if (ha === "fill" || ha === "justify" || ha === "distributed") ha = "left";
    if (ha === "centerContinuous") ha = "center";
    const pad = 4;
    // a data bar under the text
    const bar = cell.x?.bar;
    if (bar) {
      const v = Math.max(0, Math.min(1, bar.value || 0));
      const axis = Math.max(0, Math.min(1, bar.axis_position || 0));
      const bw = (w - 4) * v, ax = x + 2 + (w - 4) * axis;
      g.fillStyle = (bar.positive_color && typeof bar.positive_color === "string" ? bar.positive_color : "#638ec6") + "aa";
      g.fillRect(ax, y + 3, Math.max(1, bw), hh - 7);
      if (bar.show_value === false) return;
    }
    let left = x + pad;
    const icon = cell.x?.icon;
    if (icon) {
      g.fillStyle = typeof icon.color === "string" ? icon.color : K.muted;
      g.textAlign = "left"; g.textBaseline = "middle";
      g.fillText(ICONS[icon.icon] || "●", left, y + hh / 2);
      left += px + 2;
      if (icon.show_value === false) return;
    }
    const text = cell.text;
    if (!text) return;
    g.fillStyle = color;
    const va = st.va || "bottom";
    // wrapped text: lines within the width
    if (st.wr) {
      const lines = wrapLines(g, text, w - pad * 2);
      const lh = px * 1.25;
      let ty = va === "top" ? y + 3 + lh / 2 : va === "center" ? y + hh / 2 - (lines.length - 1) * lh / 2 : y + hh - 4 - (lines.length - 1) * lh - lh / 2 + 2;
      g.save(); g.beginPath(); g.rect(x, y, w - 1, hh - 1); g.clip();
      g.textBaseline = "middle";
      for (const l of lines) { drawLine(g, l, ha, left, x + w - pad, ty, st); ty += lh; }
      g.restore();
      return;
    }
    const ty = va === "top" ? y + px / 2 + 4 : va === "center" ? y + hh / 2 + 1 : y + hh - 5 - px / 2 + 2;
    g.textBaseline = "middle";
    const tw = g.measureText(text).width;
    let clipL = x, clipR = x + w - 1;
    // text overflows into empty cells beside it, as a spreadsheet's does
    if (over && tw > w - pad * 2 && t[0] !== "n") {
      const { r, i, cols, covered } = over;
      if (ha === "left" || ha === "center") for (let j = i + 1; j < cols.length && clipR - x < tw + pad * 2 + 2; j++) { const [cc, cx, cw] = cols[j]; if (covered.has(r * 20000 + cc) || this.src.cell(r, cc)?.text) break; clipR = cx + cw - 1; }
      if (ha === "right" || ha === "center") for (let j = i - 1; j >= 0 && clipR - clipL < tw + pad * 2 + 2; j--) { const [cc, cx] = cols[j]; if (covered.has(r * 20000 + cc) || this.src.cell(r, cc)?.text) break; clipL = cx; }
      if (clipR > x + w - 1 || clipL < x) { g.fillStyle = cell.st?.bg || K.bg; }
    }
    let shown = text;
    if (t[0] === "n" && tw > w - pad * 2 && !st.wr) {
      // General numbers drop decimals (then go to E notation) to fit, as Excel's do; other formats show ###.
      const fit = (!st.nf || st.nf === "general") ? fitGeneral(text, (v) => g.measureText(v).width <= w - pad * 2) : null;
      shown = fit ?? "#".repeat(Math.max(1, Math.floor((w - pad * 2) / Math.max(4, g.measureText("#").width))));
    }
    g.save(); g.beginPath(); g.rect(clipL, y, clipR - clipL, hh); g.clip();
    g.fillStyle = color;
    const cx0 = ha === "center" ? clipL : left, cx1 = ha === "center" ? clipR : x + w - pad;
    drawLine(g, shown, ha, ha === "center" ? cx0 : cx0, ha === "center" ? cx1 : cx1, ty, st);
    g.restore();
  }

  paintMarks(g, f, W, H) {
    const s = this.src, K = this.colors;
    g.save(); g.beginPath(); g.rect(this.HW, HH, W - this.HW, H - HH); g.clip();
    // other marks: found cells, formula references
    for (const m of this.marks) {
      const rc = this.rect(m, f);
      g.strokeStyle = m.color || K.accent; g.lineWidth = 1.5; g.setLineDash(m.dash ? [4, 3] : []);
      g.fillStyle = (m.color || K.accent) + "18";
      g.fillRect(rc.x, rc.y, rc.w, rc.h);
      g.strokeRect(rc.x + .5, rc.y + .5, rc.w - 2, rc.h - 2);
    }
    g.setLineDash([]);
    const sel = this.sel;
    const big = norm(sel);
    const rc = this.rect(big, f);
    if (big.r0 !== big.r1 || big.c0 !== big.c1) { g.fillStyle = K.sel; g.fillRect(rc.x, rc.y, rc.w, rc.h); }
    // the active cell (a merged one whole)
    const m = this.mergeAt(sel.r, sel.c);
    const ac = this.rect(m || { r0: sel.r, c0: sel.c, r1: sel.r, c1: sel.c }, f);
    g.fillStyle = s.cell(sel.r, sel.c)?.st?.bg || K.bg;
    if (big.r0 !== big.r1 || big.c0 !== big.c1) { g.save(); g.globalCompositeOperation = "destination-over"; g.restore(); }
    g.strokeStyle = K.accent; g.lineWidth = 2;
    g.strokeRect(rc.x + .5, rc.y + .5, rc.w - 1.5, rc.h - 1.5);
    if (big.r0 !== big.r1 || big.c0 !== big.c1) { g.lineWidth = 1; g.strokeRect(ac.x + 1.5, ac.y + 1.5, ac.w - 3.5, ac.h - 3.5); }
    if (this.hooks.fill) { g.fillStyle = K.accent; g.fillRect(rc.x + rc.w - 4, rc.y + rc.h - 4, 7, 7); g.fillStyle = K.bg; g.fillRect(rc.x + rc.w - 4, rc.y + rc.h - 5, 7, 1); g.fillRect(rc.x + rc.w - 5, rc.y + rc.h - 4, 1, 7); }
    if (this.fillTo) { const fr = this.rect(this.fillTo, f); g.setLineDash([4, 3]); g.strokeStyle = K.muted; g.lineWidth = 1.5; g.strokeRect(fr.x + .5, fr.y + .5, fr.w - 1, fr.h - 1); g.setLineDash([]); }
    if (this.copyArea && this.copyArea.sheet === s.sheetKey?.()) {
      const cr = this.rect(this.copyArea, f);
      g.setLineDash([5, 4]); g.lineDashOffset = -((performance.now() / 60) % 9); g.strokeStyle = K.accent; g.lineWidth = 1.5;
      g.strokeRect(cr.x + 1, cr.y + 1, cr.w - 2, cr.h - 2); g.setLineDash([]);
      clearTimeout(this.antsT); this.antsT = setTimeout(() => this.draw(), 120);
    }
    g.restore();
  }

  paintHeaders(g, f, v, W, H) {
    const s = this.src, K = this.colors, sel = norm(this.sel);
    g.font = "12px system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif";
    g.textBaseline = "middle";
    // column headers
    g.fillStyle = K.head; g.fillRect(0, 0, W, HH);
    const colsDrawn = [];
    if (f.cols) for (let c = 1; c <= f.cols; c++) colsDrawn.push(c);
    for (let c = v.c0; c <= v.c1; c++) colsDrawn.push(c);
    for (const c of colsDrawn) {
      const w = s.colAxis.size(c); if (!w) continue;
      const x = this.sx(c, f);
      if (c > f.cols && x + w < this.HW + f.fw) continue;
      const on = c >= sel.c0 && c <= sel.c1;
      if (on) { g.fillStyle = K.headOn; g.fillRect(x, 0, w, HH); }
      g.fillStyle = K.line2; g.fillRect(x + w - 1, 0, 1, HH);
      const label = s.colLabel ? s.colLabel(c) : colName(c);
      g.fillStyle = on ? K.accent : K.text2;
      g.save(); g.beginPath(); g.rect(x, 0, w - 1, HH); g.clip();
      const tag = s.colTag?.(c);
      if (tag) {
        g.textAlign = "left"; g.font = "600 12px system-ui, sans-serif"; g.fillText(label, x + 6, HH / 2);
        const lw = g.measureText(label).width;
        g.font = "10.5px system-ui, sans-serif"; g.fillStyle = K.muted; g.fillText(tag, x + 12 + lw, HH / 2 + 1);
        g.font = "12px system-ui, sans-serif";
      } else { g.textAlign = "center"; g.fillText(label, x + w / 2, HH / 2); }
      const fb = s.filterAt?.(c);
      if (fb) { g.fillStyle = fb === "on" ? K.accent : K.muted; g.textAlign = "right"; g.fillText(fb === "on" ? "▼" : "▾", x + w - 5, HH / 2); }
      const so = s.sortAt?.(c);
      if (so) { g.fillStyle = K.accent; g.textAlign = "right"; g.fillText(so === "desc" ? "↓" : "↑", x + w - 6, HH / 2); }
      g.restore();
    }
    g.fillStyle = K.line2; g.fillRect(0, HH - 1, W, 1);
    // row headers
    g.fillStyle = K.head; g.fillRect(0, HH, this.HW, H - HH);
    const rowsDrawn = [];
    if (f.rows) for (let r = 1; r <= f.rows; r++) rowsDrawn.push(r);
    for (let r = v.r0; r <= v.r1; r++) rowsDrawn.push(r);
    g.textAlign = "center";
    for (const r of rowsDrawn) {
      const hh = s.rowAxis.size(r); if (!hh) continue;
      const y = this.sy(r, f);
      if (r > f.rows && y + hh < HH + f.fh) continue;
      const on = r >= sel.r0 && r <= sel.r1;
      if (on) { g.fillStyle = K.headOn; g.fillRect(0, y, this.HW, hh); }
      g.fillStyle = K.line2; g.fillRect(0, y + hh - 1, this.HW, 1);
      g.fillStyle = on ? K.accent : K.muted;
      g.fillText(s.rowLabel ? s.rowLabel(r) : String(r), this.HW / 2, y + hh / 2);
    }
    g.fillStyle = K.line2; g.fillRect(this.HW - 1, 0, 1, H);
    // the corner
    g.fillStyle = K.head; g.fillRect(0, 0, this.HW - 1, HH - 1);
    g.fillStyle = K.line2; g.beginPath(); g.moveTo(this.HW - 5, HH - 5); g.lineTo(this.HW - 5, HH - 13); g.lineTo(this.HW - 13, HH - 5); g.fill();
    if (this.resizing) { g.fillStyle = K.accent; if (this.resizing.col) g.fillRect(this.resizing.x, 0, 1, H); else g.fillRect(0, this.resizing.y, W, 1); }
  }

  mergeAt(r, c) {
    for (const [mr, mc, mw, mh] of this.src.merges?.() || []) if (r >= mr && r < mr + mh && c >= mc && c < mc + mw) return { r0: mr, c0: mc, r1: mr + mh - 1, c1: mc + mw - 1 };
    return null;
  }

  // ------------------------------------------------------------ selection

  select(r0, c0, r1 = r0, c1 = c0, active) {
    const s = this.src;
    const clampR = (r) => Math.max(1, Math.min(MAX_ROW, r)), clampC = (c) => Math.max(1, Math.min(MAX_COL, c));
    r0 = clampR(r0); r1 = clampR(r1); c0 = clampC(c0); c1 = clampC(c1);
    let a = { r0, c0, r1, c1 };
    // a range takes in the merged cells it touches
    if (s.merges) for (let i = 0; i < 3; i++) { let grew = false; for (const [mr, mc, mw, mh] of s.merges()) { const m = { r0: mr, c0: mc, r1: mr + mh - 1, c1: mc + mw - 1 }; const n = norm(a); if (m.r0 <= n.r1 && m.r1 >= n.r0 && m.c0 <= n.c1 && m.c1 >= n.c0 && (m.r0 < n.r0 || m.r1 > n.r1 || m.c0 < n.c0 || m.c1 > n.c1)) { a = { r0: Math.min(n.r0, m.r0), c0: Math.min(n.c0, m.c0), r1: Math.max(n.r1, m.r1), c1: Math.max(n.c1, m.c1) }; grew = true; } } if (!grew) break; }
    const n = norm(a);
    const act = active || { r: r0, c: c0 };
    this.tabCol = null; // a selection of any kind ends a run of Tabs
    this.sel = { ...n, r: act.r, c: act.c, ar: r0, ac: c0, er: r1, ec: c1 };
    this.draw();
    this.hooks.select?.(this.sel);
  }
  /** Tab and Shift+Tab: the next cell across; Enter after them goes back to the column the Tabs started from. */
  tab(dc) {
    const start = this.tabCol ?? this.sel.c;
    this.move(0, dc, false, false);
    this.tabCol = start;
  }
  /** Enter: down one row (or up, `dr` -1), to the column a run of Tabs started from. */
  enter(dr) {
    if (dr > 0 && this.tabCol != null) { const r = Math.min(MAX_ROW, stepVisible(this.src.rowAxis, this.sel.r, 1)), c = this.tabCol; this.select(r, c); this.reveal(r, c); return; }
    this.move(dr, 0, false, false);
  }
  /** Moves the active cell, or extends the selection, by dr, dc (to the data's edge with `jump`). */
  move(dr, dc, extend, jump) {
    const s = this.src, sel = this.sel;
    let r = extend ? sel.er ?? sel.r : sel.r, c = extend ? sel.ec ?? sel.c : sel.c;
    if (!extend) { const m = this.mergeAt(r, c); if (m) { if (dr > 0) r = m.r1; if (dc > 0) c = m.c1; } }
    if (jump && s.jump) [r, c] = s.jump(r, c, dr, dc);
    else { r = stepVisible(s.rowAxis, r, dr); c = stepVisible(s.colAxis, c, dc); }
    r = Math.max(1, Math.min(MAX_ROW, r)); c = Math.max(1, Math.min(MAX_COL, c));
    if (extend) this.select(sel.ar ?? sel.r, sel.ac ?? sel.c, r, c, { r: sel.r, c: sel.c });
    else { const m = this.mergeAt(r, c); this.select(m ? m.r0 : r, m ? m.c0 : c, m ? m.r1 : r, m ? m.c1 : c, { r: m ? m.r0 : r, c: m ? m.c0 : c }); }
    this.reveal(r, c);
  }
  /** Scrolls so that the cell shows. */
  reveal(r, c) {
    const s = this.src, f = this.frozen(), sc = this.scroll;
    const W = this.el.clientWidth - this.HW - f.fw, H = this.el.clientHeight - HH - f.fh;
    if (c > f.cols) {
      const x = s.colAxis.pos(c) - f.fw, w = s.colAxis.size(c);
      if (x < sc.scrollLeft) sc.scrollLeft = x; else if (x + w > sc.scrollLeft + W - 14) sc.scrollLeft = x + w - W + 14;
    }
    if (r > f.rows) {
      if (r > s.rows() - 20) { s.grow?.(Math.max(500, r - s.rows() + 100), 0); this.sizeSpacer(); }
      const y = s.rowAxis.pos(r) - f.fh, hh = s.rowAxis.size(r);
      if (y < sc.scrollTop) sc.scrollTop = y; else if (y + hh > sc.scrollTop + H - 14) sc.scrollTop = y + hh - H + 14;
    }
    this.draw();
  }

  // ------------------------------------------------------------ the mouse

  onDown(e) {
    if (e.button === 2) {
      const p = this.pt(e), ht = this.hit(p.x, p.y);
      const sel = this.sel;
      // a right click outside the selection selects what it is on
      if (ht.row && ht.col && !(ht.row >= sel.r0 && ht.row <= sel.r1 && ht.col >= sel.c0 && ht.col <= sel.c1)) this.select(ht.row, ht.col);
      else if (!ht.row && ht.col && !(sel.r0 === 1 && sel.r1 >= MAX_ROW && ht.col >= sel.c0 && ht.col <= sel.c1)) this.select(1, ht.col, MAX_ROW, ht.col, { r: Math.max(1, this.visible(this.frozen()).r0), c: ht.col });
      else if (ht.row && !ht.col && !(sel.c0 === 1 && sel.c1 >= MAX_COL && ht.row >= sel.r0 && ht.row <= sel.r1)) this.select(ht.row, 1, ht.row, MAX_COL, { r: ht.row, c: 1 });
      return;
    }
    if (e.button !== 0) return;
    const p = this.pt(e);
    const sb = this.scroll.getBoundingClientRect();
    if (e.clientX > sb.left + this.scroll.clientWidth || e.clientY > sb.top + this.scroll.clientHeight) return; // the scroll bars
    const ht = this.hit(p.x, p.y);
    // a reference typed into a formula: the cell clicked goes into it
    if (this.editing && this.hooks.pointable?.() && ht.row && ht.col) {
      e.preventDefault();
      this.pointing = { r: ht.row, c: ht.col };
      this.hooks.point?.({ r0: ht.row, c0: ht.col, r1: ht.row, c1: ht.col }, true);
      this.drag(e, (q) => { const t = this.hit(q.x, q.y); if (t.row && t.col) this.hooks.point?.(norm({ r0: this.pointing.r, c0: this.pointing.c, r1: t.row, c1: t.col }), false); }, () => { this.pointing = null; this.editor.focus(); });
      return;
    }
    if (this.editing) this.commit("click");
    this.scroll.focus({ preventScroll: true });
    if (ht.colEdge || ht.rowEdge) { this.startResize(e, ht); return; }
    if (ht.handle) { this.startFill(e); return; }
    if (ht.row === 0 && ht.col === 0) { this.select(1, 1, MAX_ROW, MAX_COL, { r: 1, c: 1 }); return; }
    if (ht.row === 0) {
      if (this.hooks.headerClick?.(ht, e)) return;
      const start = e.shiftKey ? this.sel.ac ?? ht.col : ht.col;
      const vr = Math.max(this.frozen().rows + 1, this.visible(this.frozen()).r0);
      this.select(1, start, MAX_ROW, ht.col, { r: e.shiftKey ? this.sel.r : vr, c: start });
      this.drag(e, (q) => { const t = this.hit(q.x, Math.max(HH + 1, q.y)); if (t.col) this.select(1, start, MAX_ROW, t.col, { r: this.sel.r, c: start }); });
      return;
    }
    if (ht.col === 0) {
      const start = e.shiftKey ? this.sel.ar ?? ht.row : ht.row;
      this.select(start, 1, ht.row, MAX_COL, { r: start, c: 1 });
      this.drag(e, (q) => { const t = this.hit(Math.max(this.HW + 1, q.x), q.y); if (t.row) this.select(start, 1, t.row, MAX_COL, { r: start, c: 1 }); });
      return;
    }
    if (this.hooks.cellClick?.(ht, e)) return;
    if (e.shiftKey) this.select(this.sel.ar ?? this.sel.r, this.sel.ac ?? this.sel.c, ht.row, ht.col, { r: this.sel.r, c: this.sel.c });
    else { const m = this.mergeAt(ht.row, ht.col); if (m) this.select(m.r0, m.c0, m.r1, m.c1, { r: m.r0, c: m.c0 }); else this.select(ht.row, ht.col); }
    const ar = this.sel.ar, ac = this.sel.ac, act = { r: this.sel.r, c: this.sel.c };
    this.drag(e, (q) => { const t = this.hit(Math.max(this.HW + 1, q.x), Math.max(HH + 1, q.y)); if (t.row && t.col) this.select(ar, ac, t.row, t.col, act); });
  }

  /** Follows a drag: `step(point)` as it moves (scrolling at the edges), `done()` when it ends. */
  drag(e, step, done) {
    const id = e.pointerId;
    try { this.scroll.setPointerCapture(id); } catch { /* fine */ }
    let last = this.pt(e), timer = null;
    const edge = () => {
      const W = this.el.clientWidth, H = this.el.clientHeight;
      let dx = 0, dy = 0;
      if (last.x > W - 20) dx = 24; else if (last.x < this.HW + 4 && last.x >= 0) dx = -24;
      if (last.y > H - 20) dy = 24; else if (last.y < HH + 4) dy = -24;
      if (dx || dy) { this.scroll.scrollLeft += dx; this.scroll.scrollTop += dy; step(last); }
    };
    const mv = (ev) => { if (ev.pointerId !== id) return; last = this.pt(ev); step(last); };
    const up = (ev) => { if (ev.pointerId !== id) return; clearInterval(timer); this.scroll.removeEventListener("pointermove", mv); this.scroll.removeEventListener("pointerup", up); this.scroll.removeEventListener("pointercancel", up); done?.(last); };
    timer = setInterval(edge, 50);
    this.scroll.addEventListener("pointermove", mv);
    this.scroll.addEventListener("pointerup", up);
    this.scroll.addEventListener("pointercancel", up);
  }

  startResize(e, ht) {
    const s = this.src, f = this.frozen();
    if (ht.colEdge) {
      const c = ht.colEdge, x0 = this.sx(c, f);
      this.resizing = { col: c, x: x0 + s.colAxis.size(c) };
      this.drag(e, (q) => { this.resizing.x = Math.max(x0 + 8, q.x); this.draw(); }, () => {
        const w = Math.round(this.resizing.x - x0); this.resizing = null; this.draw();
        const sel = this.sel; const cols = sel.r0 === 1 && sel.r1 >= MAX_ROW && c >= sel.c0 && c <= sel.c1 ? [sel.c0, sel.c1] : [c, c];
        this.hooks.colWidth?.(cols, w);
      });
    } else {
      const r = ht.rowEdge, y0 = this.sy(r, f);
      this.resizing = { row: r, y: y0 + s.rowAxis.size(r) };
      this.drag(e, (q) => { this.resizing.y = Math.max(y0 + 6, q.y); this.draw(); }, () => {
        const hh = Math.round(this.resizing.y - y0); this.resizing = null; this.draw();
        const sel = this.sel; const rows = sel.c0 === 1 && sel.c1 >= MAX_COL && r >= sel.r0 && r <= sel.r1 ? [sel.r0, sel.r1] : [r, r];
        this.hooks.rowHeight?.(rows, hh);
      });
    }
  }

  startFill(e) {
    const base = norm(this.sel);
    this.drag(e, (q) => {
      const t = this.hit(Math.max(this.HW + 1, q.x), Math.max(HH + 1, q.y));
      if (!t.row || !t.col) return;
      const down = t.row - base.r1, up = base.r0 - t.row, right = t.col - base.c1, left = base.c0 - t.col;
      const v = Math.max(down, up, 0), hz = Math.max(right, left, 0);
      if (v === 0 && hz === 0) this.fillTo = null;
      else if (v >= hz) this.fillTo = down > 0 ? { ...base, r1: t.row } : { ...base, r0: t.row };
      else this.fillTo = right > 0 ? { ...base, c1: t.col } : { ...base, c0: t.col };
      this.draw();
    }, () => {
      const to = this.fillTo; this.fillTo = null; this.draw();
      if (to) { this.hooks.fill?.(base, to); this.select(to.r0, to.c0, to.r1, to.c1, { r: base.r0, c: base.c0 }); }
    });
  }

  onMove(e) {
    if (e.buttons) return;
    const p = this.pt(e), ht = this.hit(p.x, p.y);
    this.scroll.style.cursor = ht.colEdge ? "col-resize" : ht.rowEdge ? "row-resize" : ht.handle ? "crosshair" : ht.row === 0 || ht.col === 0 ? "default" : "cell";
    this.hooks.hover?.(ht, e);
  }

  onDbl(e) {
    const p = this.pt(e), ht = this.hit(p.x, p.y);
    if (ht.colEdge) { this.hooks.autofit?.(ht.colEdge); return; }
    if (ht.row === 0 && ht.col) { this.hooks.dblHeader?.(ht.col); return; }
    if (ht.handle && this.hooks.fillDown) { this.hooks.fillDown(norm(this.sel)); return; }
    if (ht.row && ht.col) this.startEdit(null);
  }

  onContext(e) {
    e.preventDefault();
    const p = this.pt(e), ht = this.hit(p.x, p.y);
    this.hooks.context?.(e, ht);
  }

  // ------------------------------------------------------------ the keyboard

  onKey(e) {
    if (this.editing) return;
    if (this.hooks.key?.(e)) return;
    const k = e.key, ext = e.shiftKey, jump = mod(e);
    const page = Math.max(1, Math.floor((this.el.clientHeight - HH) / this.src.rowAxis.def) - 2);
    const go = (dr, dc, x = ext, j = jump) => { e.preventDefault(); this.move(dr, dc, x, j); };
    if (k === "ArrowDown") return go(1, 0);
    if (k === "ArrowUp") return go(-1, 0);
    if (k === "ArrowRight") return go(0, 1);
    if (k === "ArrowLeft") return go(0, -1);
    if (k === "PageDown") return go(page, 0, ext, false);
    if (k === "PageUp") return go(-page, 0, ext, false);
    if (k === "Home") { e.preventDefault(); if (jump) { this.select(1, 1); this.reveal(1, 1); } else { this.select(this.sel.r, 1); this.reveal(this.sel.r, 1); } return; }
    if (k === "Tab") { e.preventDefault(); this.tab(ext ? -1 : 1); return; }
    if (k === "Enter") { e.preventDefault(); this.enter(ext ? -1 : 1); return; }
    if (k === "F2") { e.preventDefault(); this.startEdit(null); return; }
    if (k === "Escape") { if (this.copyArea) { this.copyArea = null; this.draw(); } return; }
    if (jump && k.toLowerCase() === "a") { e.preventDefault(); this.select(1, 1, MAX_ROW, MAX_COL, { r: this.sel.r, c: this.sel.c }); return; }
    if (k.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) { e.preventDefault(); this.startEdit(k); }
  }

  // ------------------------------------------------------------ editing

  /** Opens the editor on the active cell: with `typed`, it holds that alone (the cell is replaced). */
  startEdit(typed) {
    if (this.hooks.readonly?.()) return;
    const { r, c } = this.sel;
    const text = typed != null ? typed : this.hooks.editStart ? this.hooks.editStart(r, c) : "";
    if (text === false) return;
    this.editing = { r, c, start: text, typed: typed != null };
    this.editor.value = text;
    const st = this.src.cell(r, c)?.st || {};
    const px = Math.round((st.sz || 11) * 1.05 + 1.5);
    this.editor.style.font = `${st.i ? "italic " : ""}${st.b ? "600 " : ""}${px}px system-ui, sans-serif`;
    this.editor.style.textAlign = st.ha === "center" ? "center" : st.ha === "right" ? "right" : "left";
    this.editor.style.display = "block";
    this.placeEditor();
    this.editor.focus();
    const n = this.editor.value.length; this.editor.setSelectionRange(n, n);
    this.hooks.editInput?.(this.editor.value, this.editor);
  }
  placeEditor() {
    if (!this.editing) return;
    const { r, c } = this.editing;
    const m = this.mergeAt(r, c);
    const rc = this.rect(m || { r0: r, c0: c, r1: r, c1: c });
    this.editor.style.left = rc.x - 1 + "px"; this.editor.style.top = rc.y - 1 + "px";
    this.editor.style.minWidth = rc.w + 1 + "px"; this.editor.style.minHeight = rc.h + 1 + "px";
    this.fitEditor();
  }
  fitEditor() {
    const e = this.editor;
    e.style.width = "0px"; e.style.height = "0px";
    const maxW = this.el.clientWidth - parseFloat(e.style.left || 0) - 8;
    e.style.width = Math.min(maxW, Math.max(parseFloat(e.style.minWidth) || 60, e.scrollWidth + 12)) + "px";
    e.style.height = Math.max(parseFloat(e.style.minHeight) || 24, e.scrollHeight + 2) + "px";
  }
  onEditorKey(e) {
    if (this.hooks.editorKey?.(e)) return;
    const k = e.key;
    if (k === "Escape") { e.preventDefault(); this.cancel(); return; }
    if (k === "Enter" && e.altKey) { e.preventDefault(); const t = this.editor; const p = t.selectionStart; t.value = t.value.slice(0, p) + "\n" + t.value.slice(t.selectionEnd); t.setSelectionRange(p + 1, p + 1); this.fitEditor(); return; }
    if (k === "Enter") { e.preventDefault(); this.commit(mod(e) && e.shiftKey ? "array" : mod(e) ? "all" : e.shiftKey ? "up" : "down"); return; }
    if (k === "Tab") { e.preventDefault(); this.commit(e.shiftKey ? "left" : "right"); return; }
    // arrows leave a value typed fresh (not a formula), as a spreadsheet's do
    if (this.editing?.typed && !this.editor.value.startsWith("=") && ["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight"].includes(k)) {
      e.preventDefault(); this.commit({ ArrowDown: "down", ArrowUp: "up", ArrowLeft: "left", ArrowRight: "right" }[k]);
    }
  }
  /** Ends the edit, sending what was typed: `how` says where the selection goes. */
  commit(how, value) {
    const ed = this.editing; if (!ed) return;
    const text = value ?? this.editor.value;
    this.editing = null;
    this.editor.style.display = "none";
    this.hooks.editEnd?.();
    if (text !== ed.start || ed.typed || how === "all" || how === "array") this.hooks.edit?.(ed.r, ed.c, text, how);
    if (how !== "blur") this.scroll.focus({ preventScroll: true });
    if (how === "down") this.enter(1);
    else if (how === "up") this.move(-1, 0, false, false);
    else if (how === "right") this.tab(1);
    else if (how === "left") this.tab(-1);
    this.draw();
  }
  cancel() {
    if (!this.editing) return;
    this.editing = null;
    this.editor.style.display = "none";
    this.hooks.editEnd?.();
    this.scroll.focus({ preventScroll: true });
    this.draw();
  }
}

/** A General-format number's text shortened to fit (`fits(text)`), or null. */
function fitGeneral(text, fits) {
  if (!/^-?\d+(\.\d+)?(E[+-]?\d+)?$/i.test(text)) return null;
  const n = Number(text);
  if (!Number.isFinite(n)) return null;
  const dot = text.indexOf("."), dec = dot < 0 || /e/i.test(text) ? 0 : text.length - dot - 1;
  if (Math.abs(n) >= 1e-3 || n === 0) for (let d = dec - 1; d >= 0; d--) { const v = String(Number(n.toFixed(d))); if (fits(v)) return v; }
  for (let p = 5; p >= 0; p--) { const [m, e] = n.toExponential(p).split("e"); const v = `${m}E${e[0] === "-" ? "-" : "+"}${e.replace(/^[+-]/, "").padStart(2, "0")}`; if (fits(v)) return v; }
  return null;
}

const ICONS = { ArrowUp: "▲", ArrowDown: "▼", ArrowRight: "▶", ArrowAngleUp: "◥", ArrowAngleDown: "◢", Circle: "●", TriangleUp: "▲", TriangleDown: "▼", TriangleUpFilled: "▲", TriangleDownFilled: "▼", FlatRectangle: "▬", Rhombus: "◆", Flag: "⚑", Check: "✔", Cross: "✖", Exclamation: "!", Star: "★", Heart: "♥", ThumbsUp: "👍", ThumbsDown: "👎" };

function isDarkInk(c) { return luma(c) < 60; }
/** A #rrggbb color's brightness, 0 to 255 (128 when it is not one). */
function luma(c) {
  const m = /^#?([0-9a-f]{6})$/i.exec(c || ""); if (!m) return 128;
  const n = parseInt(m[1], 16), r = n >> 16, g = (n >> 8) & 255, b = n & 255;
  return 0.299 * r + 0.587 * g + 0.114 * b;
}

function prevVisible(axis, i) { let j = i - 1; while (j > 1 && axis.size(j) === 0) j--; return Math.max(1, j); }
function stepVisible(axis, i, d) {
  if (!d) return i;
  let j = i, n = Math.abs(d), s = Math.sign(d);
  while (n > 0) { j += s; if (j < 1) return 1; if (axis.size(j) > 0) n--; if (j > MAX_ROW) break; }
  return j;
}

function line(g, x0, y0, x1, y1, b) {
  const [style, color] = Array.isArray(b) ? b : ["thin", "#000"];
  g.save();
  g.strokeStyle = color || "#000";
  g.lineWidth = style === "medium" || style === "mediumdashed" ? 2 : style === "thick" ? 3 : 1;
  if (/dash/.test(style)) g.setLineDash([4, 2]); else if (/dot/.test(style)) g.setLineDash([1, 2]);
  g.beginPath();
  if (style === "double") { g.moveTo(x0, y0 - 1); g.lineTo(x1, y1 - 1); g.moveTo(x0, y0 + 1); g.lineTo(x1, y1 + 1); }
  else { g.moveTo(x0 + .5, y0 + .5); g.lineTo(x1 + .5, y1 + .5); }
  g.stroke();
  g.restore();
}

function drawLine(g, text, ha, x0, x1, y, st) {
  g.textAlign = ha === "right" ? "right" : ha === "center" ? "center" : "left";
  const x = ha === "right" ? x1 : ha === "center" ? (x0 + x1) / 2 : x0;
  g.fillText(text, x, y);
  if (st.u || st.st) {
    const w = g.measureText(text).width, lx = ha === "right" ? x - w : ha === "center" ? x - w / 2 : x;
    g.fillRect(lx, st.u ? y + 6 : y, w, 1);
  }
}

function wrapLines(g, text, width) {
  const out = [];
  for (const para of String(text).split("\n")) {
    let cur = "";
    for (const word of para.split(/(\s+)/)) {
      const t = cur + word;
      if (cur && g.measureText(t).width > width) { out.push(cur.trimEnd()); cur = word.trimStart(); }
      else cur = t;
    }
    out.push(cur);
  }
  return out;
}

// ---------------------------------------------------------------- the page

function render(el, ctx) {
  if (typeof ctx.core?.fmt?.num === "function") { sharedNum = ctx.core.fmt.num; sharedTick = ctx.core.fmt.tick || null; }
  const { h } = ctx.core;
  el.append(h("style", {}, CSS));
  const root = h("div", { style: { height: "100%" } });
  el.append(root);
  let stop = null;
  const page = el;
  const show = (path) => {
    try { stop?.(); } catch (e) { console.warn(e); }
    stop = null;
    root.replaceChildren();
    const full = path[0] === "b" || path[0] === "t";
    // a workbook takes the whole page, as a spreadsheet does
    page.style.padding = full ? "0" : "";
    page.style.maxWidth = full ? "none" : "";
    page.style.height = full ? "100%" : "";
    if (path[0] === "b" && path[1]) stop = bookPage(root, ctx, path[1], path.slice(2));
    else if (path[0] === "t" && path[1]) stop = tablePage(root, ctx, path[1]);
    else stop = homePage(root, ctx);
  };
  ctx.onPath(show);
  show(ctx.path());
  return () => { try { stop?.(); } catch (e) { console.warn(e); } page.style.padding = ""; page.style.maxWidth = ""; page.style.height = ""; };
}

// ---------------------------------------------------------------- a workbook

/** What the grid draws of a workbook's sheet: its layout, and the cells read so far. */
class BookSource {
  constructor(st) { this.st = st; }
  get s() { return this.st.sheets.get(this.st.sheet) || this.st.blank; }
  get rowAxis() { return this.s.rowAxis; }
  get colAxis() { return this.s.colAxis; }
  rows() { return this.s.rows; }
  cols() { return this.s.cols; }
  grow(dr, dc) { const s = this.s; s.rows = Math.min(MAX_ROW, s.rows + dr); s.cols = Math.min(MAX_COL, s.cols + dc); }
  frozen() { const l = this.s.layout; return l ? { rows: l.frozen_rows, cols: l.frozen_cols } : { rows: 0, cols: 0 }; }
  gridLines() { return this.s.layout ? this.s.layout.grid_lines !== false : true; }
  merges() { return this.s.layout?.merges || []; }
  cell(r, c) { return this.s.cells.get(r * 20000 + c) || null; }
  colStyle(c) { const cs = this.s.colStyles; if (!cs.length) return null; for (const [a, b, st] of cs) if (c >= a && c <= b) return st; return null; }
  rowStyle(r) { return this.s.rowStyles.get(r) || null; }
  need(r0, r1, c0, c1) { this.st.need(r0, r1, c0, c1); }
  sheetKey() { return this.st.sheet; }
  filterAt(c) { const f = this.st.filterOf(this.st.sheet); if (!f) return null; return c >= f.c0 && c <= f.c1 ? (f.criteria.has(c) ? "on" : "off") : null; }
  /** Ctrl+arrow: to the edge of the data (of the cells read; the sheet's used size beyond). */
  jump(r, c, dr, dc) {
    const s = this.s, full = (rr, cc) => !!this.cell(rr, cc)?.text;
    const maxR = Math.max(1, s.layout?.rows_used || 1), maxC = Math.max(1, s.layout?.cols_used || 1);
    const step = (rr, cc) => [rr + dr, cc + dc];
    const inside = (rr, cc) => rr >= 1 && cc >= 1 && rr <= Math.max(maxR, r) && cc <= Math.max(maxC, c);
    let [nr, nc] = step(r, c);
    if (!inside(nr, nc)) return [dr > 0 ? Math.max(maxR, r) : dr < 0 ? 1 : r, dc > 0 ? Math.max(maxC, c) : dc < 0 ? 1 : c];
    if (full(r, c) && full(nr, nc)) { while (inside(nr + dr, nc + dc) && full(nr + dr, nc + dc)) { nr += dr; nc += dc; } return [nr, nc]; }
    while (inside(nr, nc) && !full(nr, nc)) { nr += dr; nc += dc; }
    if (!inside(nr, nc)) return [dr > 0 ? Math.max(maxR, r) : dr < 0 ? 1 : r, dc > 0 ? Math.max(maxC, c) : dc < 0 ? 1 : c];
    return [nr, nc];
  }
}

function bookPage(root, ctx, id, rest) {
  const core = ctx.core, { h, icon, toast } = core;
  let gone = false;
  const stops = [];
  const st = {
    id, book: null, sheet: 0, version: 0, epoch: 0, styles: new Map(), sheets: new Map(), clip: null, busy: 0, error: null,
    blank: blankSheet(),
  };
  const src = new BookSource(st);
  /** The filter of sheet i, as the book keeps it: its area and, by column, the values shown. */
  st.filterOf = (i) => {
    const sid = st.book?.sheets[i]?.id;
    const f = (st.book?.filters || []).find((x) => x.sheet === sid);
    return f ? { r0: f.row, c0: f.col, r1: f.row + f.height - 1, c1: f.col + f.width - 1, criteria: new Map((f.criteria || []).map((c) => [c.col, c.values])) } : null;
  };
  const curFilter = () => st.filterOf(st.sheet);
  const filterOp = (f, criteria) => ({ op: "filter", sheet: st.sheet, area: rect(f), header: true, criteria: [...criteria.entries()].map(([col, values]) => ({ col, values })) });
  const api = (p, o) => ctx.api(`/v1/books/${encodeURIComponent(id)}${p}`, o);

  // ------------------------------------------------------------ the frame of the page
  const nameIn = h("input", { class: "sx-name", "aria-label": "The workbook's name", spellcheck: "false" });
  const saved = h("span", { class: "sx-saved" }, "Opening…");
  const undoB = tb(core, G.undo, "Undo (Ctrl+Z)", () => send([{ op: "undo" }]));
  const redoB = tb(core, G.redo, "Redo (Ctrl+Y)", () => send([{ op: "redo" }]));
  const top = h("div", { class: "sx-top" },
    h("a", { class: "btn ghost sm", href: ctx.href(), title: "All workbooks" }, icon("left"), "Sheets"),
    nameIn, saved, h("span", { class: "sep" }),
    h("button", { class: "btn sm", title: "Find and replace (Ctrl+F)", onclick: () => findPanel() }, svgIcon(core, G.find), "Find"),
    h("button", { class: "btn sm", title: "SQL queries whose answers fill cells", onclick: () => queriesPanel() }, svgIcon(core, G.sql), "Queries"),
    h("button", { class: "btn sm", title: "Download", onclick: (e) => core.menu(e.currentTarget, [
      { label: "Excel workbook (.xlsx)", icon: "download", onclick: () => download("xlsx") },
      { label: "This sheet as CSV", icon: "download", onclick: () => download("csv") },
    ]) }, icon("download"), "Export"),
    h("button", { class: "btn sm", title: "More", "aria-label": "More", onclick: (e) => core.menu(e.currentTarget, [
      { label: "Save this sheet as a ChloeDB table", icon: "db", onclick: () => toTableDialog() },
      { label: "Run every query again", icon: "refresh", onclick: () => refreshQueries() },
      { label: "Named ranges", icon: "sliders", onclick: () => namesDialog() },
      { label: "Keyboard shortcuts", icon: "keyboard", onclick: () => shortcuts(core) },
      "-",
      { label: "Delete this workbook", icon: "trash", danger: true, onclick: () => deleteBook() },
    ]) }, icon("dots")));
  const tools = h("div", { class: "sx-tools", role: "toolbar", "aria-label": "Formatting" });
  const refIn = h("input", { class: "sx-ref", "aria-label": "The selected cells (type a reference or a name to go there)", spellcheck: "false" });
  const fx = h("textarea", { "aria-label": "The cell's contents", spellcheck: "false", rows: 1 });
  const fbar = h("div", { class: "sx-fbar" }, refIn, h("div", { class: "sx-fx" }, "fx"), fx);
  const tabs = h("div", { class: "sx-tabs", role: "tablist" });
  const stats = h("div", { class: "sx-stats", "aria-live": "polite" });
  const foot = h("div", { class: "sx-foot" },
    h("button", { class: "sx-tab", title: "Add a sheet", "aria-label": "Add a sheet", onclick: () => addSheet() }, icon("plus")),
    h("button", { class: "sx-tab", title: "All sheets", "aria-label": "All sheets", onclick: (e) => sheetsMenu(e.currentTarget) }, icon("menu")),
    tabs, stats);
  const grid = new Grid(core, src, {});
  const bookEl = h("div", { class: "sx-book" }, top, tools, fbar, grid.el, foot);
  root.append(bookEl);
  const charts = new ChartLayer(core, grid, st, (p, o) => api(p, o), (ops) => send(ops));
  stops.push(() => grid.destroy(), () => charts.destroy());

  // ------------------------------------------------------------ talking to chloedb-sheets
  let chain = Promise.resolve();
  let inflight = 0;
  function saving(on) {
    inflight += on ? 1 : -1;
    paintSaved();
  }
  function paintSaved() {
    const b = st.book;
    saved.classList.toggle("bad", !!b?.unsaved);
    saved.textContent = inflight ? "Saving…" : b?.unsaved ? "Not saved yet: ChloeDB did not answer (it will be saved again)" : b ? "Saved in ChloeDB" : "Opening…";
    saved.title = b?.unsaved || "Every change is written to ChloeDB as you make it";
  }
  /** Sends operations, in order after those sent before, and takes in the answer. */
  function send(ops, { quiet = false } = {}) {
    const p = chain.then(async () => {
      if (gone) return null;
      saving(true);
      try {
        const j = await api("/ops", { body: { ops }, timeout: 300000 });
        take(j);
        if (j.error && !quiet) toast(j.error, "bad");
        return j;
      } catch (e) { if (!quiet) toast(e.message, "bad"); throw e; } finally { saving(false); }
    });
    chain = p.catch(() => {});
    return p;
  }
  /** An answer of /ops, /changes or /query: the book, and the cells that changed (or the sheets to draw again). */
  function take(j) {
    if (!j || gone) return;
    if (j.version != null) st.version = Math.max(st.version, j.version);
    if (j.book) setBook(j.book);
    if (j.layout) {
      st.epoch++;
      // the cells drawn stay until the new ones come (no flash); tiles are read again
      for (const s of st.sheets.values()) { s.stale = true; s.tiles.clear(); s.pending.clear(); s.old = true; }
      loadLayout(st.sheet);
    } else if (j.cells?.sheets) {
      st.epoch++;
      putStyles(j.cells.styles);
      for (const part of j.cells.sheets) {
        const s = st.sheets.get(part.sheet);
        if (!s) continue;
        for (const c of part.cells) putCell(s, c);
      }
    }
    grid.draw();
    showActive();
    charts.refresh();
    statsSoon();
  }
  function setBook(b) {
    const before = st.book;
    st.book = b;
    if (document.activeElement !== nameIn) nameIn.value = b.name;
    undoB.disabled = !b.can_undo; redoB.disabled = !b.can_redo;
    if (st.sheet >= b.sheets.length) st.sheet = Math.max(0, b.sheets.length - 1);
    // sheets whose layout changed are read again
    for (const sh of b.sheets) {
      const s = st.sheets.get(sh.index);
      if (s?.layout && (s.layout.layout !== sh.layout || s.layout.id !== sh.id)) { s.stale = true; if (s.layout.id !== sh.id) { s.cells.clear(); s.tiles.clear(); } }
    }
    for (const k of [...st.sheets.keys()]) if (k >= b.sheets.length) st.sheets.delete(k);
    const s = st.sheets.get(st.sheet);
    if (!s || s.stale) loadLayout(st.sheet);
    if (!before || JSON.stringify(before.sheets.map((x) => [x.name, x.color, x.state, x.id])) !== JSON.stringify(b.sheets.map((x) => [x.name, x.color, x.state, x.id]))) drawTabs();
    else markTab();
    paintSaved();
  }
  function putStyles(styles) { for (const [k, v] of Object.entries(styles || {})) st.styles.set(+k, v); }
  function putCell(s, c) {
    const key = c[0] * 20000 + c[1];
    if (c[2] == null) { s.cells.delete(key); return; }
    s.cells.set(key, { text: c[2], type: c[3], si: c[4], st: st.styles.get(c[4]) || null, input: c[5], x: c[6] });
  }

  async function loadLayout(sheet) {
    let s = st.sheets.get(sheet);
    if (!s) { s = blankSheet(); st.sheets.set(sheet, s); }
    if (s.loading) return s.loading;
    s.loading = (async () => {
      try {
        const l = await api(`/layout?sheet=${sheet}`, { timeout: 30000 });
        if (gone) return;
        putStyles(l.styles);
        const wasId = s.layout?.id;
        s.layout = l; s.stale = false;
        if (wasId != null && wasId !== l.id) { s.cells.clear(); s.tiles.clear(); }
        const cols = new Map(), colStyles = [];
        for (const c of l.cols) {
          if (c.style != null) colStyles.push([c.min, c.max, st.styles.get(c.style) || null]);
          if (c.hidden || Math.abs(c.width - l.default_width) > 0.01) for (let i = c.min; i <= Math.min(c.max, MAX_COL); i++) cols.set(i, c.hidden ? 0 : Math.round(c.width));
        }
        const rows = new Map(), rowStyles = new Map();
        for (const [r, ht, hidden, style] of l.rows) {
          if (hidden) rows.set(r, 0); else if (ht > 0 && Math.abs(ht - l.default_height) > 0.01) rows.set(r, Math.round(ht));
          if (style != null && st.styles.get(style)) rowStyles.set(r, st.styles.get(style));
        }
        s.colAxis.set(cols); s.rowAxis.set(rows); s.colStyles = colStyles; s.rowStyles = rowStyles;
        s.rows = Math.max(s.rows, l.rows_used + 100, 200, (l.frozen_rows || 0) + 60);
        s.cols = Math.max(s.cols, l.cols_used + 8, 26, (l.frozen_cols || 0) + 10);
        if (sheet === st.sheet) { grid.sizeSpacer(); grid.draw(); charts.refresh(true); }
      } catch (e) { if (!gone) toast(`Sheet ${sheet + 1}: ${e.message}`, "bad"); }
      finally { s.loading = null; }
    })();
    return s.loading;
  }

  /** Reads the cells of an area of the sheet shown, by tiles not read yet. */
  st.need = (r0, r1, c0, c1) => {
    const s = st.sheets.get(st.sheet);
    if (!s || !s.layout) return;
    const t0 = Math.floor((r0 - 1) / TR), t1 = Math.floor((r1 - 1) / TR), u0 = Math.floor((c0 - 1) / TC), u1 = Math.floor((c1 - 1) / TC);
    let a = null;
    for (let t = t0; t <= t1; t++) for (let u = u0; u <= u1; u++) {
      const k = t * 1000 + u;
      if (s.tiles.has(k) || s.pending.has(k)) continue;
      a = a ? { t0: Math.min(a.t0, t), t1: Math.max(a.t1, t), u0: Math.min(a.u0, u), u1: Math.max(a.u1, u) } : { t0: t, t1: t, u0: u, u1: u };
    }
    if (!a) return;
    const keys = [];
    for (let t = a.t0; t <= a.t1; t++) for (let u = a.u0; u <= a.u1; u++) { const k = t * 1000 + u; if (!s.tiles.has(k)) { s.pending.add(k); keys.push(k); } }
    const epoch = st.epoch, sheet = st.sheet;
    api(`/cells?sheet=${sheet}&r0=${a.t0 * TR + 1}&r1=${(a.t1 + 1) * TR}&c0=${a.u0 * TC + 1}&c1=${(a.u1 + 1) * TC}`, { timeout: 60000 }).then((j) => {
      for (const k of keys) s.pending.delete(k);
      if (gone) return;
      if (st.epoch !== epoch) { grid.draw(); return; } // something changed meanwhile: read again
      putStyles(j.styles);
      // what was there before goes (cells deleted or moved away), then what is there now
      if (s.old) for (const key of [...s.cells.keys()]) { const r = Math.floor(key / 20000), c = key % 20000; if (r >= j.r0 && r <= j.r1 && c >= j.c0 && c <= j.c1) s.cells.delete(key); }
      for (const c of j.cells) putCell(s, c);
      for (const k of keys) s.tiles.add(k);
      if (sheet === st.sheet) { grid.draw(); showActive(); statsSoon(); }
    }).catch((e) => { for (const k of keys) s.pending.delete(k); if (!gone) console.warn(e); });
  };

  // ------------------------------------------------------------ sheets
  function blankSheet() { return { layout: null, rowAxis: new Axis(25), colAxis: new Axis(90), rows: 200, cols: 26, cells: new Map(), tiles: new Set(), pending: new Set(), colStyles: [], rowStyles: new Map(), view: null }; }
  function openSheet(i, at) {
    const cur = st.sheets.get(st.sheet);
    if (cur) cur.view = { sel: grid.sel, x: grid.scroll.scrollLeft, y: grid.scroll.scrollTop };
    if (grid.editing) grid.commit("blur");
    st.sheet = i;
    let s = st.sheets.get(i);
    if (!s) { s = blankSheet(); st.sheets.set(i, s); }
    grid.copyArea = st.clip && st.clip.sheet === i ? grid.copyArea : null;
    const ready = !s.layout || s.stale ? loadLayout(i) : Promise.resolve();
    grid.sizeSpacer();
    if (s.view && !at) { grid.sel = s.view.sel; grid.scroll.scrollLeft = s.view.x; grid.scroll.scrollTop = s.view.y; }
    else { grid.scroll.scrollLeft = 0; grid.scroll.scrollTop = 0; grid.select(1, 1); }
    ready.then(() => { if (at) { grid.select(at.r0, at.c0, at.r1, at.c1); grid.reveal(at.r0, at.c0); } });
    drawTabs();
    grid.draw();
    showActive();
    charts.refresh(true);
    statsSoon();
    history.replaceState(null, "", ctx.href("b", id, String(i)));
  }
  function drawTabs() {
    const b = st.book; if (!b) return;
    tabs.replaceChildren(...b.sheets.filter((s) => s.state === "visible" || s.index === st.sheet).map((s) => {
      const t = h("button", { class: "sx-tab" + (s.index === st.sheet ? " on" : ""), role: "tab", "aria-selected": String(s.index === st.sheet), title: `${s.name}${s.rows ? ` · ${s.rows} rows` : ""}` },
        s.color ? h("span", { class: "dot", style: { background: s.color } }) : null, s.name);
      t.onclick = () => { if (s.index !== st.sheet) openSheet(s.index); };
      t.ondblclick = () => renameTab(t, s);
      t.oncontextmenu = (e) => { e.preventDefault(); tabMenu(t, s); };
      t.draggable = true;
      t.ondragstart = (e) => { e.dataTransfer.setData("text/x-sheet", String(s.index)); };
      t.ondragover = (e) => { if ([...e.dataTransfer.types].includes("text/x-sheet")) e.preventDefault(); };
      t.ondrop = (e) => { e.preventDefault(); const from = +e.dataTransfer.getData("text/x-sheet"); if (from !== s.index) send([{ op: "move_sheet", sheet: from, to: s.index }]).then(() => { if (st.sheet === from) st.sheet = s.index; drawTabs(); }); };
      return t;
    }));
  }
  function markTab() { [...tabs.children].forEach((t) => t.classList.toggle("on", t.getAttribute("aria-selected") === "true")); }
  function renameTab(t, s) {
    const inp = h("input", { value: s.name, "aria-label": "The sheet's name" });
    t.replaceChildren(inp); inp.focus(); inp.select();
    let done = false;
    const end = (ok) => { if (done) return; done = true; if (ok && inp.value.trim() && inp.value.trim() !== s.name) send([{ op: "rename_sheet", sheet: s.index, name: inp.value.trim() }]).finally(drawTabs); else drawTabs(); };
    inp.onkeydown = (e) => { if (e.key === "Enter") end(true); if (e.key === "Escape") end(false); e.stopPropagation(); };
    inp.onblur = () => end(true);
  }
  function tabMenu(anchor, s) {
    const n = st.book.sheets.length;
    core.menu(anchor, [
      { label: "Rename", icon: "edit", onclick: () => renameTab(anchor, s) },
      { label: "Duplicate", icon: "copy", onclick: () => send([{ op: "duplicate_sheet", sheet: s.index }]).then((j) => { const k = j?.results?.[0]?.sheet; if (k != null) openSheet(k); }) },
      { label: "Color", icon: "sparkle", onclick: () => palette(anchor, (c) => send([{ op: "sheet_color", sheet: s.index, color: c || "" }])) },
      { label: "Move left", icon: "left", onclick: () => s.index > 0 && send([{ op: "move_sheet", sheet: s.index, to: s.index - 1 }]).then(() => { if (st.sheet === s.index) st.sheet--; drawTabs(); }) },
      { label: "Move right", icon: "right", onclick: () => s.index < n - 1 && send([{ op: "move_sheet", sheet: s.index, to: s.index + 1 }]).then(() => { if (st.sheet === s.index) st.sheet++; drawTabs(); }) },
      { label: "Hide", icon: "eye", onclick: () => { if (st.book.sheets.filter((x) => x.state === "visible").length < 2) { toast("A workbook shows at least one sheet", "bad"); return; } send([{ op: "hide_sheet", sheet: s.index, hidden: true }]).then(() => { if (st.sheet === s.index) openSheet(st.book.sheets.find((x) => x.state === "visible").index); }); } },
      { label: s.grid_lines ? "Hide the grid lines" : "Show the grid lines", icon: "table", onclick: () => send([{ op: "grid_lines", sheet: s.index, show: !s.grid_lines }]) },
      { label: "Save as a ChloeDB table", icon: "db", onclick: () => toTableDialog(s.index) },
      "-",
      { label: "Delete", icon: "trash", danger: true, onclick: async () => {
        if (n < 2) { toast("A workbook keeps at least one sheet", "bad"); return; }
        if (!(await core.confirm({ title: `Delete the sheet ${s.name}?`, body: "Its cells, charts and queries go with it. Undo brings it back.", ok: "Delete", danger: true }))) return;
        await send([{ op: "delete_sheet", sheet: s.index }]);
        st.sheets.clear();
        openSheet(Math.min(s.index, st.book.sheets.length - 1));
      } },
    ]);
  }
  function sheetsMenu(anchor) {
    core.menu(anchor, st.book.sheets.map((s) => ({ label: s.state === "visible" ? s.name : `${s.name} (hidden: show it)`, color: s.color || undefined, icon: s.index === st.sheet ? "check" : "table",
      onclick: () => { if (s.state !== "visible") send([{ op: "hide_sheet", sheet: s.index, hidden: false }]).then(() => openSheet(s.index)); else openSheet(s.index); } })));
  }
  async function addSheet() {
    const j = await send([{ op: "add_sheet" }]);
    const k = j?.results?.[0]?.sheet;
    if (k != null) openSheet(k);
  }

  // ------------------------------------------------------------ the selection
  const area = () => { const s = grid.sel; return { r0: s.r0, c0: s.c0, r1: s.r1, c1: s.c1 }; };
  const rect = (a = area()) => ({ row: a.r0, col: a.c0, width: a.c1 - a.c0 + 1, height: a.r1 - a.r0 + 1 });
  /** The selection cut to the sheet's used cells (a whole column is its used rows). */
  const usedArea = (a = area()) => {
    const l = st.sheets.get(st.sheet)?.layout;
    const ur = Math.max(1, l?.rows_used || 1), uc = Math.max(1, l?.cols_used || 1);
    return { r0: a.r0, c0: a.c0, r1: a.r1 >= MAX_ROW ? Math.max(a.r0, ur) : a.r1, c1: a.c1 >= MAX_COL ? Math.max(a.c0, uc) : a.c1 };
  };
  /** The block of filled cells around the active cell (what Ctrl+A first selects in a spreadsheet), or the selection if it is a range. */
  const region = () => {
    const a = area();
    if (a.r0 !== a.r1 || a.c0 !== a.c1) return usedArea(a);
    let { r0, r1, c0, c1 } = a;
    const full = (r, c) => !!src.cell(r, c)?.text;
    let grew = true;
    while (grew) {
      grew = false;
      if (r0 > 1 && [...range(c0 - (c0 > 1), c1 + 1)].some((c) => full(r0 - 1, c))) { r0--; grew = true; }
      if ([...range(c0 - (c0 > 1), c1 + 1)].some((c) => full(r1 + 1, c))) { r1++; grew = true; }
      if (c0 > 1 && [...range(r0, r1)].some((r) => full(r, c0 - 1))) { c0--; grew = true; }
      if ([...range(r0, r1)].some((r) => full(r, c1 + 1))) { c1++; grew = true; }
      if (r1 - r0 > 100000) break;
    }
    return { r0, r1, c0, c1 };
  };
  function* range(a, b) { for (let i = a; i <= b; i++) yield i; }

  function showActive() {
    const s = grid.sel;
    if (document.activeElement !== refIn) refIn.value = s.r0 === s.r1 && s.c0 === s.c1 ? cellRef(s.r, s.c) : areaRef(s);
    if (!grid.editing && document.activeElement !== fx) {
      const c = src.cell(s.r, s.c);
      fx.value = c ? (c.input ?? c.text) : "";
    }
    paintTools();
  }

  // ------------------------------------------------------------ the formula bar and the editor
  let ac = null; // the list of functions offered as one types
  function editInput(value, input) {
    if (input !== fx && document.activeElement !== fx) fx.value = value;
    if (input === fx && grid.editing) grid.editor.value = value;
    grid.marks = value.startsWith("=") ? refsIn(value) : [];
    grid.draw();
    offer(input);
  }
  function refsIn(f) {
    const out = [], re = /(?:'[^']+'!|[A-Za-z_][A-Za-z0-9_.]*!)?\$?([A-Za-z]{1,3})\$?(\d{1,7})(?::\$?([A-Za-z]{1,3})\$?(\d{1,7}))?/g;
    const colors = ["#2a78d6", "#eb6834", "#1baf7a", "#e87ba4", "#6250d6", "#eda100"];
    let m, i = 0;
    while ((m = re.exec(f))) {
      if (m[0].includes("!")) continue;
      const before = f[m.index - 1];
      if (before && /[A-Za-z0-9_"]/.test(before)) continue;
      const a = norm({ r0: +m[2], c0: colNum(m[1]), r1: +(m[4] || m[2]), c1: colNum(m[3] || m[1]) });
      if (valid(a)) out.push({ ...a, color: colors[i++ % colors.length] });
    }
    return out;
  }
  function offer(input) {
    ac?.el.remove(); ac = null; hint?.remove(); hint = null;
    const v = input.value, caret = input.selectionStart;
    if (!v.startsWith("=")) return;
    const before = v.slice(0, caret);
    const m = before.match(/([A-Za-z][A-Za-z0-9._]*)$/);
    if (m && !/[A-Za-z0-9_$:!"']$/.test(before.slice(0, -m[1].length)) && v[caret] !== "(") {
      const p = m[1].toUpperCase();
      const list = FUNCS.filter((f) => f.name.startsWith(p)).slice(0, 10);
      if (list.length && !(list.length === 1 && list[0].name === p)) {
        const el = h("div", { class: "sx-ac", role: "listbox" });
        ac = { el, list, i: 0, input, start: caret - m[1].length, end: caret };
        const draw = () => el.replaceChildren(...list.map((f, i) => { const d = h("div", { class: i === ac.i ? "on" : "", role: "option" }, h("b", {}, f.name), h("span", {}, f.args)); d.onmousedown = (e) => { e.preventDefault(); ac.i = i; acceptOffer(); }; return d; }));
        ac.draw = draw; draw();
        placePop(el, input);
        return;
      }
    }
    // the function the caret is in, and which of its arguments
    let depth = 0, args = 0;
    for (let i = before.length - 1; i >= 0; i--) {
      const ch = before[i];
      if (ch === ")") depth++;
      else if (ch === "(") {
        if (depth === 0) { const nm = before.slice(0, i).match(/([A-Za-z][A-Za-z0-9._]*)$/); const f = nm && FUNC.get(nm[1].toUpperCase()); if (f) showHint(input, f, args); return; }
        depth--;
      } else if ((ch === "," || ch === ";") && depth === 0) args++;
    }
  }
  let hint = null;
  function showHint(input, f, arg) {
    const parts = f.args.slice(1, -1).split(",");
    hint = h("div", { class: "sx-hint" }, f.name, "(", parts.map((p, i) => [i ? "," : "", i === Math.min(arg, parts.length - 1) || (p.includes("...") && arg >= i) ? h("b", {}, p) : p]), ")");
    placePop(hint, input);
  }
  function placePop(el, input) {
    const b = grid.el.getBoundingClientRect(), r = input.getBoundingClientRect();
    el.style.left = Math.max(4, r.left - b.left) + "px";
    el.style.top = (input === fx ? r.bottom - b.top + 2 : r.bottom - b.top + 2) + "px";
    grid.el.append(el);
  }
  function acceptOffer() {
    if (!ac) return;
    const f = ac.list[ac.i], inp = ac.input;
    const v = inp.value;
    inp.value = v.slice(0, ac.start) + f.name + "(" + v.slice(ac.end);
    const p = ac.start + f.name.length + 1;
    inp.setSelectionRange(p, p);
    ac.el.remove(); ac = null;
    if (inp === grid.editor) grid.fitEditor();
    editInput(inp.value, inp);
  }
  function offerKey(e) {
    if (!ac) return false;
    if (e.key === "ArrowDown") { e.preventDefault(); ac.i = (ac.i + 1) % ac.list.length; ac.draw(); return true; }
    if (e.key === "ArrowUp") { e.preventDefault(); ac.i = (ac.i - 1 + ac.list.length) % ac.list.length; ac.draw(); return true; }
    if (e.key === "Tab" || (e.key === "Enter" && !e.ctrlKey && !e.metaKey && !e.shiftKey)) { e.preventDefault(); acceptOffer(); return true; }
    if (e.key === "Escape") { e.preventDefault(); ac.el.remove(); ac = null; return true; }
    return false;
  }
  // a reference clicked into a formula
  let pointSpan = null;
  function pointable() {
    const inp = grid.editor, v = inp.value;
    if (!v.startsWith("=")) return false;
    const p = inp.selectionStart;
    if (pointSpan && pointSpan.end === p && v.slice(pointSpan.start, pointSpan.end) === pointSpan.text) return true;
    const before = v.slice(0, p).replace(/\s+$/, "");
    return /[=(,;+\-*/^&<>:]$/.test(before);
  }
  function point(a, fresh) {
    const inp = grid.editor, v = inp.value;
    const text = areaRef(a);
    const p = inp.selectionStart;
    let start = p;
    if (pointSpan && pointSpan.end === p && v.slice(pointSpan.start, pointSpan.end) === pointSpan.text) start = pointSpan.start;
    else if (!fresh && pointSpan) start = pointSpan.start;
    inp.value = v.slice(0, start) + text + v.slice(p);
    pointSpan = { start, end: start + text.length, text };
    inp.setSelectionRange(pointSpan.end, pointSpan.end);
    grid.fitEditor();
    editInput(inp.value, inp);
  }

  Object.assign(grid.hooks, {
    select: () => { showActive(); statsSoon(); charts.select(null); },
    editStart: (r, c) => { const cell = src.cell(r, c); return cell ? (cell.input ?? cell.text) : ""; },
    editInput, pointable, point,
    editorKey: (e) => offerKey(e),
    editEnd: () => { ac?.el.remove(); ac = null; hint?.remove(); hint = null; grid.marks = []; pointSpan = null; showActive(); },
    keepEditing: () => document.activeElement === fx,
    edit: (r, c, text, how) => typed(r, c, text, how),
    key: (e) => gridKey(e),
    context: (e, ht) => cellMenu(e, ht),
    colWidth: ([a, b], w) => send([{ op: "col_width", sheet: st.sheet, col: a, count: b - a + 1, width: w }]),
    rowHeight: ([a, b], ht) => send([{ op: "row_height", sheet: st.sheet, row: a, count: b - a + 1, height: ht }]),
    autofit: (c) => autofit(c),
    fill: (base, to) => fillTo(base, to),
    fillDown: (base) => { const l = st.sheets.get(st.sheet)?.layout; let r = base.r1; const left = base.c0 > 1 ? base.c0 - 1 : base.c1 + 1; while (src.cell(r + 1, left)?.text && r < (l?.rows_used || r)) r++; if (r > base.r1) { fillTo(base, { ...base, r1: r }); grid.select(base.r0, base.c0, r, base.c1); } },
    headerClick: (ht, e) => {
      const f = curFilter();
      if (!f || ht.col < f.c0 || ht.col > f.c1) return false;
      const x = grid.sx(ht.col) + src.colAxis.size(ht.col);
      if (ht.x < x - 18) return false;
      filterMenu(e, ht.col);
      return true;
    },
    cellClick: (ht, e) => {
      // a filter's buttons sit in its header row
      const f = curFilter();
      if (!f || ht.row !== f.r0 || ht.col < f.c0 || ht.col > f.c1) return false;
      const x = grid.sx(ht.col) + src.colAxis.size(ht.col);
      if (ht.x < x - 18) return false;
      filterMenu(e, ht.col);
      return true;
    },
    painted: () => { paintFilterButtons(); charts.position(); },
  });

  /** The filter's buttons, drawn over its header row. */
  function paintFilterButtons() {
    const f = curFilter();
    if (!f) return;
    const g = grid.canvas.getContext("2d"), K = grid.colors;
    const fr = grid.frozen();
    const y = grid.sy(f.r0, fr), hh = src.rowAxis.size(f.r0);
    if (y + hh < HH || y > grid.el.clientHeight) return;
    for (let c = f.c0; c <= f.c1; c++) {
      const w = src.colAxis.size(c); if (!w) continue;
      const x = grid.sx(c, fr) + w - 18;
      if (x < grid.HW) continue;
      const on = f.criteria.has(c);
      g.fillStyle = on ? K.accent : K.head; g.strokeStyle = K.line2;
      g.fillRect(x, y + 3, 15, hh - 7); g.strokeRect(x + .5, y + 3.5, 14, hh - 8);
      g.fillStyle = on ? "#fff" : K.text2; g.font = "10px system-ui"; g.textAlign = "center"; g.textBaseline = "middle";
      g.fillText("▼", x + 7.5, y + hh / 2);
    }
  }

  fx.addEventListener("focus", () => { if (!grid.editing) { fx.dataset.start = fx.value; } });
  fx.addEventListener("input", () => { editInput(fx.value, fx); fx.style.height = "auto"; fx.style.height = Math.min(160, fx.scrollHeight) + "px"; });
  fx.addEventListener("keydown", (e) => {
    if (offerKey(e)) return;
    if (e.key === "Enter" && !e.altKey) {
      e.preventDefault();
      const s = grid.sel;
      if (grid.editing) grid.commit(mod(e) && e.shiftKey ? "array" : "down", fx.value);
      else { typed(s.r, s.c, fx.value, mod(e) && e.shiftKey ? "array" : "down"); grid.move(1, 0, false, false); }
      fx.blur(); grid.scroll.focus();
    } else if (e.key === "Escape") { e.preventDefault(); if (grid.editing) grid.cancel(); fx.value = fx.dataset.start || ""; fx.blur(); grid.scroll.focus(); }
  });
  fx.addEventListener("blur", () => { setTimeout(() => { ac?.el.remove(); ac = null; hint?.remove(); hint = null; }, 120); fx.style.height = ""; });
  refIn.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { refIn.blur(); grid.scroll.focus(); showActive(); return; }
    if (e.key !== "Enter") return;
    e.preventDefault();
    goTo(refIn.value);
  });
  refIn.addEventListener("focus", () => refIn.select());
  function goTo(text) {
    const t = String(text).trim();
    const name = st.book?.names?.find((n) => n.name.toLowerCase() === t.toLowerCase());
    const a = parseRef(name ? name.formula : t);
    if (!a) { toast(`${t}: not a cell, a range or a name (A1, B2:D9, C:C, 3:5)`, "bad"); return; }
    const si = a.sheet ? st.book.sheets.findIndex((s) => s.name.toLowerCase() === a.sheet.toLowerCase()) : st.sheet;
    if (si < 0) { toast(`No sheet ${a.sheet}`, "bad"); return; }
    if (si !== st.sheet) openSheet(si, a);
    else { grid.select(a.r0, a.c0, a.r1, a.c1); grid.reveal(a.r0, a.c0); }
    grid.scroll.focus();
  }

  /** What was typed into a cell (or, with Ctrl+Enter, into each cell of the selection). */
  function typed(r, c, text, how) {
    const s = st.sheets.get(st.sheet);
    const sel = area();
    if (how === "array") { send([{ op: "array_formula", sheet: st.sheet, area: rect(sel.r0 === sel.r1 && sel.c0 === sel.c1 ? { r0: r, c0: c, r1: r, c1: c } : sel), input: text }]); return; }
    if (how === "all" && (sel.r0 !== sel.r1 || sel.c0 !== sel.c1)) {
      const a = usedArea(sel);
      const ops = [{ op: "set", sheet: st.sheet, row: a.r0, col: a.c0, input: text }];
      if (a.r1 > a.r0) ops.push({ op: "autofill", sheet: st.sheet, area: { row: a.r0, col: a.c0 }, to_row: a.r1 });
      if (a.c1 > a.c0) ops.push({ op: "autofill", sheet: st.sheet, area: { row: a.r0, col: a.c0, height: a.r1 - a.r0 + 1 }, to_col: a.c1 });
      send(ops);
      return;
    }
    // shown at once, until the answer says what it computes to
    if (s && !text.startsWith("=")) { const old = s.cells.get(r * 20000 + c); s.cells.set(r * 20000 + c, { ...(old || {}), text, type: /^-?\d+(\.\d+)?$/.test(text.trim()) ? "n" : "s", input: null, x: null, st: old?.st || null }); grid.draw(); }
    send([{ op: "set", sheet: st.sheet, row: r, col: c, input: text }]);
  }

  function fillTo(base, to) {
    const ops = [];
    if (to.r1 > base.r1) ops.push({ op: "autofill", sheet: st.sheet, area: rect(base), to_row: to.r1 });
    else if (to.r0 < base.r0) ops.push({ op: "autofill", sheet: st.sheet, area: rect(base), to_row: to.r0 });
    else if (to.c1 > base.c1) ops.push({ op: "autofill", sheet: st.sheet, area: rect(base), to_col: to.c1 });
    else if (to.c0 < base.c0) ops.push({ op: "autofill", sheet: st.sheet, area: rect(base), to_col: to.c0 });
    if (ops.length) send(ops);
  }

  function autofit(c) {
    const s = st.sheets.get(st.sheet); if (!s) return;
    const g = grid.canvas.getContext("2d");
    let w = 40;
    for (const [k, cell] of s.cells) {
      if (k % 20000 !== c || !cell.text) continue;
      const stl = cell.st || {};
      const px = Math.round((stl.sz || 11) * 1.05 + 1.5);
      g.font = `${stl.i ? "italic " : ""}${stl.b ? "600 " : ""}${px}px system-ui, sans-serif`;
      w = Math.max(w, g.measureText(cell.text).width + 14);
    }
    const sel = grid.sel; const [a, b] = sel.r0 === 1 && sel.r1 >= MAX_ROW && c >= sel.c0 && c <= sel.c1 ? [sel.c0, sel.c1] : [c, c];
    send([{ op: "col_width", sheet: st.sheet, col: a, count: b - a + 1, width: Math.min(600, Math.ceil(w)) }]);
  }

  // ------------------------------------------------------------ the toolbar
  /** The style of the active cell, as the toolbar shows it. */
  const activeStyle = () => { const s = grid.sel, c = src.cell(s.r, s.c); return c?.st || src.rowStyle(s.r) || src.colStyle(s.c) || {}; };
  /** An area for an op: whole columns and rows stay whole (IronCalc styles them as such). */
  const styleOp = (path, value, a = area()) => ({ op: "style", sheet: st.sheet, area: rect(a), path, value: String(value) });
  const styleAll = (path, value) => send([styleOp(path, value)]);
  const T = {};
  const fmtSel = h("select", { "aria-label": "Number format", title: "Number format", style: { width: "118px" } },
    FORMATS.map(([v, label, ex]) => h("option", { value: v, title: ex }, label)), h("option", { value: "custom" }, "Custom…"));
  fmtSel.onchange = async () => {
    let v = fmtSel.value;
    if (v === "custom") {
      v = await ask(core, { title: "A number format", label: "Format (as Excel writes them)", value: activeStyle().nf || "#,##0.00", hint: "0.00 · #,##0 · 0% · $#,##0.00;[Red]-$#,##0.00 · yyyy-mm-dd · @ (text)" });
      if (!v) { paintTools(); return; }
    }
    styleAll("num_fmt", v);
    grid.scroll.focus();
  };
  const sizeSel = h("select", { "aria-label": "Font size", title: "Font size" }, [8, 9, 10, 11, 12, 14, 16, 18, 20, 24, 28, 32, 36, 48, 72].map((n) => h("option", { value: n }, String(n))));
  sizeSel.onchange = () => { styleAll("font.size", sizeSel.value); grid.scroll.focus(); };
  const toggleStyle = (key, path) => () => { const on = !!activeStyle()[key]; styleAll(path, on ? "false" : "true"); };
  T.b = tbText(core, h("span", { class: "lt", style: { fontWeight: 800 } }, "B"), "Bold (Ctrl+B)", toggleStyle("b", "font.b"));
  T.i = tbText(core, h("span", { class: "lt", style: { fontStyle: "italic" } }, "I"), "Italic (Ctrl+I)", toggleStyle("i", "font.i"));
  T.u = tbText(core, h("span", { class: "lt", style: { textDecoration: "underline" } }, "U"), "Underline (Ctrl+U)", toggleStyle("u", "font.u"));
  T.s = tbText(core, h("span", { class: "lt", style: { textDecoration: "line-through" } }, "S"), "Strikethrough (Ctrl+5)", toggleStyle("st", "font.strike"));
  const swText = h("span", { class: "sw", style: { background: "#000" } }), swFill = h("span", { class: "sw", style: { background: "#fde047" } });
  let lastText = "#d93025", lastFill = "#fff2cc";
  T.color = h("button", { class: "sx-tb col", title: "Text color", "aria-label": "Text color", onclick: (e) => palette(e.currentTarget, (c) => { if (c) lastText = c; swText.style.background = c || "#000"; styleAll("font.color", c || "#000000"); }, "Automatic") },
    h("span", { style: { font: "600 14px Georgia, serif", lineHeight: "16px" } }, "A"), swText);
  T.fill = h("button", { class: "sx-tb col", title: "Fill color", "aria-label": "Fill color", onclick: (e) => palette(e.currentTarget, (c) => { if (c) lastFill = c; swFill.style.background = c || "transparent"; styleAll("fill.color", c || ""); }, "No fill") },
    svgIcon(core, G.paint), swFill);
  T.left = tb(core, G.left, "Align left", () => styleAll("alignment.horizontal", "left"));
  T.center = tb(core, G.center, "Center", () => styleAll("alignment.horizontal", "center"));
  T.right = tb(core, G.right, "Align right", () => styleAll("alignment.horizontal", "right"));
  T.valign = tb(core, G.bottom, "Vertical alignment", (e) => core.menu(e.currentTarget, [
    { label: "Top", icon: "up", onclick: () => styleAll("alignment.vertical", "top") },
    { label: "Middle", icon: "minus", onclick: () => styleAll("alignment.vertical", "center") },
    { label: "Bottom", icon: "down", onclick: () => styleAll("alignment.vertical", "bottom") },
  ]));
  T.wrap = tb(core, G.wrap, "Wrap text", () => styleAll("alignment.wrap_text", activeStyle().wr ? "false" : "true"));
  let painter = null; // the format painter: a style to put on the next cells selected
  T.paint = tb(core, G.brush, "Paint format: click, then select the cells to format like this one", () => {
    if (painter) { painter = null; paintTools(); return; }
    painter = { ...activeStyle() }; paintTools();
    toast("Select the cells to format like this one", "info", 2500);
  });
  tools.append(
    undoB, redoB, T.paint, sep(h),
    fmtSel,
    tbText(core, h("span", { class: "lt s" }, "$"), "Currency", () => styleAll("num_fmt", "$#,##0.00")),
    tbText(core, h("span", { class: "lt s" }, "%"), "Percent", () => styleAll("num_fmt", "0.00%")),
    tbText(core, h("span", { class: "lt s" }, ".0", h("sub", { style: { fontSize: "9px" } }, "←")), "Fewer decimals", () => styleAll("num_fmt", decimals(activeStyle().nf, src.cell(grid.sel.r, grid.sel.c)?.text, -1))),
    tbText(core, h("span", { class: "lt s" }, ".00", h("sub", { style: { fontSize: "9px" } }, "→")), "More decimals", () => styleAll("num_fmt", decimals(activeStyle().nf, src.cell(grid.sel.r, grid.sel.c)?.text, 1))),
    sep(h), sizeSel, T.b, T.i, T.u, T.s, T.color, T.fill, sep(h),
    tb(core, G.border, "Borders", (e) => bordersPop(e.currentTarget)),
    tb(core, G.merge, "Merge cells", (e) => core.menu(e.currentTarget, [
      { label: "Merge all", icon: "table", onclick: () => mergeOp("merge") },
      { label: "Merge and center", icon: "table", onclick: () => mergeOp("center") },
      { label: "Merge across (each row)", icon: "right", onclick: () => mergeOp("across") },
      { label: "Merge down (each column)", icon: "down", onclick: () => mergeOp("down") },
      { label: "Unmerge", icon: "x", onclick: () => send([{ op: "unmerge", sheet: st.sheet, area: rect(usedArea()) }]) },
    ])),
    T.left, T.center, T.right, T.valign, T.wrap, sep(h),
    tb(core, G.freeze, "Freeze rows and columns", (e) => freezeMenu(e.currentTarget)),
    tb(core, G.asc, "Sort A to Z (by the active cell's column)", () => sortBy(false)),
    tb(core, G.desc, "Sort Z to A (by the active cell's column)", () => sortBy(true)),
    T.filter = tb(core, G.filter, "Filter (Ctrl+Shift+L)", () => toggleFilter()),
    sep(h),
    tb(core, G.sigma, "Sum (Alt+=); more functions in the menu", (e) => core.menu(e.currentTarget, ["SUM", "AVERAGE", "COUNT", "MAX", "MIN", "MEDIAN"].map((f) => ({ label: f, icon: "plus", onclick: () => autosum(f) })))),
    tb(core, G.chart, "Insert a chart of the selection", () => chartDialog()),
    tb(core, G.pivot, "Summarize: a pivot table of the selection on a new sheet", () => summarizeDialog()),
    tb(core, G.cf, "Conditional formatting", () => cfDialog()),
    tb(core, G.sql, "Fill cells from a SQL query of the cluster", () => queryDialog()),
    sep(h),
    tb(core, G.clear, "Clear formatting (Ctrl+\\)", () => send([{ op: "clear", sheet: st.sheet, area: rect(usedArea()), what: "formatting" }])),
  );
  function mergeOp(how) { const a = usedArea(); if (a.r0 === a.r1 && a.c0 === a.c1) { toast("Select the cells to merge", "info"); return; } send([{ op: "merge", sheet: st.sheet, area: rect(a), how }]); }
  function paintTools() {
    const s = activeStyle();
    for (const [k, b] of [["b", T.b], ["i", T.i], ["u", T.u], ["st", T.s], ["wr", T.wrap]]) b.setAttribute("aria-pressed", String(!!s[k]));
    const cell = src.cell(grid.sel.r, grid.sel.c);
    const ha = s.ha && s.ha !== "general" ? s.ha : cell?.type?.[0] === "n" ? "right" : "left";
    T.left.setAttribute("aria-pressed", String(ha === "left")); T.center.setAttribute("aria-pressed", String(ha === "center")); T.right.setAttribute("aria-pressed", String(ha === "right"));
    const nf = s.nf || "general";
    if (![...fmtSel.options].some((o) => o.value === nf)) fmtSel.append(h("option", { value: nf }, nf));
    fmtSel.value = nf;
    const sz = String(Math.round(s.sz || 11));
    if (![...sizeSel.options].some((o) => o.value === sz)) sizeSel.append(h("option", { value: sz }, sz));
    sizeSel.value = sz;
    swText.style.background = s.c || "var(--text)";
    swFill.style.background = s.bg || "transparent";
    T.paint.setAttribute("aria-pressed", String(!!painter));
    T.filter.setAttribute("aria-pressed", String(!!curFilter()));
  }
  /** The ops that give an area a style (the format painter's). */
  function styleOps(s, a) {
    const r = rect(a), o = (path, value) => ({ op: "style", sheet: st.sheet, area: r, path, value: String(value) });
    return [
      { op: "clear", sheet: st.sheet, area: r, what: "formatting" },
      s.b && o("font.b", "true"), s.i && o("font.i", "true"), s.u && o("font.u", "true"), s.st && o("font.strike", "true"),
      s.sz && Math.round(s.sz) !== 11 && o("font.size", Math.round(s.sz)), s.c && o("font.color", s.c), s.bg && o("fill.color", s.bg),
      s.nf && s.nf !== "general" && o("num_fmt", s.nf), s.ha && s.ha !== "general" && o("alignment.horizontal", s.ha), s.va && s.va !== "bottom" && o("alignment.vertical", s.va),
      s.wr && o("alignment.wrap_text", "true"),
    ].filter(Boolean);
  }

  function freezeMenu(anchor) {
    const s = grid.sel, f = src.frozen();
    core.menu(anchor, [
      { label: "No rows", icon: "x", onclick: () => send([{ op: "freeze", sheet: st.sheet, rows: 0 }]) },
      { label: "1 row", icon: "table", onclick: () => send([{ op: "freeze", sheet: st.sheet, rows: 1 }]) },
      { label: "2 rows", icon: "table", onclick: () => send([{ op: "freeze", sheet: st.sheet, rows: 2 }]) },
      { label: `Up to row ${s.r}`, icon: "table", onclick: () => send([{ op: "freeze", sheet: st.sheet, rows: s.r }]) },
      "-",
      { label: "No columns", icon: "x", onclick: () => send([{ op: "freeze", sheet: st.sheet, cols: 0 }]) },
      { label: "1 column", icon: "table", onclick: () => send([{ op: "freeze", sheet: st.sheet, cols: 1 }]) },
      { label: `Up to column ${colName(s.c)}`, icon: "table", onclick: () => send([{ op: "freeze", sheet: st.sheet, cols: s.c }]) },
      f.rows || f.cols ? "-" : null,
      f.rows || f.cols ? { label: "Unfreeze all", icon: "x", onclick: () => send([{ op: "freeze", sheet: st.sheet, rows: 0, cols: 0 }]) } : null,
    ].filter(Boolean));
  }

  let border = { style: "thin", color: "#000000" };
  function bordersPop(anchor) {
    const kinds = [["All", "All borders", "M4 4h16v16H4zM4 12h16M12 4v16"], ["Inner", "Inner borders", "M4 12h16M12 4v16"], ["CenterH", "Inner horizontal", "M4 12h16"], ["CenterV", "Inner vertical", "M12 4v16"], ["Outer", "Outer borders", "M4 4h16v16H4z"],
      ["Left", "Left border", "M4 4v16"], ["Top", "Top border", "M4 4h16"], ["Right", "Right border", "M20 4v16"], ["Bottom", "Bottom border", "M4 20h16"], ["None", "No borders", "M5 5l14 14"]];
    const styleSel = h("select", { "aria-label": "Line style" }, [["thin", "Thin"], ["medium", "Medium"], ["thick", "Thick"], ["double", "Double"], ["dotted", "Dotted"], ["mediumdashed", "Dashed"]].map(([v, l]) => h("option", { value: v, selected: v === border.style }, l)));
    styleSel.onchange = () => { border.style = styleSel.value; };
    const colorIn = h("input", { type: "color", value: border.color, "aria-label": "Line color", style: { width: "34px", height: "26px", padding: 0, border: 0, background: "none" } });
    colorIn.oninput = () => { border.color = colorIn.value; };
    let close;
    const btns = kinds.map(([k, label, d]) => { const b = h("button", { class: "sx-tb", title: label, "aria-label": label, onclick: () => { close(); send([{ op: "border", sheet: st.sheet, area: rect(usedArea()), border: { item: { style: border.style, color: border.color }, type: k } }]); } }, svgIcon(core, `<path d="M4 4h16v16H4zM4 12h16M12 4v16" stroke-dasharray="1.5 2.5" stroke-width="1"/><path d="${d}" stroke-width="2.2"/>`)); return b; });
    close = pop(core, anchor, h("div", { style: { padding: "8px", display: "grid", gap: "8px" } },
      h("div", { style: { display: "grid", gridTemplateColumns: "repeat(5, 30px)", gap: "2px" } }, btns),
      h("div", { class: "row nw", style: { gap: "8px", alignItems: "center" } }, styleSel, colorIn)));
  }

  function palette(anchor, pick, none) {
    let close;
    const choose = (c) => { close(); pick(c); grid.scroll.focus(); };
    const custom = h("input", { type: "color", value: "#2f5bd3", "aria-label": "Another color", style: { width: "28px", height: "22px", padding: 0, border: 0, background: "none" } });
    custom.onchange = () => choose(custom.value);
    close = pop(core, anchor, h("div", {},
      none ? h("button", { class: "btn ghost sm", style: { margin: "8px 8px 0", width: "calc(100% - 16px)", justifyContent: "flex-start" }, onclick: () => choose(null) }, icon("x"), none) : null,
      h("div", { class: "sx-palette" }, PALETTE.map((c) => h("button", { title: c, "aria-label": c, style: { background: c }, onclick: () => choose(c) }))),
      h("div", { class: "row nw", style: { gap: "8px", padding: "0 8px 8px", alignItems: "center" } }, custom, h("span", { class: "small muted" }, "Another color"))));
  }

  // ------------------------------------------------------------ the status bar
  const statsSoon = core.debounce(() => stats_(), 120);
  let statsN = 0;
  async function stats_() {
    const n = ++statsN;
    const sel = area();
    if (sel.r0 === sel.r1 && sel.c0 === sel.c1) {
      const c = src.cell(sel.r0, sel.c0);
      stats.replaceChildren(c?.type?.includes("f") ? h("span", { class: "opt muted" }, "formula") : "");
      return;
    }
    const a = usedArea(sel);
    const s = st.sheets.get(st.sheet);
    let nums = [], count = 0, partial = false;
    const loaded = s && [...range(Math.floor((a.r0 - 1) / TR), Math.floor((a.r1 - 1) / TR))].every((t) => [...range(Math.floor((a.c0 - 1) / TC), Math.floor((a.c1 - 1) / TC))].every((u) => s.tiles.has(t * 1000 + u)));
    if (loaded) {
      for (let r = a.r0; r <= a.r1; r++) for (let c = a.c0; c <= a.c1; c++) {
        const cell = src.cell(r, c);
        if (!cell || cell.text === "") continue;
        count++;
        if (cell.type?.[0] === "n") { const v = num(cell.text); if (v != null) nums.push(v); }
      }
    } else {
      try {
        const j = await api(`/values?sheet=${st.sheet}&range=${cellRef(a.r0, a.c0)}:${cellRef(a.r1, a.c1)}`, { timeout: 20000 });
        if (n !== statsN || gone) return;
        partial = j.range[2] < a.r1 || j.range[3] < a.c1;
        for (const row of j.rows) for (const [shown, v] of row) { if (shown !== "" && shown != null) count++; if (typeof v === "number") nums.push(v); }
      } catch { return; }
    }
    if (n !== statsN) return;
    const f = (v) => fmtNum(v);
    const sum = nums.reduce((x, y) => x + y, 0);
    const items = [["Count", count.toLocaleString()]];
    if (nums.length) items.unshift(["Sum", f(sum)], ["Average", f(sum / nums.length)]), items.push(["Min", f(Math.min(...nums)), "opt"], ["Max", f(Math.max(...nums)), "opt"]);
    stats.replaceChildren(...items.map(([k, v, cls]) => h("span", { class: cls || "", title: `${k} of the selection${partial ? " (its first 5,000 rows and 200 columns)" : ""}` }, k, ": ", h("b", {}, v))), partial ? h("span", { class: "opt muted" }, "first 5,000 rows") : "");
  }

  // ------------------------------------------------------------ sorting, filtering, sums
  /** Whether the first row of an area looks like column names (text over values). */
  function looksLikeHeader(a) {
    if (a.r1 <= a.r0) return false;
    let text = 0, below = 0;
    for (let c = a.c0; c <= a.c1; c++) {
      const top = src.cell(a.r0, c), next = src.cell(a.r0 + 1, c);
      if (top?.text && top.type?.[0] === "s") text++;
      if (next?.text && next.type?.[0] !== "s") below++;
    }
    const s = st.sheets.get(st.sheet);
    return text > 0 && (below > 0 || (s?.layout?.frozen_rows || 0) >= a.r0 || !!src.cell(a.r0, a.c0)?.st?.b);
  }
  function sortBy(desc, col) {
    const a = region();
    const header = looksLikeHeader(a);
    send([{ op: "sort", sheet: st.sheet, area: rect(a), by: col || grid.sel.c, desc, header }]).then((j) => { if (j && !j.error) toast(`Sorted ${areaRef(a)} by column ${colName(col || grid.sel.c)}${desc ? ", Z to A" : ""}${header ? " (its first row kept as the header)" : ""}`, "ok", 2600); });
  }
  function toggleFilter() {
    const f = curFilter();
    if (f) { send([{ ...filterOp(f, new Map()), off: true }]); return; }
    const a = region();
    if (a.r1 <= a.r0) { toast("Select a table of cells (its first row names the columns)", "info"); return; }
    send([filterOp(a, new Map())]).then((j) => { if (j && !j.error) toast("Filter on: the buttons in the first row choose what shows", "info", 2600); });
  }
  async function filterMenu(e, col) {
    const f = curFilter();
    if (!f) return;
    // every value of the column, from the server (the filtered rows count too)
    let values = [];
    try {
      const j = await api(`/values?sheet=${st.sheet}&range=${cellRef(f.r0 + 1, col)}:${cellRef(f.r1, col)}`, { timeout: 20000 });
      const n = new Map();
      for (const [[shown]] of j.rows.map((r) => [r[0] || ["", null]])) n.set(shown ?? "", (n.get(shown ?? "") || 0) + 1);
      values = [...n.entries()].sort((x, y) => String(x[0]).localeCompare(String(y[0]), undefined, { numeric: true }));
    } catch (err) { toast(err.message, "bad"); return; }
    const on = f.criteria.get(col);
    const search = h("input", { class: "input", placeholder: "Search the values", "aria-label": "Search the values", style: { width: "100%" } });
    const boxes = values.map(([v, n]) => { const cb = h("input", { type: "checkbox", checked: !on || on.includes(v) }); return { v, cb, el: h("label", {}, cb, h("span", { class: "grow" }, v === "" ? h("i", { class: "muted" }, "(blank)") : v), h("span", { class: "small muted" }, String(n))) }; });
    const list = h("div", { class: "sx-values" }, boxes.map((b) => b.el));
    search.oninput = () => { const q = search.value.toLowerCase(); for (const b of boxes) b.el.style.display = String(b.v).toLowerCase().includes(q) ? "" : "none"; };
    let close;
    const apply = () => {
      const chosen = boxes.filter((b) => b.cb.checked).map((b) => b.v);
      if (chosen.length === boxes.length) f.criteria.delete(col); else f.criteria.set(col, chosen);
      close();
      send([filterOp(f, f.criteria)]);
    };
    const body = h("div", { style: { padding: "10px", width: "250px", display: "grid", gap: "8px" } },
      h("div", { class: "row nw", style: { gap: "4px" } },
        h("button", { class: "btn sm", onclick: () => { close(); sortFiltered(f, col, false); } }, svgIcon(core, G.asc), "A to Z"),
        h("button", { class: "btn sm", onclick: () => { close(); sortFiltered(f, col, true); } }, svgIcon(core, G.desc), "Z to A")),
      search,
      h("div", { class: "row nw", style: { gap: "10px", fontSize: "12.5px" } },
        h("a", { href: "#", onclick: (ev) => { ev.preventDefault(); boxes.forEach((b) => (b.cb.checked = b.el.style.display !== "none" || b.cb.checked)); } }, "Select all"),
        h("a", { href: "#", onclick: (ev) => { ev.preventDefault(); boxes.forEach((b) => { if (b.el.style.display !== "none") b.cb.checked = false; }); } }, "Clear")),
      list,
      h("div", { class: "row nw", style: { gap: "6px", justifyContent: "flex-end" } }, h("button", { class: "btn sm", onclick: () => close() }, "Cancel"), h("button", { class: "btn sm primary", onclick: apply }, "OK")));
    const anchor = { getBoundingClientRect: () => ({ left: e.clientX - 10, right: e.clientX, top: e.clientY - 8, bottom: e.clientY + 8 }) };
    close = pop(core, anchor, body);
    setTimeout(() => search.focus(), 0);
  }
  function sortFiltered(f, col, desc) { send([{ op: "sort", sheet: st.sheet, area: rect(f), by: col, desc, header: true }]); }

  /** Σ: a function of the numbers above (or to the left of) the active cell, or under each column of the selection. */
  function autosum(fn) {
    const sel = area();
    const isNum = (r, c) => { const x = src.cell(r, c); return x && x.type?.[0] === "n"; };
    if (sel.r0 === sel.r1 && sel.c0 === sel.c1) {
      const { r0: r, c0: c } = sel;
      let a = r - 1; while (a >= 1 && isNum(a, c)) a--;
      if (a < r - 1) { grid.startEdit(`=${fn}(${cellRef(a + 1, c)}:${cellRef(r - 1, c)})`); return; }
      let b = c - 1; while (b >= 1 && isNum(r, b)) b--;
      if (b < c - 1) { grid.startEdit(`=${fn}(${cellRef(r, b + 1)}:${cellRef(r, c - 1)})`); return; }
      grid.startEdit(`=${fn}()`);
      const ed = grid.editor; ed.setSelectionRange(ed.value.length - 1, ed.value.length - 1);
      return;
    }
    const a = usedArea(sel);
    const rows = [];
    const line = [];
    for (let c = a.c0; c <= a.c1; c++) line.push(`=${fn}(${cellRef(a.r0, c)}:${cellRef(a.r1, c)})`);
    rows.push(line);
    send([{ op: "set_many", sheet: st.sheet, row: a.r1 + 1, col: a.c0, values: rows }]);
  }

  // ------------------------------------------------------------ the keyboard
  function gridKey(e) {
    const k = e.key.toLowerCase(), m = mod(e);
    const sel = area();
    if (m && !e.shiftKey && k === "z") { e.preventDefault(); send([{ op: "undo" }]); return true; }
    if (m && (k === "y" || (e.shiftKey && k === "z"))) { e.preventDefault(); send([{ op: "redo" }]); return true; }
    if (m && !e.shiftKey && k === "b") { e.preventDefault(); toggleStyle("b", "font.b")(); return true; }
    if (m && !e.shiftKey && k === "i") { e.preventDefault(); toggleStyle("i", "font.i")(); return true; }
    if (m && !e.shiftKey && k === "u") { e.preventDefault(); toggleStyle("u", "font.u")(); return true; }
    if (m && k === "5") { e.preventDefault(); toggleStyle("st", "font.strike")(); return true; }
    if (m && k === "f") { e.preventDefault(); findPanel(); return true; }
    if (m && k === "h") { e.preventDefault(); findPanel(true); return true; }
    if (m && k === "s") { e.preventDefault(); toast("Every change is already saved in ChloeDB", "ok", 2000); return true; }
    if (m && k === "\\") { e.preventDefault(); send([{ op: "clear", sheet: st.sheet, area: rect(usedArea()), what: "formatting" }]); return true; }
    if (m && e.shiftKey && k === "l") { e.preventDefault(); toggleFilter(); return true; }
    if (m && e.shiftKey && (k === "$" || e.code === "Digit4")) { e.preventDefault(); styleAll("num_fmt", "$#,##0.00"); return true; }
    if (m && e.shiftKey && (k === "%" || e.code === "Digit5")) { e.preventDefault(); styleAll("num_fmt", "0%"); return true; }
    if (m && e.shiftKey && (k === "!" || e.code === "Digit1")) { e.preventDefault(); styleAll("num_fmt", "#,##0.00"); return true; }
    if (e.altKey && (k === "=" || e.code === "Equal")) { e.preventDefault(); autosum("SUM"); return true; }
    if (m && !e.shiftKey && k === ";") { e.preventDefault(); const d = new Date(); typed(sel.r0, sel.c0, `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`); return true; }
    if (m && e.shiftKey && (k === ":" || k === ";")) { e.preventDefault(); const d = new Date(); typed(sel.r0, sel.c0, `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}:${String(d.getSeconds()).padStart(2, "0")}`); return true; }
    if (m && k === "d") { e.preventDefault(); fillDown(); return true; }
    if (m && k === "r") { e.preventDefault(); fillRight(); return true; }
    if (m && k === " ") { e.preventDefault(); grid.select(1, sel.c0, MAX_ROW, sel.c1, { r: grid.sel.r, c: grid.sel.c }); return true; }
    if (e.shiftKey && !m && k === " ") { e.preventDefault(); grid.select(sel.r0, 1, sel.r1, MAX_COL, { r: grid.sel.r, c: grid.sel.c }); return true; }
    if (m && k === "a") {
      e.preventDefault();
      // first the block of data around the cell, then the whole sheet
      const r = region();
      if (r.r0 === sel.r0 && r.r1 === sel.r1 && r.c0 === sel.c0 && r.c1 === sel.c1) grid.select(1, 1, MAX_ROW, MAX_COL, { r: grid.sel.r, c: grid.sel.c });
      else grid.select(r.r0, r.c0, r.r1, r.c1, { r: grid.sel.r, c: grid.sel.c });
      return true;
    }
    if (m && (k === "-" || k === "+" || k === "=")) {
      e.preventDefault();
      const rows = sel.c0 === 1 && sel.c1 >= MAX_COL, cols = sel.r0 === 1 && sel.r1 >= MAX_ROW;
      if (k === "-") { if (rows) deleteRows(); else if (cols) deleteCols(); else toast("Select whole rows or columns to delete them (click their headers)", "info"); }
      else { if (rows) insertRows(sel.r0); else if (cols) insertCols(sel.c0); else toast("Select whole rows or columns to insert them (click their headers)", "info"); }
      return true;
    }
    if (k === "delete" || k === "backspace") { e.preventDefault(); clearContents(); return true; }
    if (k === "contextmenu" || (e.shiftKey && k === "f10")) { e.preventDefault(); const rc = grid.rect(grid.sel), b = grid.el.getBoundingClientRect(); cellMenu({ clientX: b.left + rc.x + 20, clientY: b.top + rc.y + rc.h }, { row: grid.sel.r, col: grid.sel.c }); return true; }
    if (k === "pagedown" && e.altKey) { e.preventDefault(); nextSheet(1); return true; }
    if (k === "pageup" && e.altKey) { e.preventDefault(); nextSheet(-1); return true; }
    if (m && k === "pagedown") { e.preventDefault(); nextSheet(1); return true; }
    if (m && k === "pageup") { e.preventDefault(); nextSheet(-1); return true; }
    return false;
  }
  function nextSheet(d) {
    const vis = st.book.sheets.filter((s) => s.state === "visible");
    const i = vis.findIndex((s) => s.index === st.sheet);
    const n = vis[(i + d + vis.length) % vis.length];
    if (n && n.index !== st.sheet) openSheet(n.index);
  }
  function clearContents() {
    const a = usedArea();
    const s = st.sheets.get(st.sheet);
    for (let r = a.r0; r <= Math.min(a.r1, a.r0 + 5000); r++) for (let c = a.c0; c <= Math.min(a.c1, a.c0 + 300); c++) { const x = s.cells.get(r * 20000 + c); if (x) x.text = ""; }
    grid.draw();
    send([{ op: "clear", sheet: st.sheet, area: rect(a), what: "contents" }]);
  }
  function fillDown() {
    const a = usedArea();
    if (a.r0 === a.r1) { if (a.r0 > 1) send([{ op: "autofill", sheet: st.sheet, area: rect({ r0: a.r0 - 1, r1: a.r0 - 1, c0: a.c0, c1: a.c1 }), to_row: a.r0 }]); return; }
    send([{ op: "autofill", sheet: st.sheet, area: rect({ ...a, r1: a.r0 }), to_row: a.r1 }]);
  }
  function fillRight() {
    const a = usedArea();
    if (a.c0 === a.c1) { if (a.c0 > 1) send([{ op: "autofill", sheet: st.sheet, area: rect({ r0: a.r0, r1: a.r1, c0: a.c0 - 1, c1: a.c0 - 1 }), to_col: a.c0 }]); return; }
    send([{ op: "autofill", sheet: st.sheet, area: rect({ ...a, c1: a.c0 }), to_col: a.c1 }]);
  }
  const insertRows = (at, n = Math.max(1, area().r1 - area().r0 + 1)) => send([{ op: "insert_rows", sheet: st.sheet, row: at, count: Math.min(n, 1000) }]);
  const insertCols = (at, n = Math.max(1, area().c1 - area().c0 + 1)) => send([{ op: "insert_cols", sheet: st.sheet, col: at, count: Math.min(n, 100) }]);
  function deleteRows() { const a = usedArea(); send([{ op: "delete_rows", sheet: st.sheet, row: a.r0, count: a.r1 - a.r0 + 1 }]); }
  function deleteCols() { const a = usedArea(); send([{ op: "delete_cols", sheet: st.sheet, col: a.c0, count: a.c1 - a.c0 + 1 }]); }

  // ------------------------------------------------------------ the clipboard
  /** Copies the selection: as text for other programs, and (from chloedb-sheets) with its formulas and formats for a paste here. */
  function copySel(e, cut) {
    if (grid.editing || document.activeElement !== grid.scroll) return;
    const a = usedArea();
    const s = st.sheets.get(st.sheet);
    const lines = [];
    for (let r = a.r0; r <= Math.min(a.r1, a.r0 + 20000); r++) { const l = []; for (let c = a.c0; c <= a.c1; c++) l.push((s.cells.get(r * 20000 + c)?.text || "").replace(/[\t\n]/g, " ")); lines.push(l.join("\t")); }
    const text = lines.join("\n");
    e.clipboardData.setData("text/plain", text);
    e.preventDefault();
    const clip = { sheet: st.sheet, ...a, text, cut, data: null };
    st.clip = clip;
    grid.copyArea = { sheet: st.sheet, ...a };
    grid.draw();
    api("/copy", { body: { sheet: st.sheet, area: rect(a) }, timeout: 60000 }).then((j) => {
      clip.data = j.clipboard;
      if (j.text !== text) { clip.text = j.text; navigator.clipboard?.writeText(j.text).catch(() => {}); }
    }).catch((err) => toast(`Copy: ${err.message}`, "bad"));
    toast(cut ? `Cut ${areaRef(a)}: paste it where it goes` : `Copied ${areaRef(a)}`, "info", 1600);
  }
  async function pasteSel(e) {
    if (grid.editing || document.activeElement !== grid.scroll) return;
    const text = e.clipboardData?.getData("text/plain") ?? "";
    e.preventDefault();
    pasteText(text);
  }
  async function pasteText(text, valuesOnly = false) {
    const to = { row: grid.sel.r0, col: grid.sel.c0 };
    const clip = st.clip;
    const same = clip && norm1(clip.text) === norm1(text);
    if (same && !valuesOnly) {
      // pasted from here: formulas move with it, formats come along
      for (let i = 0; i < 40 && !clip.data; i++) await new Promise((r) => setTimeout(r, 100));
      if (clip.data) {
        await send([{ op: "paste", sheet: st.sheet, to, clipboard: clip.data, cut: !!clip.cut }]);
        const hgt = clip.r1 - clip.r0, w = clip.c1 - clip.c0;
        grid.select(to.row, to.col, to.row + hgt, to.col + w);
        if (clip.cut) { st.clip = null; grid.copyArea = null; }
        return;
      }
    }
    if (!text) return;
    const lines = text.replace(/\r\n/g, "\n").replace(/\n$/, "").split("\n");
    await send([{ op: "paste_text", sheet: st.sheet, to, text }]);
    grid.select(to.row, to.col, to.row + lines.length - 1, to.col + Math.max(...lines.map((l) => l.split("\t").length)) - 1);
  }
  const norm1 = (t) => String(t || "").replace(/\r\n/g, "\n").replace(/\n+$/, "");
  const onCopy = (e) => copySel(e, false), onCut = (e) => copySel(e, true), onPaste = (e) => pasteSel(e);
  document.addEventListener("copy", onCopy);
  document.addEventListener("cut", onCut);
  document.addEventListener("paste", onPaste);
  grid.scroll.addEventListener("pointerup", () => {
    if (!painter) return;
    const p = painter; painter = null;
    send(styleOps(p, area()));
    paintTools();
  });

  // ------------------------------------------------------------ menus on the cells
  function cellMenu(e, ht) {
    const sel = area();
    const rows = sel.c0 === 1 && sel.c1 >= MAX_COL, cols = sel.r0 === 1 && sel.r1 >= MAX_ROW;
    const nr = sel.r1 - sel.r0 + 1, nc = sel.c1 - sel.c0 + 1;
    const R = (n) => (n === 1 ? "row" : `${n} rows`), C = (n) => (n === 1 ? "column" : `${n} columns`);
    const anchor = { getBoundingClientRect: () => ({ left: e.clientX, right: e.clientX, top: e.clientY, bottom: e.clientY }) };
    const clipItems = [
      { label: "Cut", icon: "edit", onclick: () => document.execCommand("cut") || toast(`${isMac ? "⌘" : "Ctrl+"}X cuts`, "info") },
      { label: "Copy", icon: "copy", onclick: () => document.execCommand("copy") || toast(`${isMac ? "⌘" : "Ctrl+"}C copies`, "info") },
      { label: "Paste", icon: "download", onclick: () => navigator.clipboard?.readText ? navigator.clipboard.readText().then((t) => pasteText(t)).catch(() => (st.clip ? pasteText(st.clip.text) : toast(`${isMac ? "⌘" : "Ctrl+"}V pastes`, "info"))) : toast(`${isMac ? "⌘" : "Ctrl+"}V pastes`, "info") },
      { label: "Paste values only", icon: "download", onclick: () => (navigator.clipboard?.readText ? navigator.clipboard.readText() : Promise.reject()).catch(() => st.clip?.text || "").then((t) => pasteText(t, true)) },
      "-",
    ];
    if (ht.row === 0 || cols) {
      core.menu(anchor, [...clipItems,
        { label: `Insert ${C(nc)} left`, icon: "left", onclick: () => insertCols(sel.c0, nc) },
        { label: `Insert ${C(nc)} right`, icon: "right", onclick: () => insertCols(sel.c1 + 1, nc) },
        { label: `Delete ${C(nc)}`, icon: "trash", onclick: () => deleteCols() },
        { label: `Clear ${C(nc)}`, icon: "x", onclick: () => clearContents() },
        { label: `Hide ${C(nc)}`, icon: "eye", onclick: () => send([{ op: "hide_cols", sheet: st.sheet, col: sel.c0, count: nc, hidden: true }]) },
        { label: "Show hidden columns here", icon: "eye", onclick: () => send([{ op: "hide_cols", sheet: st.sheet, col: Math.max(1, sel.c0 - (nc === 1 ? 1 : 0)), count: nc + (nc === 1 ? 2 : 0), hidden: false }]) },
        { label: "Width…", icon: "sliders", onclick: () => sizeDialog("col", sel.c0, nc) },
        { label: "Fit to the contents", icon: "fit", onclick: () => autofit(sel.c0) },
        "-",
        { label: "Sort A to Z by this column", icon: "up", onclick: () => sortBy(false, sel.c0) },
        { label: "Sort Z to A by this column", icon: "down", onclick: () => sortBy(true, sel.c0) },
      ]);
      return;
    }
    if (ht.col === 0 || rows) {
      core.menu(anchor, [...clipItems,
        { label: `Insert ${R(nr)} above`, icon: "up", onclick: () => insertRows(sel.r0, nr) },
        { label: `Insert ${R(nr)} below`, icon: "down", onclick: () => insertRows(sel.r1 + 1, nr) },
        { label: `Delete ${R(nr)}`, icon: "trash", onclick: () => deleteRows() },
        { label: `Clear ${R(nr)}`, icon: "x", onclick: () => clearContents() },
        { label: `Hide ${R(nr)}`, icon: "eye", onclick: () => send([{ op: "hide_rows", sheet: st.sheet, row: sel.r0, count: nr, hidden: true }]) },
        { label: "Show hidden rows here", icon: "eye", onclick: () => send([{ op: "hide_rows", sheet: st.sheet, row: Math.max(1, sel.r0 - (nr === 1 ? 1 : 0)), count: nr + (nr === 1 ? 2 : 0), hidden: false }]) },
        { label: "Height…", icon: "sliders", onclick: () => sizeDialog("row", sel.r0, nr) },
        { label: nr === 1 ? "Move up" : "Move these rows up", icon: "up", onclick: () => sel.r0 > 1 && send([{ op: "move_rows", sheet: st.sheet, row: sel.r0, count: nr, delta: -1 }]).then(() => grid.select(sel.r0 - 1, 1, sel.r1 - 1, MAX_COL)) },
        { label: nr === 1 ? "Move down" : "Move these rows down", icon: "down", onclick: () => send([{ op: "move_rows", sheet: st.sheet, row: sel.r0, count: nr, delta: 1 }]).then(() => grid.select(sel.r0 + 1, 1, sel.r1 + 1, MAX_COL)) },
      ]);
      return;
    }
    const cell = src.cell(grid.sel.r, grid.sel.c);
    const f = curFilter();
    core.menu(anchor, [...clipItems,
      { label: `Insert ${R(nr)} above`, icon: "up", onclick: () => insertRows(sel.r0, nr) },
      { label: `Insert ${C(nc)} left`, icon: "left", onclick: () => insertCols(sel.c0, nc) },
      { label: `Delete ${R(nr)}`, icon: "trash", onclick: () => send([{ op: "delete_rows", sheet: st.sheet, row: sel.r0, count: nr }]) },
      { label: `Delete ${C(nc)}`, icon: "trash", onclick: () => send([{ op: "delete_cols", sheet: st.sheet, col: sel.c0, count: nc }]) },
      { label: "Clear contents", icon: "x", onclick: () => clearContents() },
      { label: "Clear formatting", icon: "x", onclick: () => send([{ op: "clear", sheet: st.sheet, area: rect(usedArea()), what: "formatting" }]) },
      "-",
      { label: "Sort A to Z by this column", icon: "up", onclick: () => sortBy(false) },
      { label: "Sort Z to A by this column", icon: "down", onclick: () => sortBy(true) },
      f && cell?.text != null ? { label: `Show only rows with “${String(cell.text).slice(0, 24)}”`, icon: "filter", onclick: () => { f.criteria.set(grid.sel.c, [cell.text]); send([filterOp(f, f.criteria)]); } } : null,
      { label: "Insert a chart", icon: "timeline", onclick: () => chartDialog() },
      { label: "Summarize (pivot table)", icon: "layers", onclick: () => summarizeDialog() },
      { label: "Conditional formatting", icon: "sparkle", onclick: () => cfDialog() },
      { label: "Name this range", icon: "pin", onclick: () => namesDialog(areaRef(sel)) },
      { label: "Copy a link to this cell", icon: "link", onclick: () => core.copy(location.href.split("#")[0] + ctx.href("b", id, String(st.sheet), cellRef(grid.sel.r, grid.sel.c)), "Link copied") },
    ].filter(Boolean));
  }
  async function sizeDialog(kind, at, n) {
    const ax = kind === "col" ? src.colAxis : src.rowAxis;
    const v = await ask(core, { title: kind === "col" ? "Column width" : "Row height", label: "Pixels", value: String(Math.round(ax.size(at)) || (kind === "col" ? 90 : 25)), type: "number" });
    if (!v || !(+v > 0)) return;
    send([kind === "col" ? { op: "col_width", sheet: st.sheet, col: at, count: n, width: +v } : { op: "row_height", sheet: st.sheet, row: at, count: n, height: +v }]);
  }

  // ------------------------------------------------------------ find and replace
  let findEl = null;
  function findPanel(replace) {
    if (findEl) { findEl.querySelector("input")?.focus(); if (replace) findEl.dataset.replace = "1"; return; }
    const q = h("input", { class: "input", placeholder: "Find", "aria-label": "Find" });
    const rep = h("input", { class: "input", placeholder: "Replace with", "aria-label": "Replace with" });
    const opt = (label, on = false) => { const cb = h("input", { type: "checkbox", checked: on }); return [cb, h("label", {}, cb, label)]; };
    const [caseCb, caseL] = opt("Match case"), [wholeCb, wholeL] = opt("Whole cell"), [fCb, fL] = opt("In formulas"), [allCb, allL] = opt("Every sheet", true);
    const count = h("span", { class: "small muted", "aria-live": "polite" });
    let hits = [], at = -1;
    const show = (i) => {
      if (!hits.length) return;
      at = (i + hits.length) % hits.length;
      const [s, r, c] = hits[at];
      count.textContent = `${at + 1} of ${hits.length}${hits.length >= 2000 ? "+" : ""}`;
      grid.marks = hits.filter((x) => x[0] === s).slice(0, 500).map(([, rr, cc]) => ({ r0: rr, c0: cc, r1: rr, c1: cc, color: "#eda100", dash: true }));
      if (s !== st.sheet) openSheet(s, { r0: r, c0: c, r1: r, c1: c });
      else { grid.select(r, c); grid.reveal(r, c); }
      grid.marks = hits.filter((x) => x[0] === s).slice(0, 500).map(([, rr, cc]) => ({ r0: rr, c0: cc, r1: rr, c1: cc, color: "#eda100", dash: true }));
      grid.draw();
    };
    const run = core.debounce(async () => {
      if (!q.value) { hits = []; count.textContent = ""; grid.marks = []; grid.draw(); return; }
      const p = new URLSearchParams({ q: q.value, case: caseCb.checked ? "1" : "0", whole: wholeCb.checked ? "1" : "0", formulas: fCb.checked ? "1" : "0" });
      if (!allCb.checked) p.set("sheet", String(st.sheet));
      try {
        const j = await api(`/find?${p}`, { timeout: 30000 });
        hits = j.matches;
        if (!hits.length) { count.textContent = "Not found"; grid.marks = []; grid.draw(); return; }
        // the first from the active cell on
        const i = hits.findIndex(([s, r, c]) => s > st.sheet || (s === st.sheet && (r > grid.sel.r || (r === grid.sel.r && c >= grid.sel.c))));
        show(i < 0 ? 0 : i);
      } catch (e) { count.textContent = e.message; }
    }, 250);
    for (const x of [q, caseCb, wholeCb, fCb, allCb]) x.addEventListener(x === q ? "input" : "change", run);
    q.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); show(at + (e.shiftKey ? -1 : 1)); } if (e.key === "Escape") close(); });
    const replaceOne = () => {
      if (at < 0 || !hits.length) return;
      const [s, r, c] = hits[at];
      const cell = s === st.sheet ? src.cell(r, c) : null;
      if (!cell) { toast("Go to the cell first (Enter)", "info"); return; }
      const input = cell.input ?? cell.text;
      const re = new RegExp(q.value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), caseCb.checked ? "" : "i");
      const next = wholeCb.checked ? rep.value : input.replace(re, rep.value);
      send([{ op: "set", sheet: s, row: r, col: c, input: next }]).then(() => run());
    };
    const replaceAll = async () => {
      if (!q.value) return;
      const j = await send([{ op: "replace", sheet: allCb.checked ? null : st.sheet, find: q.value, replace: rep.value, case: caseCb.checked, whole: wholeCb.checked }]);
      const n = j?.results?.[0]?.replaced;
      if (n != null) toast(`Replaced ${n} ${n === 1 ? "cell" : "cells"}`, "ok");
      run();
    };
    function close() { findEl?.remove(); findEl = null; grid.marks = []; grid.draw(); grid.scroll.focus(); }
    findEl = h("div", { class: "sx-panel", role: "dialog", "aria-label": "Find and replace" },
      h("h4", {}, svgIcon(core, G.find), "Find and replace", h("span", { class: "grow" }), h("button", { class: "btn ghost sm icon", "aria-label": "Close", onclick: close }, icon("x"))),
      h("div", { class: "stack", style: { gap: "8px" } },
        h("div", { class: "sx-find" }, q, h("button", { class: "btn sm icon", title: "Previous (Shift+Enter)", "aria-label": "Previous", onclick: () => show(at - 1) }, icon("up")), h("button", { class: "btn sm icon", title: "Next (Enter)", "aria-label": "Next", onclick: () => show(at + 1) }, icon("down"))),
        h("div", { class: "sx-checks" }, caseL, wholeL, fL, allL),
        h("div", { class: "sx-find" }, rep, h("button", { class: "btn sm", onclick: replaceOne }, "Replace"), h("button", { class: "btn sm", onclick: replaceAll }, "Replace all")),
        count));
    grid.el.append(findEl);
    q.focus();
    const sel = src.cell(grid.sel.r, grid.sel.c);
    if (sel?.text && grid.sel.r0 === grid.sel.r1 && grid.sel.c0 === grid.sel.c1 && sel.text.length < 40) { q.value = sel.text; q.select(); run(); }
  }

  // ------------------------------------------------------------ SQL queries into cells
  function queryDialog(q) {
    const sql = h("textarea", { class: "input sx-sql", spellcheck: "false", "aria-label": "SQL", placeholder: "SELECT region, SUM(total) AS total FROM orders GROUP BY region ORDER BY total DESC" }, q?.sql || "");
    const sheetIdx = q ? st.book.sheets.findIndex((s) => s.id === q.sheet) : st.sheet;
    const at = h("input", { class: "input", value: q ? cellRef(q.row, q.col) : cellRef(grid.sel.r, grid.sel.c), "aria-label": "Top left cell", style: { width: "90px" } });
    const limit = h("input", { class: "input", type: "number", min: 1, max: 100000, value: "10000", "aria-label": "Rows at most", style: { width: "100px" } });
    const tables = h("select", { "aria-label": "Start from a table" }, h("option", { value: "" }, "Start from a table…"));
    ctx.core.api("/v1/tables", { timeout: 8000 }).then((j) => { for (const t of tableList(j)) if (!/^sheets_/.test(t.name)) tables.append(h("option", { value: t.name }, t.name)); }).catch(() => {});
    tables.onchange = () => { if (tables.value) { sql.value = `SELECT * FROM ${qid(tables.value)} LIMIT 1000`; sql.focus(); } };
    const err = h("div", { class: "small", style: { color: "var(--crit-ink)" } });
    const go = h("button", { class: "btn primary" }, icon("play"), q ? "Run again" : "Run and fill the cells");
    const m = core.modal({
      title: q ? "A query of the cluster" : "Fill cells from a query of the cluster", wide: true,
      body: h("div", { class: "stack", style: { gap: "10px" } },
        h("p", { class: "small dim", style: { margin: 0 } }, "The answer fills the sheet from the cell given: the column names, then the rows. It is kept with the workbook and runs again when you refresh it, so the cells follow the table."),
        h("div", { class: "row nw", style: { gap: "8px" } }, tables), sql,
        h("div", { class: "row nw", style: { gap: "10px", alignItems: "center" } }, h("span", { class: "small dim" }, "From the cell"), at, h("span", { class: "small dim" }, "at most"), limit, h("span", { class: "small dim" }, "rows")),
        err),
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go],
    });
    go.onclick = async () => {
      const a = parseRef(at.value);
      if (!a) { err.textContent = "The cell: as B2"; return; }
      go.disabled = true; err.textContent = "";
      try {
        const j = await api("/query", { body: { id: q?.id, sheet: sheetIdx < 0 ? st.sheet : sheetIdx, row: a.r0, col: a.c0, sql: sql.value, limit: +limit.value || undefined }, timeout: 300000 });
        take(j);
        if (j.error) { err.textContent = j.error; go.disabled = false; return; }
        m.close();
        const nq = j.book.queries.find((x) => x.sql === sql.value.trim().replace(/;$/, "")) || j.book.queries[j.book.queries.length - 1];
        if (nq) { grid.select(nq.row, nq.col, nq.row + Math.max(0, nq.rows - 1), nq.col + Math.max(0, nq.cols - 1)); grid.reveal(nq.row, nq.col); toast(`${Math.max(0, nq.rows - 1).toLocaleString()} rows in ${Math.round(nq.took_ms || 0)} ms`, "ok"); }
      } catch (e) { err.textContent = e.message; go.disabled = false; }
    };
    setTimeout(() => sql.focus(), 0);
  }
  async function refreshQueries() {
    if (!st.book.queries.length) { toast("This workbook has no queries: Queries, New query", "info"); return; }
    saving(true);
    try {
      const j = await api("/refresh", { body: {}, timeout: 600000 });
      take(j);
      const bad = j.errors || [];
      toast(bad.length ? `${bad.length} of ${st.book.queries.length} queries failed: ${bad[0].error}` : `Ran ${st.book.queries.length} ${st.book.queries.length === 1 ? "query" : "queries"} again`, bad.length ? "bad" : "ok");
    } catch (e) { toast(e.message, "bad"); } finally { saving(false); }
  }
  function queriesPanel() {
    const list = h("div");
    const draw = () => {
      const qs = st.book.queries;
      list.replaceChildren(qs.length ? h("ul", { class: "sx-hits" }, qs.map((q) => {
        const si = st.book.sheets.findIndex((s) => s.id === q.sheet);
        const where = `${si >= 0 ? st.book.sheets[si].name : "?"}!${cellRef(q.row, q.col)}`;
        return h("li", {},
          h("div", { style: { minWidth: 0 } }, h("div", { class: "mono small", style: { whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }, title: q.sql }, q.sql),
            h("div", { class: "small muted" }, `${where} · ${Math.max(0, q.rows - 1).toLocaleString()} rows · ${q.refreshed_ms ? core.fmt.ago(q.refreshed_ms) : "not run"}${q.error ? " · " + q.error : ""}`)),
          h("div", { class: "row nw", style: { gap: "2px" } },
            h("button", { class: "btn ghost sm icon", title: "Go to it", "aria-label": "Go to it", onclick: () => { if (si >= 0) { if (si !== st.sheet) openSheet(si, { r0: q.row, c0: q.col, r1: q.row, c1: q.col }); else { grid.select(q.row, q.col, q.row + Math.max(0, q.rows - 1), q.col + Math.max(0, q.cols - 1)); grid.reveal(q.row, q.col); } } } }, icon("right")),
            h("button", { class: "btn ghost sm icon", title: "Edit and run it again", "aria-label": "Edit", onclick: () => { d.close(); queryDialog(q); } }, icon("edit")),
            h("button", { class: "btn ghost sm icon", title: "Stop keeping it (the cells stay)", "aria-label": "Remove", onclick: async () => { const j = await api(`/queries/${encodeURIComponent(q.id)}`, { method: "DELETE" }); take(j); draw(); } }, icon("trash"))));
      })) : core.empty("sql", "No queries yet", "A query of the cluster's tables fills cells, and runs again when you refresh it."));
    };
    draw();
    const d = core.drawer({ title: "Queries", sub: "SQL whose answers fill cells of this workbook", body: list,
      foot: [h("button", { class: "btn", onclick: () => { refreshQueries().then(draw); } }, icon("refresh"), "Run all again"), h("button", { class: "btn primary", onclick: () => { d.close(); queryDialog(); } }, icon("plus"), "New query")] });
  }

  // ------------------------------------------------------------ charts, summaries, formats, names
  function chartDialog(c) {
    const a = c ? null : region();
    const rangeIn = h("input", { class: "input", value: c ? c.range : areaRef(a), "aria-label": "Data" });
    const titleIn = h("input", { class: "input", value: c?.title || "", placeholder: "No title", "aria-label": "Title" });
    let kind = c?.kind || "column";
    const kinds = core.seg([["column", "Column"], ["bar", "Bar"], ["line", "Line"], ["area", "Area"], ["pie", "Pie"], ["scatter", "Scatter"]], kind, (v) => { kind = v; });
    const stacked = h("input", { type: "checkbox", checked: !!c?.stacked });
    const go = h("button", { class: "btn primary" }, c ? "Save" : "Insert");
    const m = core.modal({ title: c ? "Edit the chart" : "Insert a chart",
      body: h("div", { class: "sx-form" },
        h("label", {}, "Data"), rangeIn,
        h("div", { class: "wide small dim" }, "The first row names the series and the first column holds the categories (for a scatter: x, then y)."),
        h("label", {}, "Kind"), kinds,
        h("label", {}, "Title"), titleIn,
        h("label", {}, "Stacked"), h("label", { class: "row nw", style: { gap: "6px" } }, stacked, h("span", { class: "small dim" }, "bars, columns and areas on top of each other"))),
      foot: [c ? h("button", { class: "btn danger", style: { marginRight: "auto" }, onclick: () => { m.close(); send([{ op: "delete_chart", id: c.id }]); } }, icon("trash"), "Delete") : null, h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go] });
    go.onclick = () => {
      const r = parseRef(rangeIn.value);
      if (!r) { toast("The data: a range, as A1:D20", "bad"); return; }
      m.close();
      const top = c ? { row: c.row, col: c.col } : { row: Math.max(1, a.r0), col: Math.min(MAX_COL, a.c1 + 2) };
      send([{ op: "chart", id: c?.id, sheet: st.sheet, range: areaRef(r), kind, title: titleIn.value, row: top.row, col: top.col, width: c?.width, height: c?.height, stacked: stacked.checked }]);
    };
  }
  charts.edit = (c) => chartDialog(c);

  function summarizeDialog() {
    const a = region();
    if (a.r1 <= a.r0) { toast("Select a table of cells: its first row names the columns", "info"); return; }
    const cols = [];
    for (let c = a.c0; c <= a.c1; c++) cols.push([c, src.cell(a.r0, c)?.text || colName(c)]);
    const groups = h("div", { class: "sx-checks" }, cols.map(([c, n]) => h("label", {}, h("input", { type: "checkbox", value: c, checked: c === a.c0 }), n)));
    const valueRows = h("div", { class: "stack", style: { gap: "6px" } });
    const addValue = (col, agg = "sum") => {
      const colSel = h("select", { "aria-label": "Column" }, cols.map(([c, n]) => h("option", { value: c, selected: c === col }, n)));
      const aggSel = h("select", { "aria-label": "How" }, [["sum", "Sum"], ["count", "Count"], ["avg", "Average"], ["min", "Min"], ["max", "Max"], ["countd", "Distinct values"]].map(([v, l]) => h("option", { value: v, selected: v === agg }, l)));
      const row = h("div", { class: "row nw", style: { gap: "6px" } }, aggSel, h("span", { class: "small dim" }, "of"), colSel, h("button", { class: "btn ghost sm icon", "aria-label": "Remove", onclick: () => row.remove() }, icon("x")));
      row.get = () => ({ col: +colSel.value, agg: aggSel.value });
      valueRows.append(row);
    };
    // a column of numbers to sum, to start from
    const numCol = cols.find(([c]) => c !== a.c0 && src.cell(a.r0 + 1, c)?.type?.[0] === "n");
    addValue(numCol ? numCol[0] : a.c0, numCol ? "sum" : "count");
    const go = h("button", { class: "btn primary" }, "Summarize");
    const m = core.modal({ title: `Summarize ${areaRef(a)}`, wide: true,
      body: h("div", { class: "stack", style: { gap: "12px" } },
        h("p", { class: "small dim", style: { margin: 0 } }, "A pivot table on a new sheet: one row for each group of values, and a total row."),
        h("div", {}, h("b", { class: "small" }, "Group by"), groups),
        h("div", {}, h("b", { class: "small" }, "Show"), valueRows, h("button", { class: "btn ghost sm", style: { marginTop: "6px" }, onclick: () => addValue(numCol ? numCol[0] : a.c0) }, icon("plus"), "Another value"))),
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go] });
    go.onclick = async () => {
      const by = [...groups.querySelectorAll("input:checked")].map((x) => +x.value);
      const values = [...valueRows.children].map((r) => r.get());
      if (!values.length) { toast("Choose a value to show", "bad"); return; }
      m.close();
      const j = await send([{ op: "summarize", sheet: st.sheet, area: rect(a), by, values, header: true }]);
      const k = j?.results?.[0]?.sheet;
      if (k != null) openSheet(k);
    };
  }

  function cfDialog() {
    const sel = usedArea();
    const rangeIn = h("input", { class: "input", value: areaRef(sel), "aria-label": "Range" });
    const kinds = [
      ["gt", "Greater than"], ["lt", "Less than"], ["between", "Between"], ["eq", "Equal to"], ["contains", "Text contains"], ["begins", "Text begins with"],
      ["dup", "Duplicate values"], ["unique", "Unique values"], ["top", "Top 10"], ["bottom", "Bottom 10"], ["above", "Above average"], ["below", "Below average"], ["blank", "Blank cells"], ["formula", "A formula is true"],
      ["scale2", "Color scale: white to green"], ["scale3", "Color scale: red, yellow, green"], ["bar", "Data bars"], ["icons", "Icons: arrows"],
    ];
    const kindSel = h("select", { "aria-label": "Rule" }, kinds.map(([v, n]) => h("option", { value: v }, n)));
    const v1 = h("input", { class: "input", placeholder: "Value", "aria-label": "Value" }), v2 = h("input", { class: "input", placeholder: "and", "aria-label": "Second value" });
    const looks = [["#9c0006", "#ffc7ce", "Red"], ["#006100", "#c6efce", "Green"], ["#9c5700", "#ffeb9c", "Yellow"], ["#1f3864", "#ddebf7", "Blue"], ["#000000", "#ffffff", "Bold"]];
    let look = 0;
    const lookSeg = core.seg(looks.map(([ink, bg, n], i) => [i, h("span", { style: { color: ink, background: bg, padding: "1px 6px", borderRadius: "3px", fontWeight: n === "Bold" ? 700 : 500 } }, n)]), 0, (v) => { look = v; });
    const valueRow = h("div", { class: "row nw", style: { gap: "6px" } }, v1, v2);
    const sync = () => {
      const k = kindSel.value;
      valueRow.style.display = ["gt", "lt", "between", "eq", "contains", "begins", "top", "bottom", "formula"].includes(k) ? "" : "none";
      v2.style.display = k === "between" ? "" : "none";
      v1.placeholder = k === "formula" ? `=${cellRef(sel.r0, sel.c0)}>100` : k === "top" || k === "bottom" ? "10" : "Value";
      lookRow.style.display = ["scale2", "scale3", "bar", "icons"].includes(k) ? "none" : "";
    };
    const lookRow = h("div", { class: "row nw", style: { gap: "8px", alignItems: "center" } }, h("span", { class: "small dim" }, "Look"), lookSeg);
    kindSel.onchange = sync;
    const existing = h("div");
    const drawExisting = () => {
      const list = st.sheets.get(st.sheet)?.layout?.cf || [];
      existing.replaceChildren(list.length ? h("ul", { class: "sx-hits" }, list.map((f) => h("li", {},
        h("div", {}, h("b", { class: "mono small" }, f.range), " ", h("span", { class: "small dim" }, describeRule(f.cf_rule))),
        h("button", { class: "btn ghost sm icon", "aria-label": "Delete the rule", title: "Delete the rule", onclick: async () => { await send([{ op: "delete_format", sheet: st.sheet, index: f.index }]); await loadLayout(st.sheet); drawExisting(); } }, icon("trash"))))) : h("p", { class: "small muted", style: { margin: 0 } }, "This sheet has no rules yet."));
    };
    const m = core.modal({ title: "Conditional formatting", wide: true,
      body: h("div", { class: "stack", style: { gap: "12px" } },
        h("div", { class: "sx-form" }, h("label", {}, "Cells"), rangeIn, h("label", {}, "When"), kindSel, h("label", {}), valueRow, h("label", {}), lookRow),
        h("div", {}, h("b", { class: "small" }, "Rules of this sheet"), existing)),
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Close"), h("button", { class: "btn primary", onclick: () => add() }, icon("plus"), "Add the rule")] });
    sync(); drawExisting();
    async function add() {
      const r = parseRef(rangeIn.value);
      if (!r) { toast("The cells: a range, as A1:D20", "bad"); return; }
      const [ink, bg, n] = looks[look];
      const format = n === "Bold" ? { font: { b: true } } : { font: { color: ink }, fill: { color: bg } };
      const k = kindSel.value, val = v1.value.trim(), lit2 = (x) => (x !== "" && !isNaN(+x) ? x : `"${x.replace(/"/g, '""')}"`);
      const cellIs = (operator, f1, f2 = null) => ({ type: "CellIs", operator, formula: f1, formula2: f2, format, stop_if_true: false });
      const rule = {
        gt: () => cellIs("GreaterThan", lit2(val)), lt: () => cellIs("LessThan", lit2(val)), eq: () => cellIs("Equal", lit2(val)),
        between: () => cellIs("Between", lit2(val), lit2(v2.value.trim())),
        contains: () => ({ type: "Text", operator: "Contains", value: val, format, stop_if_true: false }),
        begins: () => ({ type: "Text", operator: "BeginsWith", value: val, format, stop_if_true: false }),
        dup: () => ({ type: "DuplicateValues", format, stop_if_true: false }), unique: () => ({ type: "UniqueValues", format, stop_if_true: false }),
        top: () => ({ type: "Top10", rank: +val || 10, percent: false, format, stop_if_true: false }), bottom: () => ({ type: "Bottom10", rank: +val || 10, percent: false, format, stop_if_true: false }),
        above: () => ({ type: "AboveAverage", format, stop_if_true: false }), below: () => ({ type: "BelowAverage", format, stop_if_true: false }),
        blank: () => ({ type: "Blanks", format, stop_if_true: false }),
        formula: () => ({ type: "Formula", formula: val.replace(/^=/, ""), format, stop_if_true: false }),
        scale2: () => ({ type: "ColorScale", thresholds: [{ cfvo: "Min", color: "#ffffff" }, { cfvo: "Max", color: "#63be7b" }] }),
        scale3: () => ({ type: "ColorScale", thresholds: [{ cfvo: "Min", color: "#f8696b" }, { cfvo: { Percentile: 50 }, color: "#ffeb84" }, { cfvo: "Max", color: "#63be7b" }] }),
        bar: () => ({ type: "DataBar", min: null, max: null, positive_color: "#638ec6", negative_color: "#ff0000", is_gradient: false, show_value: true }),
        icons: () => ({ type: "IconSet", show_value: true, thresholds: [{ icon: "ArrowDown", cfvo: "Min", color: "#d93025", is_strict: false }, { icon: "ArrowRight", cfvo: { Percent: 33 }, color: "#eda100", is_strict: false }, { icon: "ArrowUp", cfvo: { Percent: 67 }, color: "#188038", is_strict: false }] }),
      }[k]();
      const j = await send([{ op: "add_format", sheet: st.sheet, range: areaRef(r), rule }]);
      if (j && !j.error) { await loadLayout(st.sheet); drawExisting(); toast("Rule added", "ok", 1500); }
    }
  }

  async function namesDialog(prefill) {
    const list = h("div");
    const nameIn = h("input", { class: "input", placeholder: "sales_2026", "aria-label": "Name" });
    const refIn2 = h("input", { class: "input", value: prefill ? `${quoteSheet(st.book.sheets[st.sheet].name)}!${absRef(prefill)}` : "", placeholder: "Sheet1!$A$1:$A$20", "aria-label": "Refers to" });
    const draw = () => list.replaceChildren(st.book.names.length ? h("ul", { class: "sx-hits" }, st.book.names.map((n) => h("li", {},
      h("div", {}, h("b", { class: "mono small" }, n.name), " ", h("span", { class: "small dim mono" }, n.formula), n.scope != null ? h("span", { class: "small muted" }, ` (only on ${st.book.sheets[n.scope]?.name || "a sheet"})`) : null),
      h("div", { class: "row nw", style: { gap: "2px" } },
        h("button", { class: "btn ghost sm icon", "aria-label": "Go to it", title: "Go to it", onclick: () => { m.close(); goTo(n.name); } }, icon("right")),
        h("button", { class: "btn ghost sm icon", "aria-label": "Delete the name", title: "Delete the name", onclick: async () => { await send([{ op: "delete_name", name: n.name, scope: n.scope }]); draw(); } }, icon("trash")))))) : h("p", { class: "small muted", style: { margin: 0 } }, "No names yet: a name stands for a range in formulas (=SUM(sales_2026))."));
    const m = core.modal({ title: "Named ranges", wide: true,
      body: h("div", { class: "stack", style: { gap: "12px" } }, list, h("div", { class: "row nw", style: { gap: "6px" } }, nameIn, refIn2, h("button", { class: "btn primary", onclick: async () => {
        if (!nameIn.value.trim() || !refIn2.value.trim()) return;
        const j = await send([{ op: "define_name", name: nameIn.value.trim(), formula: refIn2.value.trim().replace(/^=/, "") }]);
        if (j && !j.error) { nameIn.value = ""; draw(); }
      } }, icon("plus"), "Add"))),
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Close")] });
    draw();
    if (prefill) setTimeout(() => nameIn.focus(), 0);
  }
  const absRef = (s) => s.replace(/([A-Z]+)(\d+)/g, "$$$1$$$2");

  // ------------------------------------------------------------ the workbook as a file, or as a table
  async function download(format) {
    try {
      const r = await api(`/export?format=${format}${format === "csv" ? `&sheet=${st.sheet}` : ""}`, { blob: true, timeout: 120000 });
      const blob = await r.blob();
      const disp = r.headers.get("content-disposition") || "";
      const name = (disp.match(/filename="([^"]+)"/) || [])[1] || `${st.book.name}.${format}`;
      const a = h("a", { href: URL.createObjectURL(blob), download: name });
      document.body.append(a); a.click(); a.remove();
      setTimeout(() => URL.revokeObjectURL(a.href), 4000);
      leftOut(r, toast);
    } catch (e) { toast(e.message, "bad"); }
  }
  function toTableDialog(sheet = st.sheet) {
    const sh = st.book.sheets[sheet];
    const tname = (s) => { let n = String(s).trim().toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, ""); if (!n || /^\d/.test(n)) n = "t_" + n; return n; };
    const nameIn = h("input", { class: "input", value: tname(sh.name === "Sheet1" ? st.book.name : sh.name), "aria-label": "Table" });
    const s = st.sheets.get(sheet);
    const heads = [];
    for (let c = 1; c <= Math.min(200, sh.cols || 0); c++) { const t = s?.cells.get(1 * 20000 + c)?.text; heads.push(t ? tname(t).replace(/^t_/, "") || `col_${colName(c).toLowerCase()}` : `col_${colName(c).toLowerCase()}`); }
    const keySel = h("select", { "aria-label": "Key" }, h("option", { value: "" }, "row (the row's number)"), heads.map((n) => h("option", { value: n }, n)));
    const out = h("div", { class: "small" });
    const go = h("button", { class: "btn primary" }, icon("db"), "Save as a table");
    const m = core.modal({ title: `Save ${sh.name} as a ChloeDB table`,
      body: h("div", { class: "stack", style: { gap: "10px" } },
        h("p", { class: "small dim", style: { margin: 0 } }, "The first row names the columns; each row below it is a row of the table. Each column's type is what its cells hold (INT, FLOAT, BOOL or TEXT). Saving again writes the rows again (by their key)."),
        h("div", { class: "sx-form" }, h("label", {}, "Table"), nameIn, h("label", {}, "Key"), keySel), out),
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Close"), go] });
    go.onclick = async () => {
      go.disabled = true; out.textContent = "Writing the rows…";
      try {
        const j = await api("/to-table", { body: { sheet, table: nameIn.value.trim(), key: keySel.value || null }, timeout: 600000 });
        out.replaceChildren(h("div", { style: { color: "var(--ok-ink, var(--text))" } }, `Wrote ${j.rows.toLocaleString()} rows to ${j.table}. `, h("a", { href: ctx.href("t", j.table), onclick: () => m.close() }, "Open it live"), " · ", h("a", { href: `#/sql?q=${encodeURIComponent(`SELECT * FROM ${qid(j.table)} LIMIT 100`)}`, onclick: () => m.close() }, "Query it")),
          h("pre", { class: "code small", style: { marginTop: "6px", whiteSpace: "pre-wrap" } }, j.sql));
        go.disabled = false; go.textContent = "Save again";
      } catch (e) { out.textContent = e.message; out.style.color = "var(--crit-ink)"; go.disabled = false; }
    };
  }
  async function deleteBook() {
    if (!(await core.confirm({ title: `Delete ${st.book.name}?`, body: "Its sheets, charts and queries are deleted from ChloeDB. This cannot be undone.", ok: "Delete", danger: true }))) return;
    try { await ctx.api(`/v1/books/${encodeURIComponent(id)}`, { method: "DELETE" }); toast(`Deleted ${st.book.name}`, "ok"); location.hash = ctx.href(); } catch (e) { toast(e.message, "bad"); }
  }
  nameIn.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); nameIn.blur(); } if (e.key === "Escape") { nameIn.value = st.book?.name || ""; nameIn.blur(); } });
  nameIn.addEventListener("blur", () => { const v = nameIn.value.trim(); if (st.book && v && v !== st.book.name) send([{ op: "rename_book", name: v }]); else if (st.book) nameIn.value = st.book.name; });

  // ------------------------------------------------------------ opening, and following changes made elsewhere
  (async () => {
    try {
      const b = await api("", { timeout: 120000 });
      if (gone) return;
      st.version = b.version;
      const named = rest[0] != null ? b.sheets.findIndex((s) => s.name === rest[0]) : -1;
      const want = named >= 0 ? named : rest[0] != null && /^\d+$/.test(rest[0]) ? Math.min(+rest[0], b.sheets.length - 1) : 0;
      st.sheet = b.sheets[want]?.state === "visible" ? want : b.sheets.find((s) => s.state === "visible")?.index ?? 0;
      setBook(b);
      await loadLayout(st.sheet);
      const at = rest[1] ? parseRef(rest[1]) : null;
      grid.sizeSpacer();
      if (at) { grid.select(at.r0, at.c0, at.r1, at.c1); grid.reveal(at.r0, at.c0); } else grid.select(1, 1);
      grid.scroll.focus({ preventScroll: true });
      charts.refresh(true);
      document.title = `${b.name} · Sheets`;
    } catch (e) {
      if (gone) return;
      root.replaceChildren(core.failure ? core.failure(e, "The workbook") : h("p", {}, e.message), h("p", {}, h("a", { href: ctx.href() }, "All workbooks")));
    }
  })();
  // changes made in another window or by someone else
  stops.push(core.every(3000, async () => {
    if (!st.book || inflight || grid.editing) return;
    const j = await api(`/changes?since=${st.version}`, { timeout: 15000 });
    if (j.version > st.version) take(j);
  }, false));

  return () => {
    gone = true;
    document.removeEventListener("copy", onCopy); document.removeEventListener("cut", onCut); document.removeEventListener("paste", onPaste);
    for (const f of stops) { try { f(); } catch (e) { console.warn(e); } }
    findEl?.remove();
  };
}

// ---------------------------------------------------------------- small pieces of the page

/** A toolbar button with a glyph. */
function tb(core, glyph, title, onclick) {
  const b = core.h("button", { class: "sx-tb", title, "aria-label": title, type: "button", onclick: (e) => onclick(e) }, svgIcon(core, glyph));
  b.addEventListener("mousedown", (e) => e.preventDefault()); // keeps the grid's focus
  return b;
}
const sep = (h) => h("span", { class: "sep", "aria-hidden": "true" });
/** A toolbar button with a letter or a word. */
function tbText(core, label, title, onclick) {
  const b = core.h("button", { class: "sx-tb", title, "aria-label": title, type: "button", onclick: (e) => onclick(e) }, label);
  b.addEventListener("mousedown", (e) => e.preventDefault());
  return b;
}

/** A small layer under an element (closed by a click outside or Esc); returns its closer. */
function pop(core, anchor, content) {
  const { h } = core;
  const r = anchor.getBoundingClientRect();
  const el = h("div", { class: "sx-pop", role: "dialog" }, content);
  const scrim = h("div", { style: { position: "fixed", inset: 0, zIndex: 89 } });
  const prev = document.activeElement;
  let done = false;
  // closing gives the focus back (to the grid, mostly), so its keys work again
  const close = () => { if (done) return; done = true; el.remove(); scrim.remove(); removeEventListener("keydown", esc, true); if (prev?.isConnected && (!document.activeElement || document.activeElement === document.body)) prev.focus?.({ preventScroll: true }); };
  const esc = (e) => { if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); close(); } };
  scrim.onclick = close;
  document.body.append(scrim, el);
  addEventListener("keydown", esc, true);
  const w = el.offsetWidth, ht = el.offsetHeight;
  el.style.left = Math.max(8, Math.min(r.left, innerWidth - w - 8)) + "px";
  el.style.top = (r.bottom + 4 + ht > innerHeight - 8 && r.top - 4 - ht >= 8 ? r.top - 4 - ht : Math.max(8, Math.min(r.bottom + 4, innerHeight - 8 - ht))) + "px";
  return close;
}

/** Asks for one value in a dialog; resolves it, or null. */
function ask(core, { title, label, value = "", hint, type = "text" }) {
  const { h } = core;
  return new Promise((resolve) => {
    let answered = false;
    const inp = h("input", { class: "input", value, type, "aria-label": label, style: { width: "100%" } });
    const ok = h("button", { class: "btn primary" }, "OK");
    const m = core.modal({ title, body: h("div", { class: "stack", style: { gap: "6px" } }, h("label", { class: "small dim" }, label), inp, hint ? h("div", { class: "small muted mono" }, hint) : null),
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), ok], onClose: () => { if (!answered) resolve(null); } });
    const go = () => { answered = true; m.close(); resolve(inp.value.trim() || null); };
    ok.onclick = go;
    inp.onkeydown = (e) => { if (e.key === "Enter") { e.preventDefault(); go(); } };
    setTimeout(() => { inp.focus(); inp.select(); }, 0);
  });
}

/** A number format with one decimal more (d = 1) or less (d = -1). */
function decimals(nf, shown, d) {
  if (!nf || nf === "general") {
    const m = String(shown || "").match(/\.(\d+)/);
    const n = Math.max(0, (m ? m[1].length : 0) + d);
    return n ? "0." + "0".repeat(n) : "0";
  }
  return nf.split(";").map((sec) => {
    if (/^"[^"]*"$/.test(sec) || sec === "@") return sec;
    const m = sec.match(/0(\.0*)?(?!.*0)/);
    if (!m) return sec;
    const have = m[1] ? m[1].length - 1 : 0;
    const n = Math.max(0, have + d);
    return sec.slice(0, m.index) + (n ? "0." + "0".repeat(n) : "0") + sec.slice(m.index + m[0].length);
  }).join(";");
}

/** The number a cell shows (1,234.50 · 12% · $3), or null. */
function num(text) {
  const t = String(text).trim();
  let m = t.replace(/[,\s$€£¥]/g, "");
  let pct = false;
  if (m.endsWith("%")) { pct = true; m = m.slice(0, -1); }
  if (/^\(.*\)$/.test(m)) m = "-" + m.slice(1, -1);
  if (!/^[-+]?(\d+\.?\d*|\.\d+)(e[-+]?\d+)?$/i.test(m)) return null;
  const v = parseFloat(m);
  return pct ? v / 100 : v;
}
// The command center's number format (core.fmt.num), so a sum or a chart's
// value reads as the workbench's grid and charts show it; this one is the
// same rule, for a command center from before it.
let sharedNum = null, sharedTick = null;
function fmtNum(v) {
  if (sharedNum) return sharedNum(v);
  if (!isFinite(v)) return String(v);
  const a = Math.abs(v);
  if (a !== 0 && (a >= 1e15 || a < 1e-4)) return v.toExponential(4);
  return v.toLocaleString(undefined, { maximumFractionDigits: a >= 1000 ? 2 : 6 });
}

/** A conditional format's rule, in words. */
function describeRule(r) {
  if (!r || typeof r !== "object") return "";
  const t = r.type || Object.keys(r)[0];
  const x = r.type ? r : r[t] || {};
  const ops = { GreaterThan: ">", GreaterThanOrEqual: "≥", LessThan: "<", LessThanOrEqual: "≤", Equal: "=", NotEqual: "≠", Between: "between", NotBetween: "not between" };
  switch (t) {
    case "CellIs": return `value ${ops[x.operator] || x.operator} ${x.formula}${x.formula2 ? ` and ${x.formula2}` : ""}`;
    case "Text": return `text ${String(x.operator).replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase()} “${x.value}”`;
    case "ColorScale": return `color scale (${(x.thresholds || []).length} colors)`;
    case "DataBar": return "data bars";
    case "IconSet": return "icons";
    case "Top10": return `top ${x.rank}${x.percent ? "%" : ""}`;
    case "Bottom10": return `bottom ${x.rank}${x.percent ? "%" : ""}`;
    case "Formula": return `formula =${x.formula}`;
    default: return String(t).replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase();
  }
}

/** Says what an export left out (an xlsx keeps no charts), as the app's answer names it. */
function leftOut(r, toast) {
  const what = r?.headers?.get?.("x-app-left-out");
  if (what) toast(`The file leaves out ${what}: an Excel file from Sheets keeps values, formulas and styles, not charts`, "warn");
}

/** The tables /v1/tables lists, whatever its shape. */
function tableList(j) {
  const t = Array.isArray(j) ? j : j.tables || [];
  return t.map((x) => (typeof x === "string" ? { name: x } : { ...x, name: x.name || x.table })).filter((x) => x && x.name);
}

function shortcuts(core) {
  const { h } = core;
  const k = isMac ? "⌘" : "Ctrl+";
  const rows = [
    ["Enter, Tab", "the next cell down, right (Shift: up, left)"], ["F2, double click", "edit the cell"], ["Alt+Enter", "a new line in a cell"], [`${k}Enter`, "type into every selected cell"], [`${k}Shift+Enter`, "an array formula"],
    [`${k}Z, ${k}Y`, "undo, redo"], [`${k}C, ${k}X, ${k}V`, "copy, cut, paste (formulas and formats come along)"], ["Delete", "clear the cells"], [`${k}D, ${k}R`, "fill down, fill right"],
    [`${k}B, ${k}I, ${k}U, ${k}5`, "bold, italic, underline, strikethrough"], [`${k}Shift+$, %, !`, "currency, percent, number"], [`${k}\\`, "clear the formatting"],
    [`${k}Arrow`, "to the edge of the data"], [`${k}A`, "the data around the cell, then everything"], [`${k}Space, Shift+Space`, "the whole column, row"], [`${k}+, ${k}-`, "insert, delete the selected rows or columns"],
    ["Alt+=", "sum the numbers above"], [`${k};`, "today's date"], [`${k}F, ${k}H`, "find, replace"], [`${k}Shift+L`, "filter"], [`${k}PageDown, PageUp`, "the next, previous sheet"],
  ];
  core.modal({ title: "Keyboard shortcuts", wide: true, body: h("table", { class: "t compact" }, h("tbody", {}, rows.map(([a, b]) => h("tr", {}, h("td", { class: "mono", style: { whiteSpace: "nowrap" } }, a), h("td", {}, b))))) });
}

// ---------------------------------------------------------------- charts drawn over a sheet

const SERIES_COLORS = ["#2a78d6", "#eb6834", "#1baf7a", "#e87ba4", "#6250d6", "#eda100", "#0aa5b4", "#8d6e63"];

class ChartLayer {
  constructor(core, grid, st, api, send) {
    this.core = core; this.grid = grid; this.st = st; this.api = api; this.send = send;
    this.els = new Map(); // id -> {el, body, data, key}
    this.selected = null;
    this.edit = null;
    this.version = -1;
    this.refreshSoon = core.debounce(() => this.refresh(true), 400);
  }
  destroy() { for (const x of this.els.values()) x.el.remove(); this.els.clear(); }
  charts() {
    const b = this.st.book; if (!b) return [];
    const sid = b.sheets[this.st.sheet]?.id;
    return (b.charts || []).filter((c) => c.sheet === sid);
  }
  select(id) { this.selected = id; for (const [k, x] of this.els) x.el.classList.toggle("on", k === id); }
  /** Draws the charts of the sheet shown (reading their data again when the book changed, or with `force`). */
  refresh(force) {
    const { h } = this.core, list = this.charts(), seen = new Set();
    const changed = force || this.version !== this.st.version;
    this.version = this.st.version;
    for (const c of list) {
      seen.add(c.id);
      let x = this.els.get(c.id);
      if (!x) {
        const title = h("span", { class: "t" });
        const body = h("div", { class: "bd" });
        const el = h("div", { class: "sx-chart", tabIndex: 0, role: "figure" },
          h("div", { class: "hd" }, title,
            h("button", { class: "btn ghost sm icon", title: "Edit the chart", "aria-label": "Edit the chart", onclick: () => this.edit?.(this.byId(c.id)) }, this.core.icon("edit")),
            h("button", { class: "btn ghost sm icon", title: "Delete the chart", "aria-label": "Delete the chart", onclick: () => this.send([{ op: "delete_chart", id: c.id }]) }, this.core.icon("x"))),
          body, h("div", { class: "rs", title: "Resize" }));
        x = { el, body, title, data: null, key: "" };
        this.els.set(c.id, x);
        this.grid.layer.append(el);
        this.wire(x, c.id);
      }
      x.title.textContent = c.title || `${c.kind[0].toUpperCase()}${c.kind.slice(1)} of ${c.range}`;
      x.el.setAttribute("aria-label", x.title.textContent);
      const key = `${c.range}|${this.st.sheet}`;
      if (changed || x.key !== key || !x.data) { x.key = key; this.load(x, c); } else this.draw(x, c);
    }
    for (const [k, x] of this.els) if (!seen.has(k)) { x.el.remove(); this.els.delete(k); }
    this.position();
  }
  byId(id) { return (this.st.book.charts || []).find((c) => c.id === id); }
  async load(x, c) {
    try {
      const j = await this.api(`/values?sheet=${this.st.sheet}&range=${encodeURIComponent(c.range)}`, { timeout: 20000 });
      x.data = j.rows;
      this.draw(x, this.byId(c.id) || c);
    } catch (e) { x.body.replaceChildren(this.core.h("div", { class: "small muted", style: { padding: "10px" } }, e.message)); }
  }
  /** Places the charts at their cells (and clips them to the scrolled part of the grid). */
  position() {
    const g = this.grid, s = g.src, f = g.frozen(), sc = g.scroll;
    for (const c of this.charts()) {
      const x = this.els.get(c.id); if (!x) continue;
      if (x.dragging) continue;
      x.el.style.left = s.colAxis.pos(c.col) + "px";
      x.el.style.top = s.rowAxis.pos(c.row) + "px";
      x.el.style.width = c.width + "px"; x.el.style.height = c.height + "px";
    }
    g.layer.style.clipPath = `inset(${sc.scrollTop + f.fh}px -100000px -100000px ${sc.scrollLeft + f.fw}px)`;
  }
  wire(x, id) {
    const g = this.grid, s = g.src;
    x.el.addEventListener("pointerdown", (e) => { e.stopPropagation(); this.select(id); });
    x.el.addEventListener("keydown", (e) => { if ((e.key === "Delete" || e.key === "Backspace") && e.target === x.el) { e.preventDefault(); this.send([{ op: "delete_chart", id }]); } e.stopPropagation(); });
    x.el.addEventListener("dblclick", (e) => { e.stopPropagation(); this.edit?.(this.byId(id)); });
    const hd = x.el.querySelector(".hd"), rs = x.el.querySelector(".rs");
    const follow = (e, move, end) => {
      e.preventDefault(); e.stopPropagation();
      const x0 = e.clientX, y0 = e.clientY;
      x.dragging = true;
      const mv = (ev) => move(ev.clientX - x0, ev.clientY - y0);
      const up = (ev) => { removeEventListener("pointermove", mv); removeEventListener("pointerup", up); x.dragging = false; end(ev.clientX - x0, ev.clientY - y0); };
      addEventListener("pointermove", mv); addEventListener("pointerup", up);
    };
    hd.addEventListener("pointerdown", (e) => {
      if (e.target.closest("button")) return;
      const c = this.byId(id); if (!c) return;
      const left = s.colAxis.pos(c.col), top = s.rowAxis.pos(c.row);
      follow(e, (dx, dy) => { x.el.style.left = left + dx + "px"; x.el.style.top = top + dy + "px"; }, (dx, dy) => {
        if (Math.abs(dx) < 3 && Math.abs(dy) < 3) { this.position(); return; }
        const col = Math.max(1, s.colAxis.at(Math.max(0, left + dx), MAX_COL)), row = Math.max(1, s.rowAxis.at(Math.max(0, top + dy), MAX_ROW));
        c.col = col; c.row = row; this.position();
        this.send([{ op: "chart", ...this.spec(c), row, col }]);
      });
    });
    rs.addEventListener("pointerdown", (e) => {
      const c = this.byId(id); if (!c) return;
      const w0 = c.width, h0 = c.height;
      follow(e, (dx, dy) => { x.el.style.width = Math.max(200, w0 + dx) + "px"; x.el.style.height = Math.max(140, h0 + dy) + "px"; this.draw(x, c, Math.max(200, w0 + dx), Math.max(140, h0 + dy)); }, (dx, dy) => {
        c.width = Math.max(200, Math.round(w0 + dx)); c.height = Math.max(140, Math.round(h0 + dy)); this.position();
        this.send([{ op: "chart", ...this.spec(c) }]);
      });
    });
  }
  spec(c) { return { id: c.id, sheet: this.st.sheet, range: c.range, kind: c.kind, title: c.title, row: c.row, col: c.col, width: c.width, height: c.height, stacked: !!c.stacked }; }

  /** The series of a range's values: the first row names them, the first column holds the categories. */
  static series(rows, kind) {
    rows = (rows || []).filter((r) => r.some(([s]) => s !== "" && s != null));
    if (!rows.length) return { cats: [], series: [] };
    const C = Math.max(...rows.map((r) => r.length));
    const isNum = (x) => x && typeof x[1] === "number";
    const header = rows.length > 1 && rows[0].some((x, j) => (j > 0 || C === 1) && x && x[0] !== "" && !isNum(x));
    const body = rows.slice(header ? 1 : 0);
    const cats = kind === "scatter" ? C > 1 : C > 1 && body.some((r) => r[0] && r[0][0] !== "" && !isNum(r[0]));
    const first = cats || (kind === "scatter" && C > 1) ? 1 : 0;
    const series = [];
    for (let j = first; j < C; j++) series.push({ name: header ? String(rows[0][j]?.[0] ?? `Series ${series.length + 1}`) : `Series ${series.length + 1}`, values: body.map((r) => (isNum(r[j]) ? r[j][1] : null)) });
    const labels = body.map((r, i) => (first ? String(r[0]?.[0] ?? "") : String(i + 1)));
    const xs = kind === "scatter" && first ? body.map((r) => (isNum(r[0]) ? r[0][1] : null)) : null;
    return { cats: labels, series, xs };
  }

  draw(x, c, W, H) {
    if (!x.data || !c) return;
    const core = this.core, svg = core.s("svg", { role: "img", "aria-label": x.title.textContent });
    const box = x.body.getBoundingClientRect();
    W = (W ? W - 12 : box.width) || c.width - 12; H = (H ? H - 34 : box.height) || c.height - 34;
    svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
    const { cats, series, xs } = ChartLayer.series(x.data, c.kind);
    const S = (tag, a, ...k) => core.s(tag, a, ...k);
    const ink = getComputedStyle(document.documentElement).getPropertyValue("--grid").trim() || "#e8e9ec";
    if (!series.length || !series.some((s) => s.values.some((v) => v != null))) {
      svg.append(S("text", { x: W / 2, y: H / 2, "text-anchor": "middle" }, "No numbers in " + c.range));
      x.body.replaceChildren(svg); return;
    }
    const legend = series.length > 1 || c.kind === "pie";
    const lg = legend ? 18 : 0;
    if (c.kind === "pie") {
      const vals = series[0].values.map((v) => Math.max(0, v || 0)), total = vals.reduce((a, b) => a + b, 0) || 1;
      const r = Math.max(10, Math.min(W, H - lg) / 2 - 8), cx = W / 2, cy = (H - lg) / 2 + 2;
      let a0 = -Math.PI / 2;
      vals.forEach((v, i) => {
        const a1 = a0 + (v / total) * Math.PI * 2;
        const large = a1 - a0 > Math.PI ? 1 : 0;
        const p = v >= total ? `M${cx} ${cy - r}A${r} ${r} 0 1 1 ${cx - 0.01} ${cy - r}Z` : `M${cx} ${cy}L${cx + r * Math.cos(a0)} ${cy + r * Math.sin(a0)}A${r} ${r} 0 ${large} 1 ${cx + r * Math.cos(a1)} ${cy + r * Math.sin(a1)}Z`;
        const path = S("path", { d: p, fill: SERIES_COLORS[i % SERIES_COLORS.length], stroke: "var(--surface)", "stroke-width": 1.5 });
        path.append(S("title", {}, `${cats[i]}: ${fmtNum(v)} (${((v / total) * 100).toFixed(1)}%)`));
        svg.append(path);
        if (v / total > 0.06) { const am = (a0 + a1) / 2; svg.append(S("text", { x: cx + r * 0.65 * Math.cos(am), y: cy + r * 0.65 * Math.sin(am) + 4, "text-anchor": "middle", style: "fill:#fff;font-weight:600" }, `${Math.round((v / total) * 100)}%`)); }
        a0 = a1;
      });
      this.legend(svg, cats.slice(0, 8).map((n, i) => [n, SERIES_COLORS[i % SERIES_COLORS.length]]), W, H);
      x.body.replaceChildren(svg); return;
    }
    const horiz = c.kind === "bar";
    const stacked = !!c.stacked && ["bar", "column", "area"].includes(c.kind);
    const n = cats.length;
    let lo = 0, hi = 0;
    if (stacked) for (let i = 0; i < n; i++) { let p = 0, q = 0; for (const s of series) { const v = s.values[i] || 0; if (v >= 0) p += v; else q += v; } hi = Math.max(hi, p); lo = Math.min(lo, q); }
    else for (const s of series) for (const v of s.values) if (v != null) { hi = Math.max(hi, v); lo = Math.min(lo, v); }
    if (c.kind === "line" || c.kind === "scatter") { const all = series.flatMap((s) => s.values).filter((v) => v != null); lo = Math.min(...all); hi = Math.max(...all); if (lo > 0 && lo < hi * 0.5) lo = 0; if (lo === hi) { lo -= 1; hi += 1; } }
    const step = niceStep(hi - lo || 1, horiz ? Math.max(2, Math.floor(W / 90)) : Math.max(2, Math.floor((H - lg) / 40)));
    lo = Math.floor(lo / step) * step; hi = Math.ceil(hi / step) * step; if (hi === lo) hi = lo + step;
    const ticks = []; for (let t = lo; t <= hi + step / 2; t += step) ticks.push(+t.toFixed(10));
    const tl = (v) => sharedTick ? sharedTick(v) : (Math.abs(v) >= 1e6 ? (v / 1e6).toFixed(Math.abs(v) >= 1e7 ? 0 : 1) + "M" : Math.abs(v) >= 1e3 ? (v / 1e3).toFixed(Math.abs(v) >= 1e4 ? 0 : 1) + "k" : String(+v.toFixed(4)));
    const left = horiz ? Math.min(120, 10 + Math.max(...cats.map((s) => String(s).length)) * 6) : 10 + Math.max(...ticks.map((t) => tl(t).length)) * 6.5, bottom = horiz ? 18 : 20, top = 6 + lg, right = 10;
    const pw = Math.max(10, W - left - right), ph = Math.max(10, H - top - bottom);
    const val = (v) => (horiz ? left + ((v - lo) / (hi - lo)) * pw : top + ph - ((v - lo) / (hi - lo)) * ph);
    // grid lines and value labels
    for (const t of ticks) {
      const p = val(t);
      svg.append(horiz ? S("line", { x1: p, x2: p, y1: top, y2: top + ph, stroke: ink }) : S("line", { x1: left, x2: left + pw, y1: p, y2: p, stroke: ink }));
      svg.append(horiz ? S("text", { x: p, y: top + ph + 13, "text-anchor": "middle" }, tl(t)) : S("text", { x: left - 5, y: p + 3.5, "text-anchor": "end" }, tl(t)));
    }
    const color = (i) => SERIES_COLORS[i % SERIES_COLORS.length];
    const tip = (el, text) => { el.append(S("title", {}, text)); return el; };
    if (c.kind === "scatter") {
      const xv = (xs || cats.map((_, i) => i + 1)).map((v) => (v == null ? null : +v));
      const xl = Math.min(...xv.filter((v) => v != null)), xh = Math.max(...xv.filter((v) => v != null));
      const xstep = niceStep(xh - xl || 1, Math.max(2, Math.floor(W / 90)));
      const x0 = Math.floor(xl / xstep) * xstep, x1 = Math.ceil(xh / xstep) * xstep || x0 + xstep;
      const px = (v) => left + ((v - x0) / (x1 - x0 || 1)) * pw;
      for (let t = x0; t <= x1 + xstep / 2; t += xstep) svg.append(S("text", { x: px(t), y: top + ph + 14, "text-anchor": "middle" }, tl(+t.toFixed(10))));
      series.forEach((s, si) => s.values.forEach((v, i) => { if (v == null || xv[i] == null) return; svg.append(tip(S("circle", { cx: px(xv[i]), cy: val(v), r: 3.2, fill: color(si), "fill-opacity": 0.8 }), `${s.name}: (${fmtNum(xv[i])}, ${fmtNum(v)})`)); }));
    } else if (c.kind === "line" || c.kind === "area") {
      const px = (i) => left + (n === 1 ? pw / 2 : (i / (n - 1)) * pw);
      const base = new Array(n).fill(0);
      series.forEach((s, si) => {
        const pts = s.values.map((v, i) => { const y = stacked ? (base[i] += v || 0) : v; return y == null ? null : [px(i), val(y)]; });
        const d = pts.reduce((acc, p, i) => (p ? acc + (acc && pts[i - 1] ? "L" : "M") + p[0].toFixed(1) + " " + p[1].toFixed(1) : acc), "");
        if (c.kind === "area") {
          const lowY = val(Math.max(lo, 0));
          const valid = pts.filter(Boolean);
          if (valid.length) svg.append(S("path", { d: `${d}L${valid[valid.length - 1][0]} ${lowY}L${valid[0][0]} ${lowY}Z`, fill: color(si), "fill-opacity": stacked ? 0.55 : 0.18 }));
        }
        svg.append(S("path", { d, fill: "none", stroke: color(si), "stroke-width": 2, "stroke-linejoin": "round" }));
        if (n <= 60) pts.forEach((p, i) => p && svg.append(tip(S("circle", { cx: p[0], cy: p[1], r: 2.6, fill: color(si) }), `${s.name} · ${cats[i]}: ${fmtNum(s.values[i])}`)));
      });
      const every = Math.max(1, Math.ceil(n / Math.max(1, Math.floor(pw / 70))));
      cats.forEach((cat, i) => { if (i % every === 0) svg.append(S("text", { x: px(i), y: top + ph + 14, "text-anchor": "middle" }, String(cat).slice(0, 14))); });
    } else {
      const band = (horiz ? ph : pw) / Math.max(1, n), inner = band * 0.78, k = stacked ? 1 : series.length, bw = Math.max(1, inner / k);
      const zero = val(Math.max(lo, Math.min(0, hi)));
      for (let i = 0; i < n; i++) {
        let pos = 0, neg = 0;
        series.forEach((s, si) => {
          const v = s.values[i]; if (v == null) return;
          let a, b;
          if (stacked) { if (v >= 0) { a = val(pos); b = val(pos + v); pos += v; } else { a = val(neg); b = val(neg + v); neg += v; } } else { a = zero; b = val(v); }
          const off = (band - inner) / 2 + (stacked ? 0 : si * bw);
          const r = horiz ? S("rect", { x: Math.min(a, b), y: top + i * band + off, width: Math.max(1, Math.abs(b - a)), height: bw - 1, fill: color(si), rx: 1.5 })
            : S("rect", { x: left + i * band + off, y: Math.min(a, b), width: bw - 1, height: Math.max(1, Math.abs(b - a)), fill: color(si), rx: 1.5 });
          svg.append(tip(r, `${s.name} · ${cats[i]}: ${fmtNum(v)}`));
        });
      }
      const every = Math.max(1, Math.ceil(n / Math.max(1, Math.floor((horiz ? ph / 14 : pw / 60)))));
      cats.forEach((cat, i) => {
        if (i % every) return;
        svg.append(horiz ? S("text", { x: left - 5, y: top + (i + 0.5) * band + 3.5, "text-anchor": "end" }, String(cat).slice(0, 18)) : S("text", { x: left + (i + 0.5) * band, y: top + ph + 14, "text-anchor": "middle" }, String(cat).slice(0, 12)));
      });
    }
    svg.append(horiz ? S("line", { x1: val(Math.max(lo, Math.min(0, hi))), x2: val(Math.max(lo, Math.min(0, hi))), y1: top, y2: top + ph, stroke: "var(--line-2)" }) : S("line", { x1: left, x2: left + pw, y1: val(Math.max(lo, Math.min(0, hi))), y2: val(Math.max(lo, Math.min(0, hi))), stroke: "var(--line-2)" }));
    if (legend) this.legend(svg, series.slice(0, 8).map((s, i) => [s.name, color(i)]), W, H);
    x.body.replaceChildren(svg);
  }
  legend(svg, items, W) {
    const S = (tag, a, ...k) => this.core.s(tag, a, ...k);
    let xx = 4;
    for (const [name, col] of items) {
      const label = String(name).slice(0, 18);
      if (xx + 14 + label.length * 6 > W) break;
      svg.append(S("rect", { x: xx, y: 3, width: 9, height: 9, rx: 2, fill: col }), S("text", { x: xx + 13, y: 11 }, label));
      xx += 22 + label.length * 6;
    }
  }
}

function niceStep(span, n) {
  const raw = span / Math.max(1, n);
  const mag = 10 ** Math.floor(Math.log10(raw || 1));
  const f = raw / mag;
  return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10) * mag;
}

// ---------------------------------------------------------------- a table of the cluster, live

const TPAGE = 100;

/** What the grid draws of a cluster table: its columns, and the rows read so far (by page). */
class TableSource {
  constructor(t) { this.t = t; this.rowAxis = new Axis(25); this.colAxis = new Axis(140); }
  rows() { return Math.max(1, this.t.count ?? 1); }
  cols() { return Math.max(1, this.t.fields.length); }
  frozen() { return { rows: 0, cols: this.t.pk === 0 ? 1 : 0 }; }
  cell(r, c) {
    const row = this.t.pages.get(Math.floor((r - 1) / TPAGE))?.[(r - 1) % TPAGE];
    if (!row) return null;
    const v = row[c - 1], f = this.t.fields[c - 1];
    if (v == null) return { text: "", type: "s", st: null };
    const pk = c - 1 === this.t.pk;
    const edited = this.t.edited.get(`${r}:${c}`);
    const st = pk ? { b: 1 } : edited ? { bg: "rgba(237, 161, 0, .22)" } : null;
    if (typeof v === "number") return { text: f.type === "float" || !Number.isInteger(v) ? fmtNum(v) : String(v), type: "n", st, raw: v };
    if (typeof v === "boolean") return { text: v ? "TRUE" : "FALSE", type: "b", st, raw: v };
    if (typeof v === "object") return { text: JSON.stringify(v).slice(0, 300), type: "s", st, raw: v };
    return { text: String(v).length > 400 ? String(v).slice(0, 400) + "…" : String(v), type: "s", st, raw: v };
  }
  need(r0, r1) { this.t.need(r0, r1); }
  colLabel(c) { return this.t.fields[c - 1]?.name || colName(c); }
  colTag(c) { const f = this.t.fields[c - 1]; return f ? (c - 1 === this.t.pk ? `${f.type} · key` : f.type) : ""; }
  sortAt(c) { return this.t.sort && this.t.sort.col === c - 1 ? (this.t.sort.desc ? "desc" : "asc") : null; }
  sheetKey() { return "t"; }
}

function tablePage(root, ctx, table) {
  const core = ctx.core, { h, icon, toast } = core;
  const sapi = (p, o) => core.api(p, o);
  let gone = false;
  const stops = [];
  const t = { name: table, fields: [], pk: -1, count: null, pages: new Map(), pending: new Set(), where: "", search: "", searchField: "", sort: null, epoch: 0, edited: new Map(), schema: null, window: 10000, anchors: new Map(), pastNote: false };
  const src = new TableSource(t);
  const grid = new Grid(core, src, {});
  const count = h("span", { class: "sx-saved" }, "Reading…");
  // The workbench opened on this table, as filtered and sorted here.
  const sqlLink = h("a", { class: "btn sm", title: "Query the table in the SQL workbench", href: "#/sql", onclick: () => { sqlLink.href = "#/sql?q=" + encodeURIComponent(`${sqlOf()} LIMIT 100;`); } }, icon("sql"), "SQL");
  const whereIn = h("input", { class: "input", placeholder: "WHERE: price > 10 AND brand = 'Acme'", "aria-label": "Rows where (SQL)", spellcheck: "false", style: { flex: "2 1 260px", fontFamily: "var(--mono)", fontSize: "12.5px" } });
  const searchIn = h("input", { class: "input", type: "search", placeholder: "Search the text", "aria-label": "Search the text", style: { flex: "1 1 160px" } });
  const fieldSel = h("select", { class: "select", "aria-label": "The text field searched" });
  const top = h("div", { class: "sx-top" },
    h("a", { class: "btn ghost sm", href: ctx.href(), title: "All workbooks and tables" }, icon("left"), "Sheets"),
    h("span", { class: "sx-book-ic t", "aria-hidden": "true" }, icon("db")),
    h("b", { style: { fontSize: "16px" } }, table), count, h("span", { class: "sep" }),
    h("button", { class: "btn sm", title: "Add a row", onclick: () => addRow() }, icon("plus"), "Row"),
    h("button", { class: "btn sm", title: "Add a column (ALTER TABLE)", onclick: () => addColumn() }, icon("plus"), "Column"),
    h("button", { class: "btn sm", title: "Delete the selected rows", onclick: () => deleteRows() }, icon("trash"), "Delete rows"),
    h("button", { class: "btn sm", title: "Copy these rows into a new workbook (formulas, charts, pivots)", onclick: () => toBook() }, icon("copy"), "Open as a workbook"),
    h("button", { class: "btn sm", title: "Read the rows again", "aria-label": "Refresh", onclick: () => reload() }, icon("refresh")),
    sqlLink);
  const note = h("div", { class: "sx-note", role: "status", hidden: true });
  const bar = h("div", { class: "sx-tbar" }, svgIcon(core, G.filter), whereIn, searchIn, fieldSel,
    h("button", { class: "btn sm", onclick: () => { whereIn.value = ""; searchIn.value = ""; apply(); } }, "Clear"));
  const foot = h("div", { class: "sx-foot" }, h("div", { class: "sx-stats", style: { flex: 1 } }, h("span", { class: "muted" }, "A table of the cluster: edits are written to ChloeDB as you make them, and the rows are read again every 5 seconds")));
  const stats = h("div", { class: "sx-stats" });
  foot.append(stats);
  root.append(h("div", { class: "sx-book" }, top, bar, note, grid.el, foot));
  stops.push(() => grid.destroy());

  const sel = () => norm(grid.sel);
  const isText = (f) => f.type === "text";
  function query(extra = "") {
    const conds = [];
    if (t.where.trim()) conds.push(`(${t.where.trim()})`);
    if (t.search.trim() && t.searchField) conds.push(`MATCH(${qid(t.searchField)}, ${lit(t.search.trim())})`);
    const cols = t.fields.map((f) => qid(f.name)).join(", ") || "*";
    const order = t.sort ? `${qid(t.fields[t.sort.col].name)} ${t.sort.desc ? "DESC" : "ASC"}${t.pk >= 0 && t.sort.col !== t.pk ? `, ${qid(t.fields[t.pk].name)}` : ""}` : t.pk >= 0 ? qid(t.fields[t.pk].name) : "";
    return { cols, from: qid(t.name), where: conds.length ? ` WHERE ${conds.join(" AND ")}` : "", order: order ? ` ORDER BY ${order}` : "", extra };
  }
  const sqlOf = () => { const q = query(); return `SELECT ${q.cols} FROM ${q.from}${q.where}${q.order}`; };
  // Past the server's result window (--max-result-window, 10,000 rows by
  // default) OFFSET is refused, so rows further on are read by key: the
  // sort's values of the row before each block of `window` rows (an anchor)
  // are read once, each from the one before, and a page is the rows after
  // its block's anchor, at an offset inside the window.
  const RANGED = new Set(["int", "float", "timestamp", "bool"]);
  const sortCols = () => {
    if (t.pk < 0) return null;
    const by = t.sort ? t.sort.col : t.pk;
    if (!RANGED.has(t.fields[by]?.type) || !RANGED.has(t.fields[t.pk].type)) return null;
    return by === t.pk ? [{ col: t.pk, desc: !!t.sort?.desc }] : [{ col: by, desc: !!t.sort.desc }, { col: t.pk, desc: false }];
  };
  const sqlv = (v) => typeof v === "number" || typeof v === "boolean" ? String(v) : lit(v);
  // The rows after `a` (the sort's values of a row) in the sort's order;
  // rows without a value come last, either way.
  function after(keys, a) {
    const [k0, k1] = keys, c0 = qid(t.fields[k0.col].name), op0 = k0.desc ? "<" : ">";
    if (!k1) return `${c0} ${op0} ${sqlv(a[0])}`;
    const c1 = qid(t.fields[k1.col].name);
    if (a[0] == null) return `(${c0} IS NULL AND ${c1} > ${sqlv(a[1])})`;
    return `(${c0} ${op0} ${sqlv(a[0])} OR (${c0} = ${sqlv(a[0])} AND ${c1} > ${sqlv(a[1])}) OR ${c0} IS NULL)`;
  }
  const block = () => Math.max(TPAGE, Math.floor(t.window / TPAGE) * TPAGE);
  // The anchor of block b (b >= 1): the sort's values of row b * block().
  function anchor(b, keys, epoch) {
    if (t.anchors.has(b)) return t.anchors.get(b);
    const B = block();
    const last = t.pages.get(b * B / TPAGE - 1)?.[TPAGE - 1];
    const got = last ? Promise.resolve(keys.map((k) => last[k.col])) : (async () => {
      const prev = b > 1 ? await anchor(b - 1, keys, epoch) : null;
      const q = query();
      const conds = [q.where.replace(/^ WHERE /, ""), prev ? after(keys, prev) : ""].filter(Boolean);
      const j = await sapi("/v1/query", { body: { sql: `SELECT ${keys.map((k) => qid(t.fields[k.col].name)).join(", ")} FROM ${q.from}${conds.length ? ` WHERE ${conds.join(" AND ")}` : ""}${q.order} LIMIT 1 OFFSET ${B - 1}` }, timeout: 30000 });
      if (!j.rows?.length) throw new Error("the table has fewer rows than counted: refresh");
      return j.rows[0];
    })();
    if (epoch === t.epoch) t.anchors.set(b, got);
    got.catch(() => { if (t.anchors.get(b) === got) t.anchors.delete(b); });
    return got;
  }
  function pageSql(p, epoch) {
    const B = block(), at = p * TPAGE;
    if (at + TPAGE <= B) return Promise.resolve(`${sqlOf()} LIMIT ${TPAGE} OFFSET ${at}`);
    const keys = sortCols();
    // no key to read by: OFFSET, which a server answers past its window for
    // some orders (a KEYWORD key's), until it refuses once
    if (!keys) return Promise.resolve(t.pastNote ? null : `${sqlOf()} LIMIT ${TPAGE} OFFSET ${at}`);
    const b = Math.floor(at / B);
    return anchor(b, keys, epoch).then((a) => {
      const q = query(), conds = [q.where.replace(/^ WHERE /, ""), after(keys, a)].filter(Boolean);
      return `SELECT ${q.cols} FROM ${q.from} WHERE ${conds.join(" AND ")}${q.order} LIMIT ${TPAGE} OFFSET ${at - b * B}`;
    });
  }
  // Said once, in place of the rows the server will not page to.
  function pastWindow() {
    if (t.pastNote) return;
    t.pastNote = true;
    const why = t.pk < 0 ? "the table has no primary key to read by" : "the sort is by a column of text";
    note.textContent = `The rows past ${block().toLocaleString()} are not shown: the server reads at most ${t.window.toLocaleString()} rows a query, and ${why}. Narrow them with a WHERE${t.pk >= 0 ? ", or sort by the key or a number" : ""}.`;
    note.hidden = false;
  }
  t.need = (r0, r1) => {
    if (!t.fields.length) return;
    const p0 = Math.floor((r0 - 1) / TPAGE), p1 = Math.floor((r1 - 1) / TPAGE);
    for (let p = p0; p <= p1; p++) if (!t.pages.has(p) && !t.pending.has(p)) readPage(p);
  };
  async function readPage(p, quiet) {
    t.pending.add(p);
    const epoch = t.epoch;
    try {
      const sql = await pageSql(p, epoch);
      if (gone || epoch !== t.epoch) return;
      if (!sql) { pastWindow(); return; }
      let j;
      try { j = await sapi("/v1/query", { body: { sql }, timeout: 30000 }); } catch (e) {
        // A server with a smaller window than thought says how big it is.
        const m = /returns at most (\d+) rows/.exec(e.message || "");
        if (m && +m[1] < t.window && !gone && epoch === t.epoch) { t.window = Math.max(TPAGE, +m[1]); t.anchors.clear(); t.pending.delete(p); return readPage(p, quiet); }
        if (m && !sortCols()) { pastWindow(); return; }
        throw e;
      }
      if (gone || epoch !== t.epoch) return;
      const before = JSON.stringify(t.pages.get(p) || null);
      t.pages.set(p, j.rows);
      if (!quiet || before !== JSON.stringify(j.rows)) grid.draw();
    } catch (e) { if (!gone && !quiet) { toast(e.message, "bad"); count.textContent = e.message; count.classList.add("bad"); } }
    finally { t.pending.delete(p); }
  }
  async function readCount() {
    const q = query();
    const j = await sapi("/v1/query", { body: { sql: `SELECT COUNT(*) AS n FROM ${q.from}${q.where}` }, timeout: 30000 });
    const n = j.rows?.[0]?.[0] ?? 0;
    const changed = n !== t.count;
    // rows came or went: the anchors past them have moved
    if (changed && t.count != null) t.anchors.clear();
    t.count = n;
    count.classList.remove("bad");
    count.textContent = `${n.toLocaleString()} ${n === 1 ? "row" : "rows"}${t.where || t.search ? " match" : ""}`;
    if (changed) { grid.sizeSpacer(); grid.draw(); }
  }
  async function reload() {
    t.epoch++; t.pages.clear(); t.pending.clear(); t.anchors.clear(); t.pastNote = false; note.hidden = true;
    try { await readCount(); } catch (e) { count.textContent = e.message; count.classList.add("bad"); return; }
    grid.draw();
  }
  function apply() {
    t.where = whereIn.value; t.search = searchIn.value; t.searchField = fieldSel.value;
    reload();
  }
  whereIn.addEventListener("keydown", (e) => { if (e.key === "Enter") apply(); });
  searchIn.addEventListener("input", core.debounce(apply, 350));
  fieldSel.onchange = apply;

  // ------------------------------------------------------------ editing rows
  const keyOf = (r) => { const row = t.pages.get(Math.floor((r - 1) / TPAGE))?.[(r - 1) % TPAGE]; return row && t.pk >= 0 ? row[t.pk] : null; };
  const rowOf = (r) => t.pages.get(Math.floor((r - 1) / TPAGE))?.[(r - 1) % TPAGE];
  function valueFor(f, text) {
    const s = String(text);
    if (s.trim() === "") return null;
    if (f.type === "int") { if (!/^[-+]?\d+$/.test(s.trim())) throw new Error(`${f.name} holds whole numbers`); return parseInt(s, 10); }
    if (f.type === "float") { const v = num(s); if (v == null) throw new Error(`${f.name} holds numbers`); return v; }
    if (f.type === "bool") { const v = s.trim().toLowerCase(); if (["true", "1", "yes", "y"].includes(v)) return true; if (["false", "0", "no", "n"].includes(v)) return false; throw new Error(`${f.name} holds TRUE or FALSE`); }
    if (f.type === "json" || f.type.startsWith("vector") || f.type.startsWith("array") || f.type === "geo") { try { return JSON.parse(s); } catch { throw new Error(`${f.name} holds JSON`); } }
    return s;
  }
  async function write(ops, what) {
    try {
      await sapi("/v1/write", { body: { ops }, timeout: 60000 });
      return true;
    } catch (e) { toast(`${what}: ${e.message}`, "bad"); return false; }
  }
  /** Sets cells of rows: [[r, c, text]]. */
  async function setCells(list) {
    const ops = [];
    for (const [r, c, text] of list) {
      const f = t.fields[c - 1], key = keyOf(r);
      if (!f || key == null) continue;
      if (c - 1 === t.pk) { toast("A row's key does not change: add a row with the new key, and delete this one", "info"); return; }
      let v;
      try { v = valueFor(f, text); } catch (e) { toast(e.message, "bad"); return; }
      ops.push({ op: "set", table: t.name, key, field: f.name, value: v, r, c });
    }
    if (!ops.length) return;
    // shown at once
    const old = ops.map((o) => { const row = rowOf(o.r); const was = row[o.c - 1]; row[o.c - 1] = o.value; t.edited.set(`${o.r}:${o.c}`, true); return [row, o.c - 1, was]; });
    grid.draw();
    const ok = await write(ops.map(({ r: _r, c: _c, ...o }) => o), "The change");
    if (!ok) for (const [row, i, was] of old) row[i] = was;
    setTimeout(() => { for (const o of ops) t.edited.delete(`${o.r}:${o.c}`); grid.draw(); }, 1200);
    grid.draw();
  }
  async function addRow(prefill = {}) {
    const inputs = t.fields.map((f, i) => {
      const inp = h(f.type === "text" ? "textarea" : "input", { class: "input", rows: 2, value: prefill[f.name] ?? "", placeholder: f.type, "aria-label": f.name, style: { width: "100%" } });
      if (f.type === "text") inp.value = prefill[f.name] ?? "";
      return [f, inp, i];
    });
    const go = h("button", { class: "btn primary" }, "Add the row");
    const err = h("div", { class: "small", style: { color: "var(--crit-ink)" } });
    const m = core.modal({ title: `A row of ${t.name}`, wide: true,
      body: h("div", { class: "stack", style: { gap: "10px" } }, h("div", { class: "sx-form" }, inputs.map(([f, inp, i]) => [h("label", {}, f.name, i === t.pk ? h("b", {}, " (key)") : ""), inp])), err),
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go] });
    go.onclick = async () => {
      const doc = {};
      try { for (const [f, inp] of inputs) { const v = valueFor(f, inp.value); if (v != null) doc[f.name] = v; } } catch (e) { err.textContent = e.message; return; }
      if (t.pk >= 0 && doc[t.fields[t.pk].name] == null) { err.textContent = `${t.fields[t.pk].name}, the key, is needed`; return; }
      go.disabled = true;
      try { await sapi("/v1/write", { body: { ops: [{ op: "insert", table: t.name, doc }] }, timeout: 60000 }); m.close(); toast("Row added", "ok"); reload(); }
      catch (e) { err.textContent = e.message; go.disabled = false; }
    };
  }
  async function deleteRows() {
    const a = sel();
    const r1 = Math.min(a.r1, t.count || 0);
    const keys = [];
    for (let r = a.r0; r <= r1; r++) { const k = keyOf(r); if (k != null) keys.push(k); }
    if (!keys.length) { toast(t.pk < 0 ? "This table has no key, so its rows are deleted with SQL" : "Select the rows to delete (click their numbers)", "info"); return; }
    if (!(await core.confirm({ title: `Delete ${keys.length} ${keys.length === 1 ? "row" : "rows"} of ${t.name}?`, body: `${keys.slice(0, 5).join(", ")}${keys.length > 5 ? "…" : ""}: deleted from the cluster's table, not only from this view.`, ok: "Delete", danger: true }))) return;
    for (let i = 0; i < keys.length; i += 500) if (!(await write(keys.slice(i, i + 500).map((key) => ({ op: "delete", table: t.name, key })), "Delete"))) break;
    toast(`Deleted ${keys.length} ${keys.length === 1 ? "row" : "rows"}`, "ok");
    reload();
  }
  async function addColumn() {
    const nameIn = h("input", { class: "input", placeholder: "discount", "aria-label": "Name" });
    const typeSel = h("select", { "aria-label": "Type" }, ["TEXT", "KEYWORD", "INT", "FLOAT", "BOOL", "TIMESTAMP"].map((x) => h("option", { value: x }, x)));
    const err = h("div", { class: "small", style: { color: "var(--crit-ink)" } });
    const go = h("button", { class: "btn primary" }, "Add the column");
    const m = core.modal({ title: `A column of ${t.name}`, body: h("div", { class: "stack", style: { gap: "10px" } }, h("div", { class: "sx-form" }, h("label", {}, "Name"), nameIn, h("label", {}, "Type"), typeSel), h("div", { class: "small muted" }, "ALTER TABLE ... ADD COLUMN: the rows there are get NULL."), err),
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go] });
    go.onclick = async () => {
      const n = nameIn.value.trim();
      if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(n)) { err.textContent = "A name is letters, digits and _"; return; }
      go.disabled = true;
      try { await sapi("/v1/sql", { body: { sql: `ALTER TABLE ${qid(t.name)} ADD COLUMN ${qid(n)} ${typeSel.value}` }, timeout: 60000 }); m.close(); toast(`Added ${n}`, "ok"); await loadSchema(); reload(); }
      catch (e) { err.textContent = e.message; go.disabled = false; }
    };
  }
  async function toBook() {
    try {
      toast("Copying the rows into a workbook…", "info", 2000);
      const j = await ctx.api("/v1/from-table", { body: { sql: sqlOf(), name: t.name }, timeout: 300000 });
      location.hash = ctx.href("b", j.id);
    } catch (e) { toast(e.message, "bad"); }
  }

  Object.assign(grid.hooks, {
    editStart: (r, c) => { const f = t.fields[c - 1], row = rowOf(r); if (!f || !row) return false; if (c - 1 === t.pk) { toast("A row's key does not change", "info"); return false; } const v = row[c - 1]; return v == null ? "" : typeof v === "object" ? JSON.stringify(v) : String(v); },
    edit: (r, c, text) => setCells([[r, c, text]]),
    colWidth: ([a, b], w) => { for (let c = a; c <= b; c++) src.colAxis.sizes.set(c, w); src.colAxis.set(src.colAxis.sizes); grid.sizeSpacer(); grid.draw(); },
    rowHeight: () => {},
    autofit: (c) => { const g = grid.canvas.getContext("2d"); g.font = "13px system-ui"; let w = 60; for (const rows of t.pages.values()) for (const row of rows) w = Math.max(w, g.measureText(String(row[c - 1] ?? "").slice(0, 80)).width + 16); src.colAxis.sizes.set(c, Math.min(600, Math.ceil(w))); src.colAxis.set(src.colAxis.sizes); grid.sizeSpacer(); grid.draw(); },
    headerClick: (ht) => {
      const x = grid.sx(ht.col) + src.colAxis.size(ht.col);
      if (ht.x < x - 22) return false;
      sortOn(ht.col - 1);
      return true;
    },
    dblHeader: (c) => sortOn(c - 1),
    select: () => showStats(),
    key: (e) => {
      const k = e.key.toLowerCase(), m = mod(e);
      if (k === "delete" || k === "backspace") { e.preventDefault(); const a = sel(); const list = []; for (let r = a.r0; r <= Math.min(a.r1, t.count || 0); r++) for (let c = a.c0; c <= Math.min(a.c1, t.fields.length); c++) if (c - 1 !== t.pk) list.push([r, c, ""]); if (list.length > 2000) { toast("Clear fewer cells at a time (2,000 at most)", "info"); return true; } setCells(list); return true; }
      if (m && k === "f") { e.preventDefault(); searchIn.focus(); return true; }
      return false;
    },
    context: (e, ht) => {
      const a = sel(), c = ht.col || a.c0, f = t.fields[c - 1], v = rowOf(ht.row || a.r0)?.[c - 1];
      const anchor = { getBoundingClientRect: () => ({ left: e.clientX, right: e.clientX, top: e.clientY, bottom: e.clientY }) };
      core.menu(anchor, [
        f ? { label: `Sort by ${f.name}, ascending`, icon: "up", onclick: () => { t.sort = { col: c - 1, desc: false }; reload(); } } : null,
        f ? { label: `Sort by ${f.name}, descending`, icon: "down", onclick: () => { t.sort = { col: c - 1, desc: true }; reload(); } } : null,
        f && v != null && typeof v !== "object" ? { label: `Only rows where ${f.name} = ${String(v).slice(0, 24)}`, icon: "filter", onclick: () => { const cond = `${qid(f.name)} = ${typeof v === "string" ? lit(v) : v}`; whereIn.value = whereIn.value.trim() ? `${whereIn.value.trim()} AND ${cond}` : cond; apply(); } } : null,
        "-",
        { label: "Copy", icon: "copy", onclick: () => core.copy(tsv(a), "Copied") },
        ht.row ? { label: "Copy the row as JSON", icon: "copy", onclick: () => { const row = rowOf(ht.row); if (row) core.copy(JSON.stringify(Object.fromEntries(t.fields.map((x, i) => [x.name, row[i]])), null, 2), "Copied"); } } : null,
        { label: "Add a row", icon: "plus", onclick: () => addRow() },
        ht.row ? { label: "Add a row like this one", icon: "plus", onclick: () => { const row = rowOf(ht.row); addRow(Object.fromEntries(t.fields.map((x, i) => [x.name, i === t.pk || row?.[i] == null ? "" : typeof row[i] === "object" ? JSON.stringify(row[i]) : String(row[i])]))); } } : null,
        { label: "Delete the selected rows", icon: "trash", danger: true, onclick: () => deleteRows() },
      ].filter(Boolean));
    },
  });
  function sortOn(i) {
    if (!t.sort || t.sort.col !== i) t.sort = { col: i, desc: false };
    else if (!t.sort.desc) t.sort = { col: i, desc: true };
    else t.sort = null;
    reload();
  }
  function tsv(a) {
    const out = [];
    for (let r = a.r0; r <= Math.min(a.r1, t.count || 0, a.r0 + 20000); r++) {
      const row = rowOf(r); if (!row) continue;
      const l = []; for (let c = a.c0; c <= Math.min(a.c1, t.fields.length); c++) { const v = row[c - 1]; l.push(v == null ? "" : (typeof v === "object" ? JSON.stringify(v) : String(v)).replace(/[\t\n]/g, " ")); }
      out.push(l.join("\t"));
    }
    return out.join("\n");
  }
  const onCopy = (e) => { if (grid.editing || document.activeElement !== grid.scroll) return; e.clipboardData.setData("text/plain", tsv(sel())); e.preventDefault(); toast("Copied", "info", 1200); };
  const onPaste = (e) => {
    if (grid.editing || document.activeElement !== grid.scroll) return;
    e.preventDefault();
    const text = e.clipboardData?.getData("text/plain") || "";
    const lines = text.replace(/\r\n/g, "\n").replace(/\n$/, "").split("\n").map((l) => l.split("\t"));
    const a = sel(), list = [];
    lines.forEach((l, i) => l.forEach((v, j) => { const r = a.r0 + i, c = a.c0 + j; if (r <= (t.count || 0) && c <= t.fields.length && c - 1 !== t.pk) list.push([r, c, v]); }));
    if (list.length > 5000) { toast("Paste fewer cells at a time (5,000 at most)", "info"); return; }
    setCells(list);
  };
  document.addEventListener("copy", onCopy);
  document.addEventListener("paste", onPaste);
  function showStats() {
    const a = sel();
    if (a.r0 === a.r1 && a.c0 === a.c1) { stats.replaceChildren(); return; }
    const nums = [];
    let n = 0;
    for (let r = a.r0; r <= Math.min(a.r1, t.count || 0, a.r0 + 100000); r++) { const row = rowOf(r); if (!row) continue; for (let c = a.c0; c <= Math.min(a.c1, t.fields.length); c++) { const v = row[c - 1]; if (v == null || v === "") continue; n++; if (typeof v === "number") nums.push(v); } }
    const sum = nums.reduce((x, y) => x + y, 0);
    stats.replaceChildren(...(nums.length ? [["Sum", fmtNum(sum)], ["Average", fmtNum(sum / nums.length)]] : []).concat([["Count", n.toLocaleString()]]).map(([k, v]) => h("span", {}, k, ": ", h("b", {}, v))));
  }

  async function loadSchema() {
    const j = await sapi(`/v1/schema?table=${encodeURIComponent(table)}`, { timeout: 15000 });
    const short = j.short || {};
    t.fields = (short.fields || []).map((f) => ({ name: f.name, type: String(f.type || "text").toLowerCase() }));
    t.pk = j.primary_key ? t.fields.findIndex((f) => f.name === j.primary_key) : -1;
    const widths = new Map();
    t.fields.forEach((f, i) => widths.set(i + 1, f.type === "text" ? 240 : f.type === "keyword" ? 150 : f.type === "bool" ? 90 : f.type.startsWith("vector") ? 200 : 120));
    src.colAxis.sizes = widths; src.colAxis.set(widths);
    fieldSel.replaceChildren(...t.fields.filter(isText).map((f) => h("option", { value: f.name }, f.name)));
    // The WHERE box's example, of this table's own fields.
    const num = t.fields.find((f) => (f.type === "int" || f.type === "float") && t.fields.indexOf(f) !== t.pk), kw = t.fields.find((f) => f.type === "keyword" && t.fields.indexOf(f) !== t.pk);
    const ex = [num && `${qid(num.name)} > 10`, kw && `${qid(kw.name)} = 'x'`].filter(Boolean).join(" AND ");
    whereIn.placeholder = ex ? `WHERE: ${ex}` : "WHERE: a SQL condition on the columns";
    fieldSel.style.display = searchIn.style.display = t.fields.some(isText) ? "" : "none";
    if (!t.searchField) t.searchField = fieldSel.value;
  }
  (async () => {
    try {
      await loadSchema();
      if (gone) return;
      await reload();
      grid.select(1, 1);
      grid.scroll.focus({ preventScroll: true });
      document.title = `${table} · Sheets`;
    } catch (e) {
      if (gone) return;
      root.replaceChildren(core.failure ? core.failure(e, `The table ${table}`) : h("p", {}, e.message), h("p", {}, h("a", { href: ctx.href() }, "Back to Sheets")));
    }
  })();
  // live: the count and the rows shown, read again
  stops.push(core.every(5000, async () => {
    if (gone || !t.fields.length || grid.editing) return;
    await readCount();
    const f = grid.frozen(), v = grid.visible(f);
    const p0 = Math.floor((v.r0 - 1) / TPAGE), p1 = Math.floor((v.r1 - 1) / TPAGE);
    for (let p = p0; p <= p1; p++) if (!t.pending.has(p)) await readPage(p, true);
    showStats();
  }, false));
  return () => { gone = true; document.removeEventListener("copy", onCopy); document.removeEventListener("paste", onPaste); for (const f of stops) { try { f(); } catch (e) { console.warn(e); } } };
}

// ---------------------------------------------------------------- the home page

function homePage(root, ctx) {
  const core = ctx.core, { h, icon, toast } = core;
  let gone = false;
  const stops = [];
  document.title = "Sheets";
  const searchIn = h("input", { class: "input", type: "search", placeholder: "Search every workbook: a value, a name, a word in a cell", "aria-label": "Search every workbook", style: { width: "100%" } });
  const hits = h("div");
  const books = h("div");
  const tables = h("div");
  const info = h("span", { class: "small muted" });
  const newBtn = h("button", { class: "btn primary", onclick: () => newBook() }, icon("plus"), "New workbook");
  // Whose workbooks the list shows: the key's own, others', or all it may open.
  let whose = "all";
  try { whose = localStorage.getItem("sx-whose") || "all"; } catch { /* no storage */ }
  const whoseBar = h("div", { class: "sx-whose", role: "group", "aria-label": "Whose workbooks" });
  const drawWhose = () => whoseBar.replaceChildren(...[["mine", "Mine"], ["shared", "Shared"], ["all", "All"]].map(([k, label]) =>
    h("button", { type: "button", "aria-pressed": String(whose === k), onclick: () => { whose = k; try { localStorage.setItem("sx-whose", k); } catch { /* no storage */ } drawWhose(); loadBooks(); } }, label)));
  drawWhose();
  const drop = core.dropzone({ label: "Import a file", sub: "Excel (.xlsx), CSV or TSV, or a SQLite database (.sqlite, .db): as a workbook, or as tables of the cluster", accept: ".xlsx,.xlsm,.csv,.tsv,.txt,.sqlite,.sqlite3,.db", multiple: false, onFiles: (f) => importFile(f[0]) });
  root.append(
    h("div", { class: "page-head" }, h("div", {},
      h("h1", {}, "Sheets"),
      h("p", {}, "Spreadsheets kept in ChloeDB: formulas (over 300 functions), formats, charts, pivots and SQL queries of the cluster's tables that fill cells. Every cell is a row of a ChloeDB table, so every workbook can be searched; the cluster's own tables open here as sheets you can sort, filter and edit."),
      info)),
    h("div", { class: "sx-home-grid" },
      h("div", { class: "stack", style: { gap: "16px" } },
        core.card({ title: "Search", body: h("div", { class: "stack", style: { gap: "8px" } }, searchIn, hits) }),
        core.card({ title: "Workbooks", tools: [whoseBar, newBtn], body: books, flush: true })),
      h("div", { class: "stack", style: { gap: "16px" } },
        core.card({ title: "Import", body: drop }),
        core.card({ title: "Tables of the cluster", sub: "open one live, or copy it into a workbook", body: h("div", { class: "sx-hscroll" }, tables), flush: true }))));

  async function newBook() {
    const name = await ask(core, { title: "A new workbook", label: "Its name", value: "Untitled" });
    if (!name) return;
    try { const b = await ctx.api("/v1/books", { body: { name } }); location.hash = ctx.href("b", b.id); } catch (e) { toast(e.message, "bad"); }
  }

  async function loadBooks() {
    try {
      const j = await ctx.api(`/v1/books?whose=${whose}`, { timeout: 15000 });
      if (gone) return;
      const list = j.books || [];
      const me = j.me;
      const t = core.table([
        { key: "name", label: "Name", render: (b) => h("a", { href: ctx.href("b", b.id), class: "row nw", style: { gap: "10px", alignItems: "center" } }, h("span", { class: "sx-book-ic", "aria-hidden": "true" }, icon("table")), h("span", {}, h("b", {}, b.name), h("div", { class: "small muted" }, (b.sheet_names || "").split("\n").join(" · ").slice(0, 80)))) },
        { key: "source", label: "From", render: (b) => h("span", { class: "small dim" }, sourceLabel(b.source)) },
        { key: "owner", label: "Owner", render: (b) => h("span", { class: "small dim", title: b.shared ? "Shared: keys limited to tables open it too" : "" }, b.owner ? (b.owner === me ? "you" : b.owner) : "–", b.shared ? " · shared" : "") },
        { key: "cells", label: "Cells", num: true, render: (b) => (b.cells ?? 0).toLocaleString() },
        { key: "updated_ms", label: "Changed", num: true, render: (b) => h("span", { class: "small", title: new Date(b.updated_ms).toLocaleString() }, core.fmt.ago(b.updated_ms)) },
        { key: "", label: "", sortable: false, render: (b) => h("button", { class: "btn ghost sm icon", title: "More", "aria-label": `More about ${b.name}`, onclick: (e) => core.menu(e.currentTarget, [
          { label: "Open", icon: "right", onclick: () => { location.hash = ctx.href("b", b.id); } },
          { label: "Download as Excel", icon: "download", onclick: () => downloadBook(b) },
          { label: b.shared ? "Stop sharing" : "Share (keys limited to tables open it)", icon: "share", onclick: async () => { try { await ctx.api(`/v1/books/${encodeURIComponent(b.id)}/share`, { body: { shared: !b.shared } }); toast(b.shared ? `${b.name} is no longer shared` : `${b.name} is shared`, "ok"); loadBooks(); } catch (err) { toast(err.message, "bad"); } } },
          "-",
          { label: "Delete", icon: "trash", danger: true, onclick: async () => { if (!(await core.confirm({ title: `Delete ${b.name}?`, body: "Its sheets, charts and queries are deleted from ChloeDB. This cannot be undone.", ok: "Delete", danger: true }))) return; try { await ctx.api(`/v1/books/${encodeURIComponent(b.id)}`, { method: "DELETE" }); toast(`Deleted ${b.name}`, "ok"); loadBooks(); } catch (err) { toast(err.message, "bad"); } } },
        ]) }, icon("dots")) },
      ], { rows: list, sortKey: "updated_ms", desc: true, onRow: (b) => { location.hash = ctx.href("b", b.id); }, empty: core.empty("table", whose === "mine" ? "No workbooks of yours yet" : whose === "shared" ? "No one else's workbooks to open" : "No workbooks yet", "Make one, or import an Excel, CSV or SQLite file.", newBtn.cloneNode(true)) });
      t.classList.add("sx-books");
      t.querySelector(".empty button")?.addEventListener("click", newBook);
      books.replaceChildren(t);
    } catch (e) { if (!gone) books.replaceChildren(core.failure ? core.failure(e, "The workbooks") : h("p", {}, e.message)); }
  }
  async function downloadBook(b) {
    try {
      const r = await ctx.api(`/v1/books/${encodeURIComponent(b.id)}/export?format=xlsx`, { blob: true, timeout: 120000 });
      const a = h("a", { href: URL.createObjectURL(await r.blob()), download: `${b.name}.xlsx` });
      document.body.append(a); a.click(); a.remove();
      leftOut(r, toast);
    } catch (e) { toast(e.message, "bad"); }
  }

  async function loadTables() {
    try {
      const [j, hl] = await Promise.all([core.api("/v1/tables", { timeout: 15000 }), core.api("/v1/health", { timeout: 8000 }).catch(() => null)]);
      if (gone) return;
      const counts = hl?.tables || {};
      const list = tableList(j).map((x) => ({ name: x.table || x.name, key: x.primary_key, fields: x.short?.fields?.length ?? x.schema?.fields?.length ?? 0, rows: counts[x.table || x.name] })).filter((x) => !/^sheets_(books|cells)$/.test(x.name) && !x.name.startsWith("_"));
      tables.replaceChildren(core.table([
        { key: "name", label: "Table", render: (x) => h("a", { href: ctx.href("t", x.name), class: "row nw", style: { gap: "8px", alignItems: "center" } }, h("span", { class: "sx-book-ic t", "aria-hidden": "true" }, icon("db")), h("span", {}, h("b", {}, x.name), h("div", { class: "small muted" }, `${x.fields} columns${x.key ? ` · key ${x.key}` : ""}`))) },
        { key: "rows", label: "Rows", num: true, render: (x) => (x.rows == null ? "–" : x.rows.toLocaleString()) },
        { key: "", label: "", sortable: false, render: (x) => h("div", { class: "row nw", style: { gap: "4px", justifyContent: "flex-end" } },
          h("a", { class: "btn sm", href: ctx.href("t", x.name), title: "Its rows, read and written live" }, "Open"),
          h("button", { class: "btn sm", title: "Copy its rows into a new workbook", onclick: async (e) => { const b = e.currentTarget; b.disabled = true; try { const r = await ctx.api("/v1/from-table", { body: { table: x.name, name: x.name }, timeout: 300000 }); location.hash = ctx.href("b", r.id); } catch (err) { toast(err.message, "bad"); b.disabled = false; } } }, icon("copy"), "To a workbook")) },
      ], { rows: list, sortKey: "name", desc: false, onRow: (x) => { location.hash = ctx.href("t", x.name); }, empty: core.empty("db", "No tables yet", "Import a SQLite file into the cluster, or save a sheet as a table.") }));
      // Rows the health does not say, counted (the first 40 tables).
      const missing = list.filter((x) => x.rows == null).slice(0, 40);
      for (const x of missing) {
        core.api("/v1/query", { body: { sql: `SELECT COUNT(*) AS n FROM ${qid(x.name)}` }, timeout: 15000 }).then((r) => {
          if (gone) return;
          x.rows = Number(r.rows?.[0]?.[0] ?? r.rows?.[0]?.n);
          const cell = [...tables.querySelectorAll("tr")].find((tr) => tr.querySelector("b")?.textContent === x.name)?.children?.[1];
          if (cell && Number.isFinite(x.rows)) cell.textContent = x.rows.toLocaleString();
        }).catch(() => {});
      }
    } catch (e) { if (!gone) tables.replaceChildren(core.failure ? core.failure(e, "The cluster's tables") : h("p", {}, e.message)); }
  }

  // ------------------------------------------------------------ search
  let searchN = 0;
  const runSearch = core.debounce(async () => {
    const q = searchIn.value.trim(), n = ++searchN;
    if (!q) { hits.replaceChildren(); return; }
    try {
      const j = await ctx.api(`/v1/search?q=${encodeURIComponent(q)}&limit=60`, { timeout: 15000 });
      if (n !== searchN || gone) return;
      const rows = (r) => (r?.rows || []).map((x) => Object.fromEntries((r.columns || []).map((c, i) => [c, x[i]])));
      const bs = rows(j.books), cs = rows(j.cells);
      const mark = (text) => { const t = String(text ?? ""); const words = q.toLowerCase().split(/\s+/).filter(Boolean); const parts = []; let i = 0; const lower = t.toLowerCase(); while (i < t.length) { let best = -1, w = ""; for (const x of words) { const k = lower.indexOf(x, i); if (k >= 0 && (best < 0 || k < best)) { best = k; w = x; } } if (best < 0) { parts.push(t.slice(i)); break; } parts.push(t.slice(i, best), h("mark", {}, t.slice(best, best + w.length))); i = best + w.length; } return parts; };
      const names = new Map();
      for (const b of bs) names.set(b.id, b.name);
      hits.replaceChildren(!bs.length && !cs.length ? h("p", { class: "small muted", style: { margin: 0 } }, `Nothing holds “${q}”.`) : h("ul", { class: "sx-hits" },
        bs.map((b) => h("li", {}, h("a", { href: ctx.href("b", b.id) }, h("b", {}, mark(b.name)), h("div", { class: "small muted" }, mark(String(b.sheet_names || "").split("\n").join(" · ")))), h("span", { class: "small muted" }, "workbook"))),
        cs.map((c) => h("li", {}, h("a", { href: ctx.href("b", c.book, String(c.sheet_name), c.ref) }, h("span", { class: "mono small" }, `${c.sheet_name}!${c.ref}`), " ", mark(c.text), c.input && c.input !== c.text ? h("div", { class: "small muted mono" }, c.input) : null), h("span", { class: "small muted" }, names.get(c.book) || c.book)))));
    } catch (e) { if (n === searchN) hits.replaceChildren(h("p", { class: "small", style: { color: "var(--crit-ink)", margin: 0 } }, e.message)); }
  }, 250);
  searchIn.addEventListener("input", runSearch);

  // ------------------------------------------------------------ imports
  async function importFile(file) {
    if (!file) return;
    const name = file.name;
    const head = new Uint8Array(await file.slice(0, 16).arrayBuffer());
    const sqlite = String.fromCharCode(...head).startsWith("SQLite format 3");
    if (sqlite) return importSqlite(file);
    const fmt = /\.xlsx?m?$/i.test(name) ? "xlsx" : /\.tsv$/i.test(name) ? "tsv" : "csv";
    const about = fmt === "xlsx" ? null : await csvSeconds(file).catch(() => null);
    const busy = toastBusy(`Importing ${name}…`, about);
    try {
      const b = await ctx.api(`/v1/import?name=${encodeURIComponent(name)}&format=${fmt}`, { body: file, type: "application/octet-stream", timeout: 300000 });
      busy();
      const rep = b.import || {};
      toast(`${name}: ${rep.rows != null ? `${rep.rows.toLocaleString()} rows` : `${b.sheets?.length || 1} sheets`}`, "ok");
      location.hash = ctx.href("b", b.id);
    } catch (e) { busy(); toast(`${name}: ${e.message}`, "bad", 8000); }
  }
  async function importSqlite(file) {
    let list;
    try { list = (await ctx.api("/v1/sqlite/tables", { body: file, type: "application/octet-stream", timeout: 120000 })).tables; }
    catch (e) { toast(`${file.name}: ${e.message}`, "bad", 8000); return; }
    const boxes = list.map((t) => { const cb = h("input", { type: "checkbox", checked: t.kind !== "view", value: t.name }); return [t, cb]; });
    let where = "book";
    const seg = core.seg([["book", "A workbook (a sheet a table)"], ["cluster", "Tables of the cluster"]], where, (v) => { where = v; go.textContent = v === "book" ? "Open as a workbook" : "Copy into the cluster"; note.textContent = v === "book" ? "Each table becomes a sheet: its first row the column names. Up to 2 million cells." : "Each table becomes a ChloeDB table of the same name, with its columns' types, key and indexes, written with your key. Tables that exist already get the rows too."; });
    const note = h("p", { class: "small dim", style: { margin: 0 } }, "Each table becomes a sheet: its first row the column names. Up to 2 million cells.");
    const go = h("button", { class: "btn primary" }, "Open as a workbook");
    const out = h("div", { class: "small" });
    const m = core.modal({ title: `Import ${file.name}`, wide: true,
      body: h("div", { class: "stack", style: { gap: "12px" } },
        h("div", {}, h("b", { class: "small" }, "Its tables"),
          h("div", { class: "sx-values", style: { marginTop: "6px" } }, boxes.map(([t, cb]) => h("label", {}, cb, h("span", { class: "grow" }, h("b", {}, t.name), h("span", { class: "small muted" }, `  ${(t.columns || []).map((c) => c.name).join(", ").slice(0, 90)}`)), h("span", { class: "small muted" }, t.kind === "view" ? "a view: runs its query as it is read" : `${(t.rows ?? 0).toLocaleString()} rows`))))),
        h("div", {}, h("b", { class: "small" }, "As"), h("div", { style: { marginTop: "6px" } }, seg)), note, out),
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go] });
    go.onclick = async () => {
      const chosen = boxes.filter(([, cb]) => cb.checked).map(([t]) => t.name);
      if (!chosen.length) { out.textContent = "Choose a table"; return; }
      go.disabled = true; out.textContent = where === "book" ? "Reading the tables…" : "Writing the rows…";
      try {
        if (where === "book") {
          const b = await ctx.api(`/v1/import?name=${encodeURIComponent(file.name)}&format=sqlite&tables=${encodeURIComponent(chosen.join(","))}`, { body: file, type: "application/octet-stream", timeout: 300000 });
          m.close(); location.hash = ctx.href("b", b.id);
        } else {
          const j = await core.api(`/v1/import?format=sqlite&tables=${encodeURIComponent(chosen.join(","))}`, { body: file, type: "application/vnd.sqlite3", timeout: 600000 });
          out.replaceChildren(h("div", {}, `Wrote ${(j.written ?? 0).toLocaleString()} rows in ${Math.round(j.took_ms ?? 0).toLocaleString()} ms: `,
            ...(j.tables || []).flatMap((t, i) => [i ? ", " : "", h("a", { href: ctx.href("t", t.table || t.name), onclick: () => m.close() }, t.table || t.name)])));
          go.textContent = "Done"; go.onclick = () => m.close(); go.disabled = false;
          loadTables();
        }
      } catch (e) { out.textContent = e.message; out.style.color = "var(--crit-ink)"; go.disabled = false; }
    };
  }
  // A toast while a long request runs, with the seconds it has taken (an
  // import of a large file takes a while: every cell is a row written).
  function toastBusy(msg, about) {
    const t0 = Date.now(), took = h("span", { class: "muted" });
    const el = h("div", { class: "toast info", role: "status", style: { position: "fixed", right: "16px", bottom: "16px", zIndex: 95 } }, icon("clock"), h("div", { class: "grow" }, msg, " ", took));
    const tick = () => { const s = Math.round((Date.now() - t0) / 1000); took.textContent = s >= 2 ? `${s} s${about ? ` of about ${about} s` : ""}` : ""; };
    const timer = setInterval(tick, 1000);
    document.body.append(el);
    return () => { clearInterval(timer); el.remove(); };
  }
  // About how long an import of a CSV file takes: its cells, counted on its
  // first 64 KB, at the 25,000 rows a second a server writes.
  async function csvSeconds(file) {
    const head = await file.slice(0, 65536).text();
    const lines = head.split("\n").length - 1;
    if (lines < 2) return null;
    const first = head.split("\n")[0], d = [",", ";", "\t", "|"].reduce((a, x) => first.split(x).length > first.split(a).length ? x : a, ",");
    const cells = (file.size / Math.max(1, head.length)) * lines * first.split(d).length;
    const secs = Math.round(cells / 25000);
    return secs >= 5 ? secs : null;
  }

  ctx.api("/v1/info", { timeout: 8000 }).then((j) => {
    if (gone) return;
    info.textContent = `chloedb-sheets ${j.version} · ${j.engine || "IronCalc"} · ${j.open} open · workbooks in ${j.tables?.books} and ${j.tables?.cells}${j.strata?.error ? ` · ChloeDB: ${j.strata.error}` : ""}`;
  }).catch(() => {});
  loadBooks(); loadTables();
  stops.push(core.every(15000, () => { loadBooks(); loadTables(); }, false));
  setTimeout(() => searchIn.focus(), 0);
  return () => { gone = true; for (const f of stops) f(); };
}

function sourceLabel(s) {
  const t = String(s || "");
  if (t === "new" || !t) return "made here";
  const [kind, ...rest] = t.split(":");
  const what = rest.join(":");
  return { xlsx: `Excel, ${what}`, csv: `CSV, ${what}`, tsv: `TSV, ${what}`, sqlite: `SQLite, ${what}`, db: `SQLite, ${what}`, table: `table ${what}`, sql: "a query" }[kind] || t;
}
