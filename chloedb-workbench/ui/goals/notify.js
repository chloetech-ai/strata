// The interrupt model (JOURNEYS.md, "Rules and notifications"; the UX
// critique's power-user recommendation 5).
//
//   const n = init(ctx)   starts polling GET /api/notify/pending (every 15 s,
//                         at once when the page comes back, and on n.poke())
//   n.control             a node for the top bar (desktop) or More (phone): a
//                         bell that opens the Notifications sheet
//   n.open()              opens that sheet: "Notify me", quiet, a level per project
//   n.openDigest()        shows the digest now, if there is one
//   n.poke()              re-reads at once (call it on the stream's changes)
//   n.stop()
//
// A desktop notification carries the agent, what it asks, the command and
// Claude Code's options, and never answers anything: clicking it opens that
// card in Workbench, focused, to be answered with a digit and Enter.
import { h, useStyle, btn, icon, label, fmtDur, fmtClock, onEscape } from "/levels/common.js";

const POLL_MS = 15000;
const SEEN_KEY = "chloedb-workbench-notified";

useStyle("notify", `
.nt-bell{position:relative;display:inline-flex;align-items:center;gap:var(--s-1);height:var(--ctl-h);padding:0 var(--s-2);border:var(--hair) solid transparent;border-radius:var(--r-sm);background:none;color:var(--text-2);cursor:pointer;font:var(--w-mid) var(--fs-sm)/1 var(--sans)}
.nt-bell:hover{background:var(--surface-2);color:var(--text)}
.nt-bell.quiet{color:var(--warn-ink);border-color:var(--warn-line);background:var(--warn-soft)}
.nt-bell .d{position:absolute;top:5px;right:5px;width:6px;height:6px;border-radius:50%;background:var(--accent)}
.nt-scrim{position:fixed;inset:0;z-index:90;background:var(--scrim);display:grid;place-items:center;padding:var(--s-5)}
.nt-sheet{width:min(var(--sheet-w),100%);max-height:calc(100vh - 2 * var(--s-5));overflow:auto;background:var(--surface);border:var(--hair) solid var(--line);border-radius:var(--r);box-shadow:var(--shadow-3)}
.nt-hd{display:flex;align-items:flex-start;gap:var(--s-3);padding:var(--s-4) var(--s-5) 0}
.nt-hd h2{margin:var(--s-1) 0 0;font:var(--w-title) var(--fs-lg)/var(--lh-lg) var(--sans);letter-spacing:var(--track-md)}
.nt-hd .k{font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2)}
.nt-hd .x{margin-left:auto}
.nt-bd{padding:var(--s-3) var(--s-5) var(--s-5);display:flex;flex-direction:column;gap:var(--s-5)}
.nt-row{display:flex;align-items:center;gap:var(--s-3);min-height:var(--ctl-h-lg)}
.nt-row .t{flex:1;min-width:0}
.nt-row .t b{display:block;font-weight:var(--w-mid);font-size:var(--fs-body)}
.nt-row .t span{display:block;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.nt-sw{width:40px;height:24px;border-radius:var(--r-pill);border:var(--hair) solid var(--line-2);background:var(--surface-3);position:relative;cursor:pointer;flex:none;padding:0}
.nt-sw::after{content:"";position:absolute;top:2px;left:2px;width:18px;height:18px;border-radius:50%;background:var(--surface);box-shadow:var(--shadow-2);transition:transform .15s}
.nt-sw[aria-checked="true"]{background:var(--accent);border-color:var(--accent)}
.nt-sw[aria-checked="true"]::after{transform:translateX(16px)}
.nt-say{margin:0;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.nt-say.warn{color:var(--warn-ink)}
.nt-levels{display:flex;flex-direction:column;border:var(--hair) solid var(--line);border-radius:var(--r-sm);overflow:hidden}
.nt-proj{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:var(--s-2) var(--s-3);align-items:center;padding:var(--s-3);border-bottom:var(--hair) solid var(--line)}
.nt-proj:last-child{border-bottom:0}
.nt-proj b{font:var(--w-mid) var(--fs-body)/var(--lh-body) var(--mono);overflow-wrap:anywhere}
.nt-proj b.def{font-family:var(--sans)}
.nt-proj small{grid-column:1 / -1;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.nt-seg{display:inline-flex;border:var(--hair) solid var(--line-2);border-radius:var(--r-sm);overflow:hidden;flex-wrap:wrap}
.nt-seg button{border:0;background:none;color:var(--text-2);font:var(--w-mid) var(--fs-sm)/1 var(--sans);height:var(--ctl-h);padding:0 var(--s-3);cursor:pointer}
.nt-seg button+button{border-left:var(--hair) solid var(--line-2)}
.nt-seg button[aria-pressed="true"]{background:var(--accent-soft);color:var(--accent-text)}
.nt-min{display:inline-flex;align-items:center;gap:var(--s-1);color:var(--text-2);font-size:var(--fs-sm)}
.nt-min input{width:56px;height:var(--ctl-h);padding:0 var(--s-2);border-radius:var(--r-sm);border:var(--hair) solid var(--line-2);background:var(--surface);color:var(--text);font:var(--w-text) var(--fs-body) var(--mono)}
.nt-quiet{display:flex;gap:var(--s-2);flex-wrap:wrap;align-items:center}
.nt-tiles{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:var(--hair);background:var(--line);border:var(--hair) solid var(--line);border-radius:var(--r);overflow:hidden}
.nt-tiles>div{background:var(--surface);padding:var(--s-3)}
.nt-tiles b{display:block;font:var(--w-mid) var(--fs-lg)/var(--lh-lg) var(--mono);letter-spacing:-.03em;margin-top:var(--s-1)}
.nt-sent{margin:0;font-size:var(--fs-md);line-height:var(--lh-md);font-weight:var(--w-mid);letter-spacing:var(--track-md)}
.nt-list{display:flex;flex-direction:column;border:var(--hair) solid var(--line);border-radius:var(--r-sm);overflow:hidden}
.nt-it{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:var(--s-1) var(--s-3);padding:var(--s-3);border-bottom:var(--hair) solid var(--line);align-items:start}
.nt-it:last-child{border-bottom:0}
.nt-it b{font-weight:var(--w-mid)}
.nt-it .w{font:var(--w-text) var(--fs-xs)/var(--lh-xs) var(--mono);color:var(--text-2);white-space:nowrap}
.nt-it p{grid-column:1 / -1;margin:0;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.nt-it pre{grid-column:1 / -1;margin:0;padding:var(--s-2);border-radius:var(--r-xs);background:var(--well);font:var(--w-text) var(--fs-xs)/var(--lh-xs) var(--mono);white-space:pre-wrap;overflow-wrap:anywhere}
.nt-it .o{grid-column:1 / -1;font:var(--w-text) var(--fs-xs)/var(--lh-xs) var(--mono);color:var(--text-2)}
.nt-it .btn{grid-column:1 / -1;justify-self:start}
.nt-it.crit{box-shadow:inset 3px 0 0 var(--crit)}
.nt-ft{position:sticky;bottom:0;display:flex;justify-content:flex-end;gap:var(--s-2);padding:var(--s-3) var(--s-5) var(--s-4);border-top:var(--hair) solid var(--line);background:var(--surface)}
@media (max-width:640px){
  .nt-scrim{place-items:end stretch;padding:0}
  .nt-sheet{width:100%;max-height:92vh;border-radius:var(--r) var(--r) 0 0}
  .nt-hd,.nt-bd{padding-left:var(--s-4);padding-right:var(--s-4)}
  .nt-ft{padding:var(--s-3) var(--s-4) calc(var(--s-3) + env(safe-area-inset-bottom))}
  .nt-ft .btn{flex:1;justify-content:center;min-height:var(--touch)}
  .nt-tiles{grid-template-columns:repeat(2,minmax(0,1fr))}
  .nt-proj{grid-template-columns:minmax(0,1fr)}
  .nt-seg{display:flex}.nt-seg button{flex:1;height:var(--touch)}
  .nt-min input{height:var(--touch);font-size:16px}
  .nt-quiet .btn,.nt-it .btn{min-height:var(--touch)}
  .nt-sw{width:52px;height:32px}.nt-sw::after{width:26px;height:26px}.nt-sw[aria-checked="true"]::after{transform:translateX(20px)}
}
@media (pointer:coarse){.nt-bell{min-height:var(--touch);min-width:var(--touch);justify-content:center}.nt-seg button{min-height:var(--touch)}}
`);

