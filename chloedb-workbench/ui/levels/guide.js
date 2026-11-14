// Guide (sixth build): GUIDE.md, served by Workbench at /guide.md, read in
// the app. What each screen is for and when to use it, a first ten minutes,
// and the keyboard. Links to screens (#/map) open them; a contents list on
// the side jumps to a section. A small Markdown reader: headings,
// paragraphs, lists, tables, fenced code, quotes, `code`, **bold**,
// *italic*, [links](…) and <kbd>keys</kbd>.
import { h, useStyle } from "./common.js";

const slug = (t) => String(t).toLowerCase().replace(/<[^>]+>/g, "").replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");

/** Inline Markdown as nodes. */
export function inline(text, go) {
  const out = [];
  const re = /`([^`]+)`|\*\*([^*]+)\*\*|\*([^*\s][^*]*)\*|\[([^\]]+)\]\(([^)\s]+)\)|<kbd>([^<]+)<\/kbd>/g;
  const s = String(text ?? "");
  let at = 0, m;
  while ((m = re.exec(s))) {
    if (m.index > at) out.push(s.slice(at, m.index));
    if (m[1] != null) out.push(h("code", { class: "lv-code" }, m[1]));
    else if (m[2] != null) out.push(h("b", null, inline(m[2], go)));
    else if (m[3] != null) out.push(h("i", null, m[3]));
    else if (m[4] != null) {
      const href = m[5];
      const inApp = href.startsWith("#/");
      const safe = inApp || href.startsWith("#") || /^https?:\/\//.test(href) ? href : "#";
      out.push(h("a", {
        href: safe, target: inApp || safe.startsWith("#") ? null : "_blank", rel: inApp ? null : "noopener",
        onclick: inApp && go ? (e) => { e.preventDefault(); go(href); } : null,
      }, inline(m[4], go)));
    } else out.push(h("kbd", null, m[6]));
    at = re.lastIndex;
  }
  if (at < s.length) out.push(s.slice(at));
  return out;
}

/** Block Markdown as nodes, and its ## headings for the contents. */
export function render(md, go) {
  const lines = String(md).replace(/\r/g, "").split("\n");
  const out = [], toc = [];
  let i = 0;
  const para = [];
  const flush = () => { if (para.length) { out.push(h("p", null, inline(para.join(" "), go))); para.length = 0; } };
  while (i < lines.length) {
    const l = lines[i];
    let m;
    if (/^```/.test(l)) {
      flush();
      const body = [];
      i++;
      while (i < lines.length && !/^```/.test(lines[i])) body.push(lines[i++]);
      i++;
      out.push(h("pre", { class: "gd-pre" }, h("code", null, body.join("\n"))));
      continue;
    }
    if ((m = l.match(/^(#{1,4})\s+(.*)$/))) {
      flush();
      const n = m[1].length, id = slug(m[2]);
      if (n === 2 || n === 3) toc.push({ id, text: m[2].replace(/:.*$/, ""), sub: n === 3 });
      out.push(h(`h${n}`, { id: `g-${id}`, class: `gd-h${n}` }, inline(m[2], go)));
      i++;
      continue;
    }
    if (/^\s*\|/.test(l)) {
      flush();
      const rows = [];
      while (i < lines.length && /^\s*\|/.test(lines[i])) rows.push(lines[i++]);
      const cells = (r) => r.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      const body = rows.filter((r) => !/^\s*\|[\s:|-]+\|\s*$/.test(r));
      const [head, ...rest] = body;
      out.push(h("div", { class: "gd-tw" }, h("table", { class: "gd-t" },
        h("thead", null, h("tr", null, cells(head).map((c) => h("th", null, inline(c, go))))),
        h("tbody", null, rest.map((r) => h("tr", null, cells(r).map((c) => h("td", null, inline(c, go)))))))));
      continue;
    }
    if (/^\s*([-*]|\d+\.)\s+/.test(l)) {
      flush();
      const ordered = /^\s*\d+\./.test(l);
      const items = [];
      while (i < lines.length && (/^\s*([-*]|\d+\.)\s+/.test(lines[i]) || (/^\s{2,}\S/.test(lines[i]) && items.length))) {
        const t = lines[i];
        if (/^\s*([-*]|\d+\.)\s+/.test(t) && !/^\s{2,}/.test(t)) items.push(t.replace(/^\s*([-*]|\d+\.)\s+/, ""));
        else items[items.length - 1] += " " + t.trim().replace(/^([-*]|\d+\.)\s+/, "· ");
        i++;
      }
      out.push(h(ordered ? "ol" : "ul", { class: "gd-l" }, items.map((t) => h("li", null, inline(t, go)))));
      continue;
    }
    if (/^>\s?/.test(l)) {
      flush();
      const q = [];
      while (i < lines.length && /^>\s?/.test(lines[i])) q.push(lines[i++].replace(/^>\s?/, ""));
      out.push(h("blockquote", { class: "gd-q" }, inline(q.join(" "), go)));
      continue;
    }
    if (!l.trim()) { flush(); i++; continue; }
    para.push(l.trim());
    i++;
  }
  flush();
  return { nodes: out, toc };
}

export function mount(root, ctx) {
  const el = h("div", { class: "lv-screen gd" });
  root.append(el);
  let alive = true;
  const go = (hash) => ctx.go(hash);
  el.append(h("div", { class: "gd-wrap" }, h("div", { class: "lv-sub" }, "Reading the guide…")));
  fetch("/guide.md", { cache: "no-store" }).then((r) => (r.ok ? r.text() : Promise.reject(new Error(`${r.status} ${r.statusText}`)))).then((text) => {
    if (!alive) return;
    const { nodes, toc } = render(text, go);
    const nav = h("nav", { class: "gd-toc", "aria-label": "Contents" }, h("div", { class: "gd-toc-h" }, "Contents"),
      toc.map((t) => h("a", { href: `#g-${t.id}`, class: t.sub ? "sub" : null, onclick: (e) => { e.preventDefault(); el.querySelector(`#g-${CSS.escape(t.id)}`)?.scrollIntoView({ behavior: "smooth", block: "start" }); } }, inline(t.text))));
    el.replaceChildren(h("div", { class: "gd-wrap" }, nav, h("article", { class: "gd-doc" }, nodes)));
  }).catch((e) => {
    if (alive) el.replaceChildren(h("div", { class: "gd-wrap" }, h("div", { class: "lv-card" }, `The guide did not load: ${e.message || e}. It is GUIDE.md beside Workbench's README.`)));
  });
  return { update() {}, unmount() { alive = false; el.remove(); } };
}

