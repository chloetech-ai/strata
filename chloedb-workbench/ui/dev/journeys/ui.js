// Shared pieces of the journeys mock (JOURNEYS.md): the page frame, the
// stepper, objective cards, question cards, and their styles. Built on
// ui/levels/common.js (h, useStyle, avatar, ctxChip, pill) so the mock keeps
// the app's theme, density and the ◆ ChloeCtx chips in --ctx purple.
import { h, useStyle, avatar, ctxChip, pill } from "../../levels/common.js";
export { h, avatar, ctxChip, pill };

const SVG = "http://www.w3.org/2000/svg";
const PATHS = {
  search: "M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14zM20 20l-4-4",
  bell: "M6 8a6 6 0 1 1 12 0c0 7 3 9 3 9H3s3-2 3-9M10.3 21a1.9 1.9 0 0 0 3.4 0",
  more: "M5 12h.01M12 12h.01M19 12h.01",
  chev: "M9 6l6 6-6 6",
  down: "M6 9l6 6 6-6",
  term: "M4 5h16v14H4zM7.5 9.5l3 2.5-3 2.5M12.5 15h4",
  check: "M5 12.5l4.5 4.5L19 7.5",
  plus: "M12 5v14M5 12h14",
  folder: "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z",
  spark: "M12 3v4M12 17v4M3 12h4M17 12h4M6 6l2.5 2.5M15.5 15.5L18 18M6 18l2.5-2.5M15.5 8.5L18 6",
  rewind: "M3 12a9 9 0 1 0 2.6-6.4M3 4v5h5M12 8v4.5l3 1.8",
  branch: "M6 3v12M18 9a3 3 0 1 0 0-.01M6 21a3 3 0 1 0 0-.01M18 12c0 4-6 3-11.5 7",
  ship: "M4 15l8 5 8-5M4 10l8 5 8-5-8-5z",
  why: "M9.3 9.2a2.8 2.8 0 0 1 5.4 1c0 1.9-2.7 2.4-2.7 4.1M12 17.6h.01M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18z",
  new: "M12 3l2.5 5.5L20 9l-4 4 1 6-5-3-5 3 1-6-4-4 5.5-.5z",
  map: "M3 3h18v18H3zM11 3v18M11 12h10M16 12v9",
  stop: "M7 7h10v10H7z",
  warn: "M12 3.5l9.5 16.5h-19zM12 10v4.5M12 17.4h.01",
  pause: "M8 5v14M16 5v14",
  eye: "M2 12s3.6-7 10-7 10 7 10 7-3.6 7-10 7S2 12 2 12zM12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6z",
  x: "M6 6l12 12M18 6L6 18",
  back: "M15 6l-6 6 6 6",
  home: "M4 11l8-7 8 7M6 9.5V20h12V9.5",
  grid: "M4 4h7v7H4zM13 4h7v7h-7zM4 13h7v7H4zM13 13h7v7h-7z",
  kbd: "M3 7h18v10H3zM7 11h.01M11 11h.01M15 11h.01M8 14h8",
};
export function icon(name, cls = "ji") {
  const s = document.createElementNS(SVG, "svg");
  s.setAttribute("viewBox", "0 0 24 24");
  s.setAttribute("class", cls);
  s.setAttribute("aria-hidden", "true");
  const p = document.createElementNS(SVG, "path");
  p.setAttribute("d", PATHS[name] || PATHS.chev);
  s.append(p);
  return s;
}

export function mark() {
  const s = document.createElementNS(SVG, "svg");
  s.setAttribute("viewBox", "0 0 24 24");
  s.setAttribute("class", "mark");
  s.innerHTML = `<rect x="2" y="2" width="9" height="9" rx="2.5" fill="var(--accent)"/><rect x="13" y="2" width="9" height="9" rx="2.5" fill="var(--line-2)"/><rect x="2" y="13" width="9" height="9" rx="2.5" fill="var(--line-2)"/><rect x="13" y="13" width="9" height="9" rx="2.5" fill="var(--warn)"/>`;
  return s;
}

