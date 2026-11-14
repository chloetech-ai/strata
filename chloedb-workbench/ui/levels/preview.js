// Preview (SPEC.md, "Levels", Preview): the app an agent is running, in an
// iframe through /preview/<agent>/, with the overlay script inside it. A
// click on anything in the app ("wb-pick") opens a bubble that says who owns
// that element (POST /api/preview/route) and sends your words to that agent
// (POST /api/agents/:id/steer). The overlay toggle labels what each agent
// changed on the page ("wb-who" against the agents' diffs).
import {
  h, useStyle, agentColor, avatar, agentChip, ctxChip, pill, fmtAgo, fmtClock, toMs, onEscape, token, card, setFleet, throttle,
} from "./common.js";
import { mountLab } from "./preview-lab.js";
import { serversBox } from "./lifecycle.js";

const EDIT_TOOLS = new Set(["Edit", "MultiEdit", "Write", "NotebookEdit"]);
const URL_OK = /^http:\/\/(127\.0\.0\.1|localhost):\d{1,5}(\/.*)?$/;
const nameOf = (a) => (a && (a.name || a.id)) || "?";
const base = (p) => String(p).split("/").pop();
const hostOf = (u) => { try { const x = new URL(u); return x.host; } catch { return u || ""; } };

/** Class names a diff line sets: className="a b", class='a', className={`a b-${x}`}. */
function lineClasses(text) {
  const out = new Set();
  for (const m of text.matchAll(/class(?:Name)?=\{?["'`]([^"'`]+)["'`]/g)) for (const c of m[1].split(/\s+/)) if (c && !c.includes("$")) out.add(c.replace(/-$/, ""));
  return out;
}

/** What on the page each agent's diff explains: for every element the overlay
 *  reported, the one file whose added lines name its classes or quote its
 *  text; inner boxes of the same file dropped, touching ones joined. */
export function matchPage(elements, diffs, agents) {
  const files = [];
  for (const [id, d] of diffs) {
    const agent = agents.get(id);
    if (!agent || !d) continue;
    for (const f of d.files || []) {
      const added = (f.hunks || []).flatMap((hk) => hk.lines.filter((l) => l.kind === "+"));
      if (!added.length) continue;
      const removed = (f.hunks || []).reduce((s, hk) => s + hk.lines.filter((l) => l.kind === "-").length, 0);
      files.push({ agent, path: f.path, added, removed, classes: added.map((l) => lineClasses(l.text)) });
    }
  }
  const hits = [];
  for (const e of elements) {
    const text = String(e.text || "").replace(/^[↳\s]+/, "").trim();
    let best = null;
    for (const f of files) {
      let score = 0, line = null, why = null;
      f.added.forEach((l, i) => {
        for (const c of e.classes || []) {
          if (c.length >= 3 && f.classes[i].has(c)) {
            const s = 2 + Math.min(c.length, 20) / 20;
            if (s > score) { score = s; line = l; why = "." + c; }
          }
        }
        if (text.length >= 3 && !/^[\d\s.,$€£−-]+$/.test(text) && text.length <= 80) {
          const q = l.text.includes(`"${text}"`) || l.text.includes(`>${text}<`) || l.text.includes(`'${text}'`);
          const s = q ? 3.2 : 0;
          if (s > score) { score = s; line = l; why = `“${text}”`; }
        }
      });
      if (score && (!best || score > best.score)) best = { f, score, line, why };
    }
    if (best) hits.push({ el: e, ...best });
  }
  // Drop boxes inside a box of the same file, then join the ones that touch.
  const inside = (a, b) => a.x >= b.x - 1 && a.y >= b.y - 1 && a.x + a.width <= b.x + b.width + 1 && a.y + a.height <= b.y + b.height + 1;
  const keep = hits.filter((x) => !hits.some((y) => y !== x && y.f === x.f && inside(x.el.rect, y.el.rect) && !(inside(y.el.rect, x.el.rect) && hits.indexOf(y) > hits.indexOf(x))));
  const groups = [];
  for (const x of keep) {
    const r = { ...x.el.rect };
    const g = groups.find((g) => g.f === x.f && r.x < g.r.x + g.r.width + 2 && g.r.x < r.x + r.width + 2 && r.y <= g.r.y + g.r.height + 6 && g.r.y <= r.y + r.height + 6);
    if (g) {
      const x1 = Math.min(g.r.x, r.x), y1 = Math.min(g.r.y, r.y);
      g.r = { x: x1, y: y1, width: Math.max(g.r.x + g.r.width, r.x + r.width) - x1, height: Math.max(g.r.y + g.r.height, r.y + r.height) - y1 };
      if (!g.whys.includes(x.why)) g.whys.push(x.why);
    } else groups.push({ f: x.f, r, line: x.line, whys: [x.why] });
  }
  return groups;
}

export function mount(root, ctx) {
  const S = {
    agentId: ctx.params?.agent || null, overlay: true, who: [], groups: [], diffs: new Map(), edits: [], since: null,
    path: "/", pick: null, route: null, routeErr: null, sending: false, sent: [], setting: false, setErr: null, hi: null, whoId: 0,
  };
  const el = h("div", { class: "lv-screen pv" });
  root.append(el);
  let frame = null, layer = null, stage = null, bubble = null, dropEsc = null;

  const agents = () => (ctx.state?.agents || []).filter((a) => a.running !== false || a.preview_url);
  const byId = () => new Map((ctx.state?.agents || []).map((a) => [a.id, a]));
  const current = () => (ctx.state?.agents || []).find((a) => a.id === S.agentId) || null;
  if (!S.agentId) S.agentId = (agents().find((a) => a.preview_url) || agents()[0] || {}).id || null;
  setFleet(ctx.state?.agents || []);

  // ----------------------------------------------------------------- data
  async function loadSide() {
    const list = agents().filter((a) => a.running !== false);
    const [digest] = await Promise.all([
      ctx.api("/api/digest").catch(() => null),
      ...list.map((a) => ctx.api(`/api/agents/${encodeURIComponent(a.id)}/diff`).then((d) => S.diffs.set(a.id, d), () => {})),
    ]);
    S.since = digest ? toMs(digest.since_ms) : null;
    const edits = [];
    if (S.since) {
      await Promise.all(list.map(async (a) => {
        try {
          const { events } = await ctx.api(`/api/agents/${encodeURIComponent(a.id)}/events?data=1&limit=500`);
          for (const e of events || []) {
            if (e.kind !== "PostToolUse" || !EDIT_TOOLS.has(e.tool) || toMs(e.ts) < S.since) continue;
            let p = null;
            try { p = JSON.parse(e.data || "{}").tool_input?.file_path || null; } catch {}
            if (!p) p = (String(e.summary || "").match(/(\S+\.\w+)\s*$/) || [])[1] || null;
            if (!p) continue;
            if (a.cwd && p.startsWith(a.cwd + "/")) p = p.slice(a.cwd.length + 1);
            edits.push({ agent: a, path: p, ts: toMs(e.ts) });
          }
        } catch {}
      }));
    }
    const last = new Map();
    for (const e of edits) { const k = e.agent.id + " " + e.path; if (!last.has(k) || last.get(k).ts < e.ts) last.set(k, e); }
    S.edits = [...last.values()].sort((a, b) => b.ts - a.ts);
    rematch();
  }

  function rematch() {
    S.groups = matchPage(S.who, S.diffs, byId());
    drawLabels();
    renderSide();
  }

  // --------------------------------------------------------------- render
  let side = null;
  function render() {
    const a = current();
    stage = h("div", { class: "pv-stage" });
    side = h("aside", { class: "pv-side" });
    el.replaceChildren(
      h("header", { class: "lv-head" },
        h("h1", { class: "lv-h1" }, "Preview"),
        h("span", { class: "lv-sub" }, "the app an agent is running · click anything in it to ask for a change; it goes to the agent that made it")),
      h("div", { class: "pv-body" },
        h("div", { class: "pv-browser" }, chrome(a), stage),
        side));
    renderFrame();
    renderSide();
  }

  function chrome(a) {
    const pick = h("select", { class: "pv-pick", "aria-label": "Agent", onchange: (e) => switchTo(e.target.value) },
      agents().map((x) => h("option", { value: x.id, selected: x.id === S.agentId ? true : null }, `${nameOf(x)}${x.preview_url ? "" : " (no preview)"}`)));
    const url = a?.preview_url ? hostOf(a.preview_url) + (S.path === "/" ? "" : S.path) : "no preview URL";
    return h("div", { class: "pv-chrome" },
      h("span", { class: "pv-dots", "aria-hidden": "true" }, h("i"), h("i"), h("i")),
      h("label", { class: "pv-pickw", style: a ? `--c:${agentColor(a)}` : null }, a ? avatar(a, 18) : null, pick),
      h("div", { class: "pv-url", title: a?.preview_url || "" },
        h("span", { class: "pv-url-t" }, url),
        // Workbench cannot tell which checkout the server at this URL serves (an agent's worktree, main…): say only whose URL it is.
        a?.preview_url ? h("span", { class: "pv-url-m", title: "The server at this URL serves whatever checkout it was started in." }, `· ${nameOf(a)}'s preview URL`) : null),
      a?.preview_url ? h("button", { class: "btn ghost icon sm", title: "Reload", "aria-label": "Reload the preview", onclick: reload }, "↻") : null,
      a?.preview_url ? h("button", { class: "btn ghost sm", title: "Set another preview URL", onclick: () => { S.setting = true; renderFrame(); } }, "Change URL") : null,
      a ? mainLink(a) : null,
      h("button", {
        class: "pv-ov" + (S.overlay ? " on" : ""), "aria-pressed": S.overlay ? "true" : "false", title: ovTitle(),
        onclick: (e) => {
          S.overlay = !S.overlay;
          e.currentTarget.classList.toggle("on", S.overlay);
          e.currentTarget.setAttribute("aria-pressed", String(S.overlay));
          e.currentTarget.textContent = ovLabel();
          e.currentTarget.title = ovTitle();
          if (!S.overlay) closeBubble(false);
          pickMode();
          drawLabels();
        },
      }, ovLabel()));
  }
  // Overlay off (SPEC.md, "Sixth build: E"): the page inside gets every
  // click and key, as it would in its own tab; on, a click picks.
  function ovLabel() { return S.overlay ? "overlay: who and why" : "overlay off: the page is yours"; }
  function ovTitle() { return S.overlay ? "A click picks an element and asks its agent. Turn off to use the page itself." : "Clicks and keys go to the page. Turn on to label and pick what each agent changed."; }
  function pickMode() {
    if (!frame || !frame.contentWindow) return;
    try { frame.contentWindow.postMessage({ type: "wb-pick-mode", on: !!S.overlay }, location.origin); } catch {}
  }

  // "Preview main" (Maya): after a merge the page to check is main's,
  // through the same proxy; the project page holds it.
  const mains = new Map();
  function mainLink(a) {
    const host = h("span", { class: "pv-main" });
    const fill = (m) => {
      if (!m || !m.project) return;
      const merged = (m.merged || []).some((x) => x.agent === a.id);
      host.replaceChildren(h("a", { class: `btn sm${merged ? " primary" : " ghost"}`, href: `#/p/${encodeURIComponent(m.project)}/main`, title: m.url ? `main's app at ${m.url}` : "Set where main's app runs" }, "Preview main"));
    };
    if (mains.has(a.repo)) fill(mains.get(a.repo));
    else ctx.api(`/api/repos/main-preview?repo=${encodeURIComponent(a.repo)}`).then((m) => { mains.set(a.repo, m); fill(m); }, () => {});
    return host;
  }

  function previewSrc(a) { return `/preview/${encodeURIComponent(a.id)}/?token=${encodeURIComponent(token(ctx))}`; }

  function renderFrame() {
    if (!stage) return;
    closeBubble(false);
    const a = current();
    if (!a) { stage.replaceChildren(h("div", { class: "pv-empty" }, h("b", null, "No agent to preview."), h("div", { class: "lv-sub" }, "Start an agent that runs a dev server, then pick it here."))); return; }
    if (!a.preview_url || S.setting) { stage.replaceChildren(setForm(a)); frame = null; return; }
    frame = h("iframe", { class: "pv-frame", src: previewSrc(a), title: `${nameOf(a)}'s app` });
    frame.addEventListener("load", onLoad);
    layer = h("div", { class: "pv-layer" + (S.overlay ? "" : " off") });
    stage.replaceChildren(frame, layer);
  }

  function setForm(a) {
    const input = h("input", { class: "pv-in", type: "url", value: a.preview_url || "http://localhost:", placeholder: "http://localhost:5173", "aria-label": "Preview URL", spellcheck: "false" });
    const submit = async (e) => {
      e.preventDefault();
      const url = input.value.trim();
      if (!URL_OK.test(url)) { S.setErr = "Only http://localhost:<port> or http://127.0.0.1:<port>."; renderFrame(); return; }
      try {
        const r = await ctx.api(`/api/agents/${encodeURIComponent(a.id)}/preview`, { method: "PUT", body: { url } });
        Object.assign(a, r.agent || { preview_url: url });
        shown = a.preview_url || null;
        S.setting = false; S.setErr = null; S.path = "/";
        render();
      } catch (err) { S.setErr = err.message || String(err); renderFrame(); }
    };
    const f = h("form", { class: "pv-set", onsubmit: submit },
      h("div", { class: "pv-set-h" }, avatar(a, 28), h("div", null, h("b", null, a.preview_url ? `Change ${nameOf(a)}'s preview` : `${nameOf(a)} has no preview yet`),
        h("div", { class: "lv-sub" }, "An agent that starts a dev server can say where with ", h("code", { class: "lv-code" }, "set_preview"), ", or set it here. Only this machine: localhost or 127.0.0.1."))),
      h("div", { class: "pv-set-r" }, input, h("button", { class: "btn primary", type: "submit" }, "Set preview URL"),
        a.preview_url ? h("button", { class: "btn", type: "button", onclick: () => { S.setting = false; S.setErr = null; renderFrame(); } }, "Cancel") : null),
      S.setErr ? h("div", { class: "err" }, S.setErr) : null);
    // Focus only when you asked to change it: an empty tab's box would take the page's keys (t, c).
    if (S.setting) requestAnimationFrame(() => { input.focus(); input.setSelectionRange(input.value.length, input.value.length); });
    if (S.setting) { dropEsc?.(); dropEsc = onEscape(() => { dropEsc?.(); dropEsc = null; S.setting = false; S.setErr = null; renderFrame(); }); }
    return f;
  }

  function onLoad() {
    try {
      const p = frame.contentWindow.location.pathname.replace(/^\/preview\/[^/]+/, "") || "/";
      S.path = p;
      const t = el.querySelector(".pv-url-t");
      const a = current();
      if (t && a) t.textContent = hostOf(a.preview_url) + (p === "/" ? "" : p);
    } catch {}
    S.who = [];
    pickMode();
    askWho();
  }
  function askWho() {
    if (!frame || !frame.contentWindow) return;
    S.whoId++;
    // The overlay (ui/preview-overlay.js) answers {type: "wb-who", id, items:
    // [{selector, text, classes, rect}]}, and again on scroll, resize and
    // changes while `watch` holds. Same origin: the preview is served by
    // Workbench under /preview/<agent>/.
    try { frame.contentWindow.postMessage({ type: "wb-who", id: S.whoId, watch: true, max: 800 }, location.origin); } catch {}
  }
  function reload() { closeBubble(false); try { frame.contentWindow.location.reload(); } catch { frame.src = previewSrc(current()); } }

  function switchTo(id) {
    S.agentId = id; S.path = "/"; S.setting = false; S.who = []; S.groups = [];
    ctx.go(`#/preview/${id}`);
    render();
  }

  // --------------------------------------------------------------- labels
  function drawLabels() {
    if (!layer) return;
    layer.classList.toggle("off", !S.overlay);
    layer.querySelectorAll(".pv-box").forEach((x) => x.remove());
    if (!S.overlay) return;
    const W = layer.clientWidth;
    const placed = [];
    const hit = (r) => placed.some((p) => r.x < p.x + p.w && p.x < r.x + r.w && r.y < p.y + p.h && p.y < r.y + r.h);
    const recent = new Set(S.edits.filter((e) => Date.now() - e.ts < 30 * 60000).map((e) => e.agent.id + " " + e.path));
    S.groups.forEach((g, i) => {
      const r = g.r;
      const changed = S.edits.find((e) => e.agent.id === g.f.agent.id && e.path === g.f.path);
      const text = `${nameOf(g.f.agent)} · ${base(g.f.path)}${changed ? ` · changed ${fmtAgo(changed.ts)}` : ""}`;
      const lw = Math.min(W - 8, text.length * 5.9 + 16), lh = 17;
      // On the box's top edge, half outside, so it hides little of the row above.
      const tries = [{ x: r.x + 6, y: r.y - lh + 6 }, { x: r.x + r.width - lw - 6, y: r.y - lh + 6 }, { x: r.x + 6, y: r.y + r.height - 6 }, { x: r.x + r.width - lw - 6, y: r.y + 4 }];
      let at = tries.find((t) => t.y >= 0 && !hit({ ...t, w: lw, h: lh })) || tries[0];
      at = { x: Math.max(2, Math.min(W - lw - 2, at.x)), y: Math.max(0, at.y) };
      placed.push({ ...at, w: lw, h: lh });
      const box = h("div", {
        class: "pv-box" + (recent.has(g.f.agent.id + " " + g.f.path) ? " glow" : "") + (S.hi === i ? " hi" : ""),
        style: `--c:${agentColor(g.f.agent)};left:${r.x - 3}px;top:${r.y - 3}px;width:${r.width + 6}px;height:${r.height + 6}px`,
      }, h("span", { class: "pv-lab", style: `left:${at.x - r.x + 1}px;top:${at.y - r.y + 1}px` }, text));
      layer.append(box);
    });
  }

  // --------------------------------------------------------------- picking
  async function onPick(p) {
    if (!p || !p.rect || !S.overlay) return;
    S.pick = p; S.route = null; S.routeErr = null;
    openBubble();
    try {
      S.route = await ctx.api("/api/preview/route", { method: "POST", body: { agent: S.agentId, pick: p } });
    } catch (e) { S.routeErr = e.message || String(e); }
    if (S.pick === p) fillRoute();
  }

  function openBubble() {
    closeBubble(false);
    const p = S.pick, r = p.rect;
    const W = layer.clientWidth, H = layer.clientHeight;
    const mark = h("div", { class: "pv-mark", style: `left:${r.x - 3}px;top:${r.y - 3}px;width:${r.width + 6}px;height:${r.height + 6}px` }, h("span", { class: "pv-mark-l" }, "you clicked"));
    const ta = h("textarea", { class: "pv-ta", rows: "2", placeholder: "Say what you want here: “make this a split button: Full / Partial”", "aria-label": "What you want changed" });
    import("./complete.js").then((m) => m.attachComplete(ta, { field: "steer", ctx, agent: () => S.agentId })).catch(() => {}); // Fifth build: autocomplete
    const send = h("button", { class: "btn primary sm pv-send", disabled: true, onclick: () => doSend(ta.value) }, "Send");
    ta.addEventListener("input", () => { send.disabled = !ta.value.trim() || !S.route || S.sending; });
    ta.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); if (!send.disabled) doSend(ta.value); } });
    const bw = Math.min(400, W - 16);
    const below = r.y + r.height + 12;
    const left = Math.max(8, Math.min(W - bw - 8, r.x + Math.min(r.width, 60)));
    bubble = h("div", { class: "pv-bub", role: "dialog", "aria-label": "Ask for a change to this element", style: `left:${left}px;width:${bw}px` },
      h("div", { class: "pv-bub-h" }, avatar("you", 24), h("b", null, "Ask for a change"),
        h("code", { class: "pv-sel", title: p.selector }, p.selector || ""),
        h("button", { class: "btn ghost icon sm", "aria-label": "Close (Esc)", title: "Close (Esc)", onclick: () => closeBubble(true) }, "×")),
      ta,
      h("div", { class: "pv-route" }, h("span", { class: "spin" }), "Finding who made this…"),
      h("div", { class: "pv-bub-f" }, send, h("button", { class: "btn sm", onclick: () => closeBubble(true) }, "Cancel"), h("span", { class: "pv-kb" }, h("kbd", null, navigator.platform.includes("Mac") ? "⌘" : "Ctrl"), h("kbd", null, "↵"))));
    layer.append(mark, bubble);
    layer.classList.add("picking");
    const bh = bubble.offsetHeight;
    bubble.style.top = (below + bh < H - 8 ? below : Math.max(8, r.y - bh - 12)) + "px";
    dropEsc?.(); dropEsc = onEscape(() => closeBubble(true));
    ta.focus();
  }

  function fillRoute() {
    if (!bubble) return;
    const slot = bubble.querySelector(".pv-route");
    const send = bubble.querySelector(".pv-send");
    const ta = bubble.querySelector(".pv-ta");
    if (S.routeErr || !S.route || !S.route.owner) {
      slot.replaceChildren(h("span", { class: "lv-sub" }, S.routeErr ? `Could not route it: ${S.routeErr}` : "No agent claims it."));
      return;
    }
    const { owner, files = [], related = [] } = S.route;
    slot.replaceChildren(...[
      h("div", null, "Goes to ", agentChip(owner, ctx),
        files.length ? [", which owns ", h("code", { class: "lv-code" }, files.map(base).join(", "))] : ", whose preview this is",
        `, with the element${related.length ? ` and ${related.length} related record${related.length > 1 ? "s" : ""} from ChloeCtx` : ""}.`),
      related.length ? h("div", { class: "pv-rel" }, related.map((r) => h("span", { class: "pv-relc", title: r.text }, ctxChip(r.kind), h("span", null, r.text)))) : null].filter(Boolean));
    send.textContent = `Send to ${nameOf(owner)}`;
    send.disabled = !ta.value.trim() || S.sending;
    const H = layer.clientHeight, top = parseFloat(bubble.style.top);
    if (top + bubble.offsetHeight > H - 8) bubble.style.top = Math.max(8, H - bubble.offsetHeight - 8) + "px";
  }

  function closeBubble(focus) {
    layer?.querySelectorAll(".pv-mark,.pv-bub").forEach((x) => x.remove());
    layer?.classList.remove("picking");
    bubble = null;
    if (!S.setting) { dropEsc?.(); dropEsc = null; }
    if (focus) frame?.focus();
  }

  async function doSend(words) {
    const p = S.pick, route = S.route;
    if (!p || !route?.owner || !words.trim()) return;
    const a = current();
    const text = [
      words.trim(),
      "",
      `(From the preview of ${nameOf(a)}'s app at ${a?.preview_url || ""}${S.path === "/" ? "" : S.path}: the element \`${p.selector}\`${p.text ? `, text “${String(p.text).slice(0, 160)}”` : ""}.`,
      p.html ? `Its HTML: ${String(p.html).slice(0, 800)}` : "",
      route.files?.length ? `It comes from ${route.files.join(", ")}.` : "",
      route.related?.length ? `Related from ChloeCtx: ${route.related.map((r) => `${r.kind} ${r.id}: ${r.text}`).join(" | ")}` : "",
      ")",
    ].filter(Boolean).join("\n");
    S.sending = true;
    const btn = bubble?.querySelector(".pv-send");
    if (btn) { btn.disabled = true; btn.textContent = "Sending…"; }
    try {
      await ctx.api(`/api/agents/${encodeURIComponent(route.owner.id)}/steer`, { method: "POST", body: { text } });
      S.sent.unshift({ owner: route.owner, words: words.trim(), selector: p.selector, ts: Date.now() });
      closeBubble(true);
      renderSide();
    } catch (e) {
      if (btn) { btn.disabled = false; btn.textContent = `Send to ${nameOf(route.owner)}`; }
      bubble?.querySelector(".pv-route")?.append(h("div", { class: "err" }, e.message || String(e)));
    }
    S.sending = false;
  }

  // ------------------------------------------------------------------ side
  function renderSide() {
    if (!side) return;
    side.replaceChildren(...[serversCard(), changesCard(), sinceCard(), sentCard()].filter(Boolean));
  }
  // The servers the agent started (SPEC.md, "Sixth build: E"), one box per
  // agent shown, kept across renders; hidden while it has none.
  let srv = null;
  function serversCard() {
    if (!S.agentId) return null;
    if (!srv || srv.agent !== S.agentId) {
      srv?.box.destroy();
      const box = serversBox(ctx.api, S.agentId, { title: "Servers it started", onChange: (list) => { c.hidden = !list.length; } });
      const c = card(null, null, box.el, h("p", { class: "pv-p lv-sub" }, "When you stop the agent, Workbench asks whether these stop too."));
      c.hidden = true;
      srv = { agent: S.agentId, box, card: c };
    }
    return srv.card;
  }

  function fileStats(agentId, path) {
    const f = (S.diffs.get(agentId)?.files || []).find((x) => x.path === path);
    if (!f) return null;
    let add = 0, del = 0;
    for (const hk of f.hunks || []) for (const l of hk.lines) { if (l.kind === "+") add++; else if (l.kind === "-") del++; }
    return { add, del, f };
  }

  function changesCard() {
    const a = current();
    const seen = new Map();
    S.groups.forEach((g, i) => { const k = g.f.agent.id + " " + g.f.path; if (!seen.has(k)) seen.set(k, { g, idx: [i], whys: [...g.whys] }); else { seen.get(k).idx.push(i); seen.get(k).whys.push(...g.whys); } });
    let rows = [...seen.values()];
    const fallback = !rows.length;
    if (fallback && a) rows = (S.diffs.get(a.id)?.files || []).map((f) => ({ g: { f: { agent: a, path: f.path }, line: (f.hunks || []).flatMap((hk) => hk.lines).find((l) => l.kind === "+") }, idx: [], whys: [] }));
    const body = rows.length ? rows.map(({ g, idx, whys }) => {
      const st = fileStats(g.f.agent.id, g.f.path);
      const line = g.line?.new_line ?? null;
      return h("div", {
        class: "pv-chg",
        onmouseenter: () => { S.hi = idx[0] ?? null; drawLabels(); },
        onmouseleave: () => { S.hi = null; drawLabels(); },
      },
      avatar(g.f.agent, 26),
      h("div", { class: "pv-chg-b" },
        h("div", { class: "pv-chg-t" }, h("b", null, base(g.f.path)), h("span", { class: "lv-sub pv-dir" }, g.f.path.slice(0, -base(g.f.path).length))),
        h("div", { class: "pv-chg-m" },
          st ? [h("span", { class: "n-add" }, `+${st.add}`), " ", h("span", { class: "n-del" }, `−${st.del}`)] : null,
          whys.length ? h("span", { class: "lv-sub" }, " · ", [...new Set(whys)].slice(0, 2).join(", ")) : null,
          h("span", { class: "lv-sub" }, " · ", nameOf(g.f.agent)),
          h("button", { class: "lv-link pv-why", onclick: () => ctx.openWhy({ agent: g.f.agent.id, path: g.f.path, line }) }, "Why?"))));
    }) : [h("div", { class: "lv-sub" }, a ? `${nameOf(a)}'s branch has nothing ${(S.diffs.get(a.id)?.landing || {}).onto || "main"} does not have: it changed nothing yet, or its change is merged.` : "Pick an agent.")];
    // The page itself may be a file it changed (/ is index.html).
    const pageFile = S.path === "/" || !S.path ? "index.html" : base(S.path);
    const isPage = fallback && rows.find((r) => base(r.g.f.path) === pageFile);
    const note = !fallback || !rows.length ? null : isPage ? `This page is served from ${isPage.g.f.path}, which ${nameOf(a)} changed; no element on it is matched to a line yet.` : "None of it is matched to an element of this page yet.";
    return card(fallback ? `What ${nameOf(a)} changed` : "Changes on this page", fallback ? null : h("span", { class: "lv-sub" }, `${rows.length} from ${new Set(rows.map((r) => r.g.f.agent.id)).size} agents`),
      note ? h("div", { class: "lv-sub pv-note" }, note) : null, ...body);
  }

  function sinceCard() {
    if (!S.since) return null;
    const onPage = new Set(S.groups.map((g) => g.f.agent.id + " " + g.f.path));
    const list = S.edits;
    const nAgents = new Set(list.map((e) => e.agent.id)).size;
    const here = list.filter((e) => onPage.has(e.agent.id + " " + e.path)).length;
    return card(`Since you last looked · ${fmtClock(S.since)}`, null,
      h("p", { class: "pv-p" }, list.length
        ? [h("b", null, `${list.length} file${list.length > 1 ? "s" : ""} changed`), ` by ${nAgents} agent${nAgents > 1 ? "s" : ""}`, here ? [`; ${here} of them on this page, and those glow.`] : "; none of them on this page."]
        : (() => {
          // No edit recorded since then is not "nothing changed": the branches may still differ (a commit, a merge, an edit through the shell).
          const n = [...S.diffs.values()].reduce((k, d) => k + ((d && d.files) || []).length, 0);
          return n ? `No file edit was recorded since then; the agents' branches differ from main in ${n} file${n === 1 ? "" : "s"}, under What changed.` : "Nothing changed since.";
        })()),
      list.length ? h("div", { class: "pv-since" }, list.slice(0, 8).map((e) => h("div", { class: "pv-since-r" },
        avatar(e.agent, 18), h("code", { class: "pv-since-p", title: e.path }, base(e.path)),
        onPage.has(e.agent.id + " " + e.path) ? pill("on this page", "acc") : null,
        h("span", { class: "lv-sub pv-ago" }, fmtAgo(e.ts))))) : null);
  }

  function sentCard() {
    if (!S.sent.length) return card("Ask from the page", null, h("p", { class: "pv-p lv-sub" }, "Click any element in the app: the bubble says which agent made it and what ChloeCtx knows about it, and sends your words to that agent. A working agent gets them at its next tool call; an idle one at once."));
    return card("Sent from this preview", null, S.sent.slice(0, 6).map((s) => h("div", { class: "pv-sent" }, agentChip(s.owner, ctx),
      h("div", { class: "pv-sent-b" }, h("div", null, s.words), h("code", { class: "lv-sub pv-sel" }, s.selector)), h("span", { class: "lv-sub pv-ago" }, fmtAgo(s.ts)))));
  }

  // -------------------------------------------------------------- messages
  function onMessage(e) {
    if (!frame || e.source !== frame.contentWindow || !e.data || typeof e.data !== "object") return;
    if (e.origin !== location.origin) return;
    if (e.data.type === "wb-pick") onPick(e.data);
    else if (e.data.type === "wb-ready") { pickMode(); askWho(); } // a page loaded inside the frame (its load event may come later)
    else if (e.data.type === "wb-who") {
      if (e.data.id != null && e.data.id !== S.whoId) return; // an answer to an older ask
      S.who = (e.data.items || e.data.elements || []).filter((x) => x && x.rect);
      rematch();
    }
  }
  window.addEventListener("message", onMessage);
  const onResize = () => askWho();
  window.addEventListener("resize", onResize);

  render();
  mountLab(el, ctx, () => S.agentId); // Preview lab: persona tests and runtime errors (preview-lab.js)
  loadSide().catch(() => {});
  // The agents' diffs and edits again, at most every 15 s while they work.
  const refreshSide = throttle(() => loadSide().catch(() => {}), 15000);
  let shown = current()?.preview_url || null;
  return {
    update(state) {
      if (state) ctx.state = { ...ctx.state, ...state };
      setFleet(ctx.state?.agents || []);
      const a = current();
      // A preview URL set, changed or cleared (by set_preview, or here).
      if (a && (a.preview_url || null) !== shown && !S.setting) { shown = a.preview_url || null; render(); }
      refreshSide();
    },
    unmount() {
      refreshSide.cancel();
      srv?.box.destroy();
      window.removeEventListener("message", onMessage);
      window.removeEventListener("resize", onResize);
      dropEsc?.();
      el.remove();
    },
  };
}