function readSeen() {
  try { return new Map(JSON.parse(localStorage.getItem(SEEN_KEY) || "[]")); } catch { return new Map(); }
}
function writeSeen(m) {
  // Keep a day of ids.
  const cut = Date.now() - 86400000;
  try { localStorage.setItem(SEEN_KEY, JSON.stringify([...m].filter(([, t]) => t > cut))); } catch { /* private mode: per page only */ }
}

const canNotify = () => typeof window.Notification === "function";
const perm = () => (canNotify() ? Notification.permission : "unsupported");

/** A notification's text: what it asks, the command, Claude Code's options. */
export function notificationOf(it) {
  const title = it.kind === "alert" ? `${it.title}: ${it.ask}` : `${it.name} waits for you${it.waited_ms > 60000 ? ` · ${fmtDur(it.waited_ms)}` : ""}`;
  const lines = [];
  if (it.kind !== "alert") lines.push(it.ask);
  if (it.command && it.command.text) lines.push(it.command.text + (it.command.cut ? `\n… ${it.command.lines} lines in all` : ""));
  if ((it.options || []).length) lines.push(it.options.map((o) => `${o.n} ${o.label}`).join(" · "));
  lines.push(it.kind === "alert" ? "Open Workbench to see it." : "Open Workbench to answer: nothing is answered from here.");
  return { title, body: lines.filter(Boolean).join("\n") };
}