/** A page: header (title, a sentence, a stepper or an aside), then the body. */
export function page({ title, sub, aside, steps, wide, cls, mbar }, ...body) {
  return h("div", { class: `jp${wide ? " wide" : ""}${cls ? " " + cls : ""}` },
    steps ? stepper(steps) : null,
    steps && !steps.plain ? mstep(steps) : null,
    h("header", { class: "jp-head" },
      h("div", { class: "jp-ht" }, h("h1", {}, title), sub ? h("p", { class: "jp-sub" }, sub) : null),
      aside || null),
    ...body,
    mbar ? h("div", { class: "jp-mbar" }, btn(mbar.back.label, { href: mbar.back.href, iconName: "back" }), btn(mbar.next.label, { primary: true, href: mbar.next.href })) : null);
}

/** On a phone the stepper is one line, "Step 2 of 4 · What to build", and a bar of segments (JOURNEYS.md, "Phone"). */
function mstep({ list }) {
  const at = Math.max(0, list.findIndex((s) => s.state === "now"));
  return h("div", { class: "jp-mstep", "aria-label": "Steps" },
    h("span", {}, h("b", {}, `Step ${at + 1} of ${list.length}`), ` · ${list[at].label}`),
    h("span", { class: "jp-mseg" }, list.map((s) => h("i", { class: s.state }))));
}

/** The stepper: [{label, href, state: done|now|next}], then an optional link out. */
export function stepper({ list, after, plain }) {
  return h("nav", { class: `jp-steps${plain ? " plain" : ""}`, "aria-label": plain ? "Project" : "Steps" },
    list.map((s, i) => [
      i && !plain ? h("span", { class: "jp-sep" }, icon("chev", "ji sm")) : null,
      h("a", { class: `jp-step ${s.state}`, href: s.href || "#", "aria-current": s.state === "now" ? "step" : null },
        plain ? (s.icon ? icon(s.icon, "ji sm") : null) : h("span", { class: "jp-n" }, s.state === "done" ? icon("check", "ji sm") : String(i + 1)), s.label),
    ]),
    after ? h("a", { class: "jp-after", href: after.href }, after.label, icon("chev", "ji sm")) : null);
}

export function btn(label, { primary, ghost, sm, href, onclick, kbd, iconName, cls } = {}) {
  const kids = [iconName ? icon(iconName, "ji sm") : null, label, kbd ? h("kbd", {}, kbd) : null];
  const c = `btn${primary ? " primary" : ""}${ghost ? " ghost" : ""}${sm ? " sm" : ""}${cls ? " " + cls : ""}`;
  return href ? h("a", { class: c, href, onclick }, kids) : h("button", { class: c, type: "button", onclick }, kids);
}

/** A question as JOURNEYS.md wants it: everything that will run, and Claude Code's own options. */
export function questionCard(q, { compact = false, onAnswer, agentLink = true } = {}) {
  const a = q.agent;
  const head = h("div", { class: "jq-head" },
    a ? avatar(a, 22) : h("span", { class: "jq-alert" }, icon("warn", "ji sm")),
    h("b", {}, a ? a.name : `Alert · ${q.severity}`),
    h("span", { class: "jq-wait" }, q.kind === "alert" ? "asks you" : "waiting for you"),
    h("span", { class: "jq-since", "data-since": q.since }, since(q.since)),
    h("span", { class: "sep" }),
    a && agentLink ? h("a", { class: "lv-link", href: `#/a/${a.id}` }, "Open agent") : null);
  const body = q.kind === "alert"
    ? [h("div", { class: "jq-title" }, q.title), h("p", { class: "jq-explain" }, q.detail)]
    : [
      h("div", { class: "jq-title" }, q.title, " ", h("span", { class: "muted" }, `${q.tool}, ${q.command.split("\n").length} lines, shown in full`)),
      h("pre", { class: `jq-cmd${compact ? " compact" : ""}` }, q.command),
      q.explain ? h("p", { class: "jq-explain" }, h("span", { class: "lv-ctx" }, "what it does"), " ", q.explain) : null,
    ];
  return h("div", { class: "jq" }, head, body,
    h("div", { class: "jq-opts" }, q.options.map((o, i) =>
      h("button", { class: `btn sm${i === 0 ? " primary" : ""}`, type: "button", onclick: () => onAnswer && onAnswer(o) }, h("kbd", {}, String(o.n)), o.label))),
    q.kind === "permission" ? h("div", { class: "jq-foot" }, "The numbers are Claude Code's own: Workbench types the one you pick, and only while this prompt is still open.") : null);
}