useStyle("preview", `
.pv{display:flex;flex-direction:column;gap:14px}
.pv-body{flex:1 1 auto;min-height:560px;display:grid;grid-template-columns:minmax(0,1fr) 400px;gap:18px}
.pv-browser{display:flex;flex-direction:column;min-width:0;min-height:0;border:1px solid var(--line-2);border-radius:12px;overflow:hidden;background:var(--surface);box-shadow:var(--shadow-1)}
.pv-chrome{display:flex;align-items:center;gap:8px;height:44px;padding:0 10px;background:var(--surface-2);border-bottom:1px solid var(--line);flex:none;min-width:0}
.pv-dots{display:flex;gap:6px;margin-right:4px}.pv-dots i{width:10px;height:10px;border-radius:50%;background:var(--line-2)}
.pv-pickw{display:inline-flex;align-items:center;gap:6px;height:30px;padding:0 4px 0 6px;border:1px solid var(--line);border-radius:8px;background:var(--surface)}
.pv-pick{border:0;background:transparent;font-weight:600;padding:0 4px;outline:none;max-width:150px;cursor:pointer}
.pv-pick option{background:var(--surface);color:var(--text)}
.pv-url{flex:1;min-width:0;display:flex;align-items:center;height:30px;padding:0 12px;border-radius:8px;background:var(--sunken);border:1px solid var(--line);font:12px var(--mono);color:var(--text-2);white-space:nowrap;overflow:hidden}
.pv-url-t{color:var(--text);margin-right:8px}.pv-url-m{color:var(--muted);overflow:hidden;text-overflow:ellipsis}
.pv-ov{border:1px solid var(--line);border-radius:999px;background:var(--surface);color:var(--muted);font:600 11.5px var(--sans);padding:4px 10px;cursor:pointer;white-space:nowrap}
.pv-ov.on{background:var(--good-soft);color:var(--good-ink);border-color:color-mix(in srgb,var(--good) 40%,transparent)}
.pv-stage{position:relative;flex:1 1 auto;min-height:0;background:var(--surface-2)}
.pv-frame{position:absolute;inset:0;width:100%;height:100%;border:0;background:#fff}
.pv-layer{position:absolute;inset:0;pointer-events:none;overflow:hidden}
.pv-layer.off .pv-box{display:none}
.pv-layer.picking .pv-box{opacity:.25}
.pv-box{position:absolute;border:2px solid var(--c);border-radius:8px;transition:box-shadow .15s}
.pv-box.glow{box-shadow:0 0 0 4px color-mix(in srgb,var(--c) 22%,transparent),0 0 24px color-mix(in srgb,var(--c) 45%,transparent)}
.pv-box.hi{border-width:3px;box-shadow:0 0 0 6px color-mix(in srgb,var(--c) 35%,transparent)}
.pv-lab{position:absolute;white-space:nowrap;font:700 10px/17px var(--sans);height:17px;padding:0 7px;border-radius:6px;background:var(--c);color:#0b0d10;box-shadow:0 1px 3px rgba(0,0,0,.2)}
.pv-mark{position:absolute;border:2px solid var(--warn);border-radius:8px;box-shadow:0 0 0 4px var(--warn-glow)}
.pv-mark-l{position:absolute;left:-2px;top:-21px;font:700 10.5px/19px var(--sans);padding:0 7px;border-radius:6px 6px 6px 0;background:var(--warn);color:#0b0d10;white-space:nowrap}
.pv-bub{position:absolute;pointer-events:auto;background:var(--surface);color:var(--text);border:1px solid var(--line-2);border-radius:12px;padding:12px;box-shadow:var(--shadow-3);display:flex;flex-direction:column;gap:9px;font-size:13px;z-index:2}
.pv-bub-h{display:flex;align-items:center;gap:8px;min-width:0}.pv-bub-h b{flex:none}
.pv-sel{font:11px var(--mono);color:var(--muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0;flex:1}
.pv-ta{width:100%;resize:vertical;min-height:52px;border:1px solid var(--line-2);border-radius:8px;background:var(--sunken);padding:8px 10px;outline:none;line-height:1.4}
.pv-ta:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.pv-route{color:var(--text-2);display:flex;flex-direction:column;gap:8px}
.pv-route .lv-agent{color:var(--text);font-weight:650;vertical-align:-4px}
.pv-route>.spin{display:inline-block}
.pv-route:has(>.spin){flex-direction:row;align-items:center;gap:8px;color:var(--muted)}
.pv-rel{display:flex;flex-direction:column;gap:5px}
.pv-relc{display:flex;gap:7px;align-items:baseline;padding:6px 8px;border-radius:7px;background:color-mix(in srgb,var(--ctx) 9%,transparent);font-size:12px;color:var(--text-2)}
.pv-relc .lv-ctx{flex:none}
.pv-bub-f{display:flex;align-items:center;gap:6px}.pv-kb{margin-left:auto;display:flex;gap:3px}
.pv-empty,.pv-set{position:absolute;inset:0;display:flex;flex-direction:column;justify-content:center;gap:12px;padding:32px max(24px,8%);}
.pv-set-h{display:flex;gap:12px;align-items:flex-start}.pv-set-h b{font-size:16px}
.pv-set-r{display:flex;gap:8px;flex-wrap:wrap}
.pv-in{flex:1;min-width:220px;height:34px;border:1px solid var(--line-2);border-radius:8px;background:var(--surface);padding:0 10px;font:13px var(--mono);outline:none}
.pv-in:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.pv-side{display:flex;flex-direction:column;gap:12px;min-height:0;overflow:auto}
.pv-note{margin:-4px 0 6px}
.pv-chg{display:flex;gap:10px;padding:9px 0;border-top:1px solid var(--line);align-items:flex-start}
.pv-chg:first-of-type{border-top:0}
.lv-card-head+.pv-chg{border-top:0;padding-top:0}
.pv-chg-b{min-width:0;flex:1}
.pv-chg-t{display:flex;align-items:baseline;gap:6px;min-width:0}.pv-chg-t b{font-weight:650}
.pv-dir{font:11px var(--mono);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.pv-chg-m{margin-top:2px;font-size:12.5px;display:flex;flex-wrap:wrap;align-items:baseline;gap:0 3px}
.pv-why{margin-left:6px;font-size:12.5px}
.pv-p{margin:0;color:var(--text-2)}.pv-p b{color:var(--text)}
.pv-since{display:flex;flex-direction:column;gap:6px;margin-top:10px}
.pv-since-r{display:flex;align-items:center;gap:8px;min-width:0}
.pv-since-p{font:12px var(--mono);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.pv-ago{margin-left:auto;white-space:nowrap;font-size:12px}
.pv-sent{display:flex;gap:8px;align-items:flex-start;padding:8px 0;border-top:1px solid var(--line)}
.lv-card-head+.pv-sent{border-top:0;padding-top:0}
.pv-sent-b{min-width:0;flex:1}
@media (max-width:1180px){.pv-body{grid-template-columns:minmax(0,1fr) 330px}}
@media (max-width:860px){.pv-body{grid-template-columns:1fr;min-height:0}.pv-browser{height:min(640px,78vh)}.pv-side{overflow:visible}.pv-url-m{display:none}.pv-dots{display:none}}
@media (max-width:560px){.pv-chrome{flex-wrap:wrap;height:auto;gap:6px;padding:6px 8px}.pv-url{order:5;flex-basis:100%}.pv-chrome .btn.sm:not(.icon){display:none}.pv-chrome .pv-main .btn.sm{display:inline-flex}.pv-ov{padding:4px 8px;margin-left:auto}}
`);