/** Opens a card in Workbench, focused: the route, then the card itself. */
function openItem(ctx, it) {
  try { window.focus(); } catch { /* not allowed */ }
  if (ctx.go) ctx.go(it.href); else location.hash = it.href;
  window.dispatchEvent(new CustomEvent("wb:focus-question", { detail: { question: it.question || null, agent: it.agent || null, href: it.href } }));
  if (!it.question) return;
  const end = Date.now() + 4000;
  (function look() {
    const el = document.querySelector(`.qc[data-q="${CSS.escape(it.question)}"]`);
    if (el) { el.classList.add("focus"); el.scrollIntoView({ block: "center" }); el.focus({ preventScroll: true }); return; }
    if (Date.now() < end) setTimeout(look, 120);
  })();
}

function sheet({ kicker, title, body, foot, onClose }) {
  const scrim = h("div", { class: "nt-scrim", role: "presentation" });
  const close = btn("", { ghost: true, sm: true, icon: "x", cls: "x", title: "Close", onclick: onClose });
  close.setAttribute("aria-label", "Close");
  const box = h("div", { class: "nt-sheet", role: "dialog", "aria-modal": "true", "aria-label": title },
    h("div", { class: "nt-hd" }, h("div", {}, h("div", { class: "k" }, kicker), h("h2", {}, title)), close),
    h("div", { class: "nt-bd" }, body),
    foot ? h("div", { class: "nt-ft" }, foot) : null);
  scrim.append(box);
  scrim.addEventListener("click", (e) => { if (e.target === scrim) onClose(); });
  document.body.append(scrim);
  return scrim;
}