export function since(t) {
  const m = Math.max(1, Math.round((Date.now() - Date.parse(t)) / 60000));
  return m < 60 ? `${m} min` : `${Math.round(m / 60)} h`;
}
export function clock(t) {
  return new Date(typeof t === "number" ? t : Date.parse(t)).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

/** A host for a reused level module: a box of a fixed height, position relative. */
export function moduleHost(height, cls = "") {
  return h("div", { class: `jm-host ${cls}`, style: { height: typeof height === "number" ? height + "px" : height } });
}

useStyle("journeys", `
.ji{width:16px;height:16px;flex:none;stroke:currentColor;fill:none;stroke-width:1.9;stroke-linecap:round;stroke-linejoin:round}
.ji.sm{width:13px;height:13px}.ji.lg{width:22px;height:22px}
a.btn,a.lv-link{text-decoration:none}
.muted{color:var(--muted)}
#app{position:absolute;inset:0;display:flex;flex-direction:column}
.jt{height:52px;flex:none;display:flex;align-items:center;gap:12px;padding:0 16px;background:var(--surface);border-bottom:1px solid var(--line);position:relative;z-index:20}
.jt .brand{display:flex;align-items:center;gap:9px;font-weight:650;font-size:15px;color:var(--text);text-decoration:none;letter-spacing:-.01em}
.jt .mark{width:22px;height:22px}
.jt .crumbs{display:flex;align-items:center;gap:4px;font-size:13.5px;color:var(--muted);min-width:0}
.jt .crumbs a{color:var(--text-2);text-decoration:none;padding:3px 6px;border-radius:6px;white-space:nowrap}
.jt .crumbs a:hover{background:var(--surface-3);color:var(--text)}
.jt .crumbs a:last-child{color:var(--text);font-weight:600}
.jt .sep{flex:1}
.jt-search{display:inline-flex;align-items:center;gap:8px;height:32px;padding:0 8px 0 10px;min-width:240px;border:1px solid var(--line-2);border-radius:8px;background:var(--surface-2);color:var(--muted);cursor:pointer;font-size:13px}
.jt-search kbd{margin-left:auto}
.jt-needs{display:inline-flex;align-items:center;gap:7px;height:32px;padding:0 11px;border-radius:8px;border:1px solid var(--warn-line);background:var(--warn-soft);color:var(--warn-ink);font-weight:600;cursor:pointer;font-size:13px}
.jt-needs.none{border-color:var(--line-2);background:var(--surface);color:var(--muted);font-weight:500}
.jt-needs b{display:inline-grid;place-items:center;min-width:20px;height:20px;border-radius:99px;background:var(--warn);color:#241a00;font-size:11.5px;padding:0 5px}
.jt-agents{display:inline-flex;align-items:center;gap:8px;height:32px;padding:0 8px 0 6px;border:1px solid var(--line-2);border-radius:8px;background:var(--surface);cursor:pointer;font-size:13px;color:var(--text-2)}
.jt-agents .stack{display:inline-flex}.jt-agents .stack .lv-av{margin-left:-6px;box-shadow:0 0 0 2px var(--surface)}.jt-agents .stack .lv-av:first-child{margin-left:0}
.jt-agents .ndot{width:8px;height:8px;border-radius:50%;background:var(--warn);margin-left:-4px}
.jp-scroll{flex:1;overflow:auto;position:relative}
.jp{max-width:1200px;margin:0 auto;padding:22px 32px 48px}
.jp.wide{max-width:none}
.jp-head{display:flex;align-items:flex-end;gap:16px;margin:4px 0 18px}
.jp-ht{flex:1;min-width:0}
.jp-head h1{margin:0;font-size:24px;font-weight:680;letter-spacing:-.015em}
.jp-sub{margin:4px 0 0;color:var(--text-2);font-size:14px;max-width:780px}
.jp-steps{display:flex;align-items:center;gap:4px;margin:0 0 10px;font-size:13px}
.jp-step{display:inline-flex;align-items:center;gap:7px;padding:5px 11px 5px 6px;border-radius:99px;color:var(--muted);text-decoration:none;font-weight:600;border:1px solid transparent}
.jp-step .jp-n{display:inline-grid;place-items:center;width:20px;height:20px;border-radius:50%;background:var(--surface-3);color:var(--text-2);font-size:11.5px}
.jp-step.done{color:var(--text-2)}.jp-step.done .jp-n{background:var(--good-soft);color:var(--good-ink)}
.jp-step.now{color:var(--accent);background:var(--accent-soft);border-color:color-mix(in srgb,var(--accent) 30%,transparent)}.jp-step.now .jp-n{background:var(--accent);color:var(--accent-ink)}
.jp-step:hover{color:var(--text)}
.jp-sep{color:var(--line-2);display:inline-flex}
.jp-steps.plain{gap:2px}.jp-steps.plain .jp-step{padding:5px 12px}
.jp-after{margin-left:auto;display:inline-flex;align-items:center;gap:5px;color:var(--accent);font-weight:600;text-decoration:none;padding:5px 8px;border-radius:8px}
.jp-after:hover{background:var(--accent-soft)}
.jc{background:var(--surface);border:1px solid var(--line);border-radius:12px;padding:16px 18px;box-shadow:var(--shadow-1);min-width:0}
.jc h2{margin:0 0 10px;font-size:11.5px;font-weight:700;color:var(--muted);text-transform:uppercase;letter-spacing:.06em;display:flex;align-items:center;gap:8px}
.jc h2 .n{color:var(--text-2);font-weight:600}
.jc h3{margin:0;font-size:15px;font-weight:650}
.jrow{display:flex;align-items:center;gap:10px}
.jgrid2{display:grid;grid-template-columns:minmax(0,1fr) 360px;gap:16px;align-items:start}
.jgrid3{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:14px}
.jstack{display:flex;flex-direction:column;gap:14px}
.jm-host{position:relative;border:1px solid var(--line);border-radius:12px;overflow:hidden;background:var(--bg)}
.jm-host.flat{border:0;border-radius:0}
.jm-host .tt-bar{display:none !important}
.jbig{display:flex;align-items:center;gap:10px;border:1.5px solid var(--line-2);border-radius:12px;background:var(--surface);padding:6px 6px 6px 16px;box-shadow:var(--shadow-1)}
.jbig:focus-within{border-color:var(--accent);box-shadow:0 0 0 4px var(--focus)}
.jbig input,.jbig textarea{flex:1;border:0;outline:0;background:transparent;font-size:16px;padding:9px 0;color:var(--text);resize:none}
.jbig .btn{height:38px;padding:0 16px;font-size:14px}
.jtry{display:flex;flex-wrap:wrap;gap:6px;margin:10px 2px 0;font-size:12.5px;color:var(--muted);align-items:center}
.jtry a{color:var(--text-2);background:var(--surface-3);border-radius:99px;padding:3px 10px;text-decoration:none}
.jtry a:hover{color:var(--text)}
.jq{border:1px solid var(--warn-line);background:color-mix(in srgb,var(--warn-soft) 55%,var(--surface));border-radius:12px;padding:12px 14px;display:flex;flex-direction:column;gap:9px}
.jq-head{display:flex;align-items:center;gap:8px;font-size:13px}
.jq-wait{color:var(--warn-ink);font-weight:600}
.jq-since{color:var(--muted)}
.jq-alert{display:inline-grid;place-items:center;width:22px;height:22px;border-radius:7px;background:var(--crit-soft);color:var(--crit-ink)}
.jq-head .sep{flex:1}
.jq-title{font-weight:650;font-size:14px}
.jq-cmd{margin:0;font:12px/1.5 var(--mono);background:var(--term-bg);color:var(--term-fg);border:1px solid var(--term-line);border-radius:8px;padding:9px 11px;white-space:pre-wrap;word-break:break-all;max-height:190px;overflow:auto}
.jq-cmd.compact{max-height:96px}
.jq-explain{margin:0;color:var(--text-2);font-size:13px}
.jq-opts{display:flex;flex-wrap:wrap;gap:6px}
.jq-opts .btn kbd{margin-right:1px}
.jq-foot{font-size:11.5px;color:var(--muted)}
.jchip{display:inline-flex;align-items:center;gap:5px;font-size:12px;color:var(--text-2);background:var(--surface-3);border-radius:99px;padding:2px 9px;white-space:nowrap}
.jchip.ok{background:var(--good-soft);color:var(--good-ink)}.jchip.warn{background:var(--warn-soft);color:var(--warn-ink)}.jchip.crit{background:var(--crit-soft);color:var(--crit-ink)}
.jchip.ctx{background:color-mix(in srgb,var(--ctx) 13%,transparent);color:var(--ctx)}
.jdrawer{position:absolute;top:52px;right:0;bottom:0;width:520px;background:var(--surface);border-left:1px solid var(--line);box-shadow:var(--shadow-3);z-index:30;display:flex;flex-direction:column}
.jdrawer-h{display:flex;align-items:center;gap:10px;padding:14px 18px;border-bottom:1px solid var(--line)}
.jdrawer-h h2{margin:0;font-size:16px}
.jdrawer-b{flex:1;overflow:auto;padding:14px 18px;display:flex;flex-direction:column;gap:12px}
.jpop{position:absolute;top:48px;right:56px;width:470px;background:var(--surface);border:1px solid var(--line-2);border-radius:12px;box-shadow:var(--shadow-3);z-index:31;padding:8px;max-height:calc(100vh - 70px);overflow:auto}
.jpop-g{font-size:11px;font-weight:700;color:var(--muted);text-transform:uppercase;letter-spacing:.06em;padding:10px 10px 4px;display:flex;gap:6px}
.jpop-r{display:grid;grid-template-columns:24px minmax(0,1fr) auto auto;gap:4px 10px;align-items:center;padding:8px 10px;border-radius:9px;color:var(--text);text-decoration:none}
.jpop-r:hover,.jpop-r.cur{background:var(--surface-2)}
.jpop-r .nm{font-weight:650;display:flex;gap:8px;align-items:center}
.jpop-r .proj{color:var(--muted);font-weight:500;font-size:12px}
.jpop-r .line{grid-column:2/4;color:var(--text-2);font-size:12.5px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.jpop-r kbd{grid-row:1}
.jpop-r .term{grid-row:1/3;grid-column:4;color:var(--muted)}
.jpop-f{display:flex;gap:12px;border-top:1px solid var(--line);margin-top:6px;padding:9px 10px 3px;font-size:12px;color:var(--muted)}
.jstate{font-size:12px;font-weight:600}
.jstate.warn{color:var(--warn-ink)}.jstate.good{color:var(--good-ink)}.jstate.acc{color:var(--accent)}.jstate.mut{color:var(--muted)}
.jmore{position:absolute;top:48px;right:12px;width:280px;background:var(--surface);border:1px solid var(--line-2);border-radius:12px;box-shadow:var(--shadow-3);z-index:31;padding:6px}
.jmore a{display:flex;flex-direction:column;padding:8px 10px;border-radius:8px;color:var(--text);text-decoration:none;font-weight:600;font-size:13.5px}
.jmore a span{font-weight:400;color:var(--muted);font-size:12px}
.jmore a:hover{background:var(--surface-2)}
.jmore hr{border:0;border-top:1px solid var(--line);margin:4px 6px}
.jscrim{position:absolute;inset:52px 0 0 0;background:color-mix(in srgb,var(--bg) 40%,transparent);z-index:25}
.jsearch{position:absolute;top:70px;left:50%;transform:translateX(-50%);width:640px;background:var(--surface);border:1px solid var(--line-2);border-radius:14px;box-shadow:var(--shadow-3);z-index:31;overflow:hidden}
.jnote{font-size:12.5px;color:var(--muted)}
.jkv{display:grid;grid-template-columns:auto 1fr;gap:6px 14px;font-size:13px}
.jkv dt{color:var(--muted)}.jkv dd{margin:0}
.jlist{display:flex;flex-direction:column}
.jlist>*{padding:10px 2px;border-top:1px solid var(--line)}
.jlist>*:first-child{border-top:0}
.jtag{font:600 11px var(--mono);color:var(--text-2);background:var(--surface-3);border-radius:5px;padding:1px 6px}
.jtoast{position:fixed;left:50%;bottom:22px;transform:translateX(-50%);background:var(--text);color:var(--bg);padding:9px 14px;border-radius:9px;font-size:13px;z-index:60;box-shadow:var(--shadow-2)}
`);