useStyle("guide", `
.lv-screen.gd{overflow:auto}
.gd-wrap{display:grid;grid-template-columns:220px minmax(0,760px);gap:32px;padding:24px 28px 60px;max-width:1080px;margin:0 auto}
.gd-toc{position:sticky;top:20px;align-self:start;display:flex;flex-direction:column;gap:2px;font-size:12.5px}
.gd-toc-h{color:var(--muted);font-size:11px;font-weight:650;text-transform:uppercase;letter-spacing:.06em;margin-bottom:6px}
.gd-toc a{color:var(--text-2);text-decoration:none;padding:4px 8px;border-radius:6px}
.gd-toc a.sub{padding-left:20px;font-size:12px}
.gd-toc a:hover{background:var(--surface-2);color:var(--text)}
.gd-doc{font-size:14px;line-height:1.6;color:var(--text)}
.gd-doc h1{font-size:26px;margin:0 0 6px;letter-spacing:-.015em}
.gd-doc h2{font-size:19px;margin:30px 0 8px;padding-top:6px;border-top:1px solid var(--line);scroll-margin-top:12px}
.gd-doc h3{font-size:15px;margin:20px 0 6px}
.gd-doc h4{font-size:13.5px;margin:14px 0 4px;color:var(--text-2)}
.gd-doc p{margin:8px 0}
.gd-doc a{color:var(--accent);font-weight:600;text-decoration:none}.gd-doc a:hover{text-decoration:underline}
.gd-l{margin:8px 0;padding-left:22px}.gd-l li{margin:3px 0}
.gd-q{margin:12px 0;padding:8px 14px;border-left:3px solid var(--accent);background:var(--accent-soft);border-radius:0 8px 8px 0;color:var(--text-2)}
.gd-pre{background:var(--surface-2);border:1px solid var(--line);border-radius:8px;padding:10px 12px;overflow:auto;font:12.5px/1.5 var(--mono)}
.gd-tw{overflow-x:auto;margin:10px 0}
.gd-t{border-collapse:collapse;width:100%;font-size:13px}
.gd-t th,.gd-t td{text-align:left;vertical-align:top;padding:6px 10px;border-bottom:1px solid var(--line)}
.gd-t th{color:var(--muted);font-size:11.5px;text-transform:uppercase;letter-spacing:.05em}
.gd-doc kbd{font:11.5px var(--mono);border:1px solid var(--line-2);border-bottom-width:2px;border-radius:4px;padding:0 5px;background:var(--surface)}
@media (max-width:860px){.gd-wrap{grid-template-columns:1fr;padding:16px}.gd-toc{position:static}}
`);