export function init(ctx) {
  const N = { pending: null, settings: null, timer: null, open: null, digestKey: null, err: null, stopped: false };
  const seen = readSeen();
  const dot = h("span", { class: "d", hidden: true });
  const control = h("button", { class: "nt-bell", type: "button", "aria-label": "Notifications", title: "Notifications", onclick: () => open() }, icon("bell"), h("span", { class: "lbl-q", hidden: true }), dot);

  function paintControl() {
    const q = N.pending && N.pending.quiet;
    const on = q && q.on;
    control.classList.toggle("quiet", !!on);
    const l = control.querySelector(".lbl-q");
    l.hidden = !on;
    l.textContent = on ? `Quiet ${fmtDur(q.left_ms)}` : "";
    control.setAttribute("aria-label", on ? `Notifications: quiet for ${fmtDur(q.left_ms)}` : "Notifications");
    dot.hidden = !(N.pending && N.pending.digest);
  }

  async function poll() {
    if (N.stopped) return;
    clearTimeout(N.timer);
    try {
      const p = await ctx.api("/api/notify/pending");
      N.pending = p; N.err = null;
      deliver(p);
      if (p.digest) {
        const key = `${p.digest.reason}:${p.digest.since_ms}`;
        if (N.digestKey !== key && document.visibilityState === "visible") { N.digestKey = key; openDigest(); }
      }
    } catch (e) { N.err = e.message; }
    paintControl();
    if (N.open && N.open.kind === "settings") N.open.render();
    // Wake when the next "blocked over N minutes" falls due.
    const due = Math.min(...((N.pending && N.pending.later) || []).map((x) => x.due_ms - Date.now() + 500).filter((x) => x > 0), POLL_MS);
    N.timer = setTimeout(poll, Math.max(1000, due));
  }

  function deliver(p) {
    const desktop = N.settings ? N.settings.desktop : true;
    for (const it of p.notify || []) {
      if (seen.has(it.id)) continue;
      seen.set(it.id, Date.now());
      // The page in front shows it already (Needs you); a notification is
      // for when you look elsewhere.
      const away = document.visibilityState !== "visible" || !document.hasFocus();
      if (desktop && away && perm() === "granted") {
        const { title, body } = notificationOf(it);
        try {
          const n = new Notification(title, { body, tag: it.id, requireInteraction: it.kind === "alert" && it.critical });
          n.onclick = () => { n.close(); openItem(ctx, it); };
        } catch { /* a browser that only notifies from a service worker */ }
      } else if (ctx.toast && it.kind === "alert" && it.critical) {
        ctx.toast(`${it.title}: ${it.ask}`);
      }
    }
    writeSeen(seen);
  }

  async function loadSettings() {
    try { N.settings = await ctx.api("/api/notify/settings"); } catch (e) { N.err = e.message; }
  }

  async function put(body) {
    try { N.settings = await ctx.api("/api/notify/settings", { method: "PUT", body }); } catch (e) { N.err = e.message; }
    if (N.open) N.open.render();
  }

  async function setQuiet(minutes) {
    try { await ctx.api("/api/notify/quiet", { method: "POST", body: { minutes } }); } catch (e) { N.err = e.message; }
    await poll();
  }

  // "Notify me": the browser's permission is asked here, after this click, never before.
  async function toggleDesktop() {
    const on = !(N.settings && N.settings.desktop && perm() === "granted");
    if (!on) return put({ desktop: false });
    if (!canNotify()) { N.say = "This browser cannot show notifications here. Workbench still counts what needs you."; return N.open && N.open.render(); }
    let p = perm();
    if (p === "default") p = await Notification.requestPermission();
    if (p !== "granted") { N.say = "The browser said no. Allow notifications for this page in its site settings, then turn this on again."; return put({ desktop: false }); }
    N.say = null;
    return put({ desktop: true });
  }

  function settingsBody() {
    const s = N.settings || { default: { level: "blocked", minutes: 2 }, projects: [], levels: [], desktop: false };
    const q = (N.pending && N.pending.quiet) || {};
    const desktopOn = !!s.desktop && perm() === "granted";
    const levelSeg = (cur, onPick) => h("div", { class: "nt-seg", role: "group", "aria-label": "Level" },
      (s.levels || []).map((l) => h("button", { type: "button", "aria-pressed": String(cur === l.id), title: l.says, onclick: () => onPick(l.id) }, l.label.replace("Only when an agent is blocked", "Blocked").replace("Every question", "Every question").replace("Only alerts", "Alerts only"))));
    const minutes = (cur, onSet) => h("label", { class: "nt-min" }, "over", h("input", { type: "number", min: "1", max: "240", value: String(cur), inputmode: "numeric", onchange: (e) => onSet(Number(e.target.value) || 2) }), "min");
    const held = (N.pending && N.pending.held) || [];
    return [
      h("div", {},
        h("div", { class: "nt-row" },
          h("div", { class: "t" }, h("b", {}, "Notify me"), h("span", {}, "Desktop notifications with the command and Claude Code's options. They never answer: a click opens the card here.")),
          h("button", { class: "nt-sw", role: "switch", "aria-checked": String(desktopOn), "aria-label": "Notify me", onclick: toggleDesktop })),
        N.say ? h("p", { class: "nt-say warn" }, N.say) : perm() === "denied" ? h("p", { class: "nt-say warn" }, "The browser blocks notifications for this page; allow them in its site settings.") : null),
      h("div", {},
        label("Quiet", q.on ? h("span", { class: "num" }, `until ${fmtClock(q.until_ms)}`) : null),
        h("div", { class: "nt-quiet", style: { marginTop: "var(--s-2)" } },
          q.on ? [btn("End quiet now", { onclick: () => setQuiet(0) }), btn("25 min more", { ghost: true, onclick: () => setQuiet(Math.round(q.left_ms / 60000) + 25) })]
            : [btn("Quiet for 25 min", { onclick: () => setQuiet(25) }), btn("1 hour", { ghost: true, onclick: () => setQuiet(60) })]),
        h("p", { class: "nt-say", style: { marginTop: "var(--s-2)" } }, q.on ? `Holding ${held.length} for the digest. Critical alerts still notify you.` : "Critical alerts still notify you. The rest waits for one digest when quiet ends.")),
      h("div", {},
        label("When to notify"),
        h("div", { class: "nt-levels", style: { marginTop: "var(--s-2)" } },
          h("div", { class: "nt-proj" }, h("b", { class: "def" }, "Every project"), h("span", {}, levelSeg(s.default.level, (l) => put({ level: l }))),
            s.default.level === "blocked" ? h("small", {}, "An agent waiting ", minutes(s.default.minutes, (m) => put({ minutes: m }))) : h("small", {}, ((s.levels || []).find((l) => l.id === s.default.level) || {}).says || "")),
          (s.projects || []).map((p) => h("div", { class: "nt-proj" }, h("b", {}, p.name), h("span", {}, levelSeg(p.level, (l) => put({ repo: p.repo, level: l }))),
            h("small", {}, p.level === "blocked" ? ["An agent waiting ", minutes(p.minutes, (m) => put({ repo: p.repo, minutes: m }))] : ((s.levels || []).find((l) => l.id === p.level) || {}).says || "",
              p.own ? [" · ", h("button", { class: "lv-link", type: "button", onclick: () => put({ repo: p.repo, reset: true }) }, "use the default")] : null))))),
      N.err ? h("p", { class: "nt-say warn" }, N.err) : null,
    ];
  }

  async function open() {
    if (N.open) return;
    // Held from the click on, so a second click does not open a second sheet.
    N.open = { kind: "settings", render: () => {} };
    await loadSettings();
    // Leaving the page closes it (Priya: it stayed over the next page).
    const close = () => { unesc(); window.removeEventListener("hashchange", close); scrim.remove(); N.open = null; };
    const unesc = onEscape(close);
    window.addEventListener("hashchange", close);
    const scrim = sheet({ kicker: "Interruptions", title: "Notifications", body: settingsBody().filter(Boolean), foot: null, onClose: close });
    // Without the empty rows: replaceChildren(null) writes "null".
    N.open = { kind: "settings", render: () => scrim.querySelector(".nt-bd").replaceChildren(...settingsBody().filter(Boolean)) };
  }

  function itemRow(it) {
    return h("div", { class: `nt-it${it.critical ? " crit" : ""}` },
      h("b", {}, it.kind === "alert" ? it.ask : `${it.name}: ${it.ask}`),
      h("span", { class: "w" }, `${fmtDur(it.waited_ms)}`),
      it.command && it.command.text ? h("pre", {}, it.command.text) : null,
      (it.options || []).length ? h("div", { class: "o" }, it.options.map((o) => `${o.n} ${o.label}`).join(" · ")) : null,
      btn(it.kind === "alert" ? "Open" : "Open the card", { sm: true, onclick: () => { closeDigest(); openItem(ctx, it); } }));
  }

  let closeDigest = () => {};
  function openDigest() {
    const d = N.pending && N.pending.digest;
    if (!d || N.open) return;
    const c = d.counts || {};
    const done = async () => {
      try { await ctx.api("/api/notify/seen", { method: "POST", body: { until_ms: d.until_ms } }); } catch { /* shown again next time */ }
      closeDigest();
      poll();
    };
    closeDigest = () => { unesc(); scrim.remove(); N.open = null; closeDigest = () => {}; };
    const unesc = onEscape(() => closeDigest());
    const body = [
      h("p", { class: "nt-sent" }, d.sentence),
      h("div", { class: "nt-tiles" },
        [["Prompts", c.prompts], ["Answered", (c.by_you || 0) + (c.by_terminal || 0) + (c.by_rule || 0)], ["Still waiting", c.waiting], ["Alerts", c.alerts]].map(([l, n]) => h("div", {}, label(l), h("b", {}, String(n || 0))))),
      (d.waiting || []).length ? h("div", {}, label("Still waiting on you", h("span", { class: "num" }, "oldest first")), h("div", { class: "nt-list", style: { marginTop: "var(--s-2)" } }, d.waiting.map(itemRow))) : null,
      (d.alerts || []).length ? h("div", {}, label("Alerts"), h("div", { class: "nt-list", style: { marginTop: "var(--s-2)" } }, d.alerts.map((a) => h("div", { class: `nt-it${a.critical ? " crit" : ""}` }, h("b", {}, a.title), h("span", { class: "w" }, `${a.severity}${a.state === "resolved" ? " · resolved" : ""}`))))) : null,
      (d.finished || []).length ? h("p", { class: "nt-say" }, `Finished: ${d.finished.map((f) => `${f.name}${f.exit_code ? ` (exit ${f.exit_code})` : ""}`).join(", ")}.`) : null,
    ];
    const scrim = sheet({ kicker: d.reason === "quiet" ? "Quiet is over" : `Since ${fmtClock(d.since_ms)}`, title: d.reason === "quiet" ? "While you were quiet" : "While you were away", body, foot: [btn("Done", { primary: true, onclick: done })], onClose: () => closeDigest() });
    N.open = { kind: "digest", render: () => {} };
  }

  const onVis = () => { if (document.visibilityState === "visible") poll(); };
  document.addEventListener("visibilitychange", onVis);
  loadSettings().then(poll);
  return {
    control,
    open,
    openDigest,
    poke: () => poll(),
    stop() { N.stopped = true; clearTimeout(N.timer); document.removeEventListener("visibilitychange", onVis); },
    get state() { return N.pending; },
  };
}

