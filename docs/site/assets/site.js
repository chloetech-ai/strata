/* Strata showcase site: code highlighting, copy buttons, tabs, tooltips,
 * and the renderers for benchmark charts (bench-data.js) and the capability
 * matrix (compare-data.js). No dependencies. */
(function () {
  "use strict";

  var $$ = function (sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); };
  var esc = function (s) {
    return String(s).replace(/[&<>"]/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]; });
  };

  /* ---------------------------------------------------------- highlighting */
  var SQL_KW = new Set(("SELECT FROM WHERE AND OR NOT IN ORDER BY DESC ASC LIMIT OFFSET GROUP HAVING WITH AS JOIN LEFT ON " +
    "INSERT INTO VALUES UPDATE SET DELETE CREATE TABLE ALTER ADD COLUMN DROP RANK PROFILE OPTIONS PRIMARY KEY REFERENCES " +
    "DEFAULT NULL IS BETWEEN LIKE DISTINCT CASE WHEN THEN ELSE END MAX PER EXPLAIN TRUE FALSE EXISTS IF REPLACE RENAME TO " +
    "CONFLICT DO NULLS FIRST LAST INT FLOAT TEXT KEYWORD VECTOR BOOL TIMESTAMP JSON MEDIA EDGES SPARSE CURRENT_TIMESTAMP").split(" "));
  var PY_KW = new Set("import from as def return for in with if else elif not and or True False None lambda".split(" "));
  var RULES = {
    sql: /(?<com>--[^\n]*)|(?<str>'(?:[^']|'')*')|(?<par>:[A-Za-z_]\w*)|(?<num>\b\d+(?:\.\d+)?\b)|(?<fn>\b[A-Za-z_]\w*(?=\())|(?<w>\b[A-Za-z_]\w*\b)/g,
    python: /(?<com>#[^\n]*)|(?<str>"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*')|(?<num>\b\d+(?:\.\d+)?\b)|(?<fn>\b[A-Za-z_]\w*(?=\())|(?<w>\b[A-Za-z_]\w*\b)/g,
    rust: /(?<com>\/\/[^\n]*)|(?<str>"(?:[^"\\\n]|\\.)*")|(?<num>\b\d+(?:\.\d+)?\b)|(?<fn>\b[A-Za-z_]\w*!?(?=\())|(?<w>\b(?:let|mut|fn|use|for|in|if|else|match|return|pub|struct|impl|true|false|Some|None|Ok|Err)\b)/g,
    sh: /(?<com>(?<![^\s])#[^\n]*)|(?<str>"(?:[^"\\]|\\.)*"|'[^']*')|(?<par>(?<![\w-])--?[A-Za-z][\w-]*)/g,
    yaml: /(?<com>(?<![^\s])#[^\n]*)|(?<str>"(?:[^"\\]|\\.)*"|'[^']*')|(?<fn>^[ \t-]*[\w.\/-]+(?=:))|(?<num>\b\d+(?:\.\d+)?\b)/gm,
    json: /(?<fn>"(?:[^"\\]|\\.)*"(?=\s*:))|(?<str>"(?:[^"\\]|\\.)*")|(?<num>-?\b\d+(?:\.\d+)?\b)|(?<w>\b(?:true|false|null)\b)/g
  };
  function highlight(text, lang) {
    var re = RULES[lang];
    if (!re) return esc(text);
    re.lastIndex = 0;
    var out = "", last = 0, m;
    while ((m = re.exec(text))) {
      if (m[0] === "") { re.lastIndex++; continue; }
      out += esc(text.slice(last, m.index));
      var g = m.groups, cls = null;
      if (g.com != null) cls = "com";
      else if (g.str != null) cls = "str";
      else if (g.par != null) cls = "par";
      else if (g.num != null) cls = "num";
      else if (g.fn != null) cls = "fn";
      else if (g.w != null) {
        if (lang === "sql" && SQL_KW.has(m[0])) cls = "kw";
        else if (lang === "python" && PY_KW.has(m[0])) cls = "kw";
        else if (lang === "rust" || lang === "json") cls = "kw";
      }
      out += cls ? '<span class="tok-' + cls + '">' + esc(m[0]) + "</span>" : esc(m[0]);
      last = m.index + m[0].length;
    }
    return out + esc(text.slice(last));
  }
  var LANG_LABEL = { sql: "SQL", python: "Python", sh: "Shell", yaml: "YAML", json: "JSON", rust: "Rust" };
  function wrapCode(pre) {
    var lang = pre.getAttribute("data-lang");
    var text = pre.textContent.replace(/^\n/, "").replace(/\s+$/, "");
    pre.innerHTML = "<code>" + highlight(text, lang) + "</code>";
    if (pre.parentNode && pre.parentNode.classList && pre.parentNode.classList.contains("code")) return;
    var box = document.createElement("div");
    box.className = "code";
    var head = document.createElement("div");
    head.className = "code-head";
    head.innerHTML = "<span>" + esc(pre.getAttribute("data-title") || LANG_LABEL[lang] || lang) + '</span><button class="copy" type="button">Copy</button>';
    pre.parentNode.insertBefore(box, pre);
    box.appendChild(head);
    box.appendChild(pre);
  }
  function copyText(btn, text) {
    var done = function () { btn.textContent = "Copied"; setTimeout(function () { btn.textContent = "Copy"; }, 1400); };
    var fallback = function () {
      var pre = btn.closest(".code").querySelector("pre");
      var r = document.createRange(); r.selectNodeContents(pre);
      var s = window.getSelection(); s.removeAllRanges(); s.addRange(r);
      btn.textContent = "Selected";
      setTimeout(function () { btn.textContent = "Copy"; }, 1400);
    };
    try {
      if (navigator.clipboard && navigator.clipboard.writeText) navigator.clipboard.writeText(text).then(done, fallback);
      else fallback();
    } catch (e) { fallback(); }
  }
  document.addEventListener("click", function (e) {
    var btn = e.target.closest && e.target.closest(".copy");
    if (!btn) return;
    var pre = btn.closest(".code").querySelector("pre");
    copyText(btn, pre.textContent);
  });

  /* ---------------------------------------------------------- tabs */
  function initTabs(root) {
    var buttons = $$("[data-tab]", root);
    var show = function (id) {
      buttons.forEach(function (b) { b.setAttribute("aria-selected", String(b.getAttribute("data-tab") === id)); });
      $$("[data-panel]", root).forEach(function (p) { p.hidden = p.getAttribute("data-panel") !== id; });
    };
    buttons.forEach(function (b) {
      b.setAttribute("role", "tab");
      b.addEventListener("click", function () { show(b.getAttribute("data-tab")); });
    });
    if (buttons.length) show(buttons[0].getAttribute("data-tab"));
  }

  /* ---------------------------------------------------------- tooltip */
  var tip = null;
  function showTip(el, x, y) {
    var t = el.getAttribute("data-tip");
    if (!t) return;
    if (!tip) { tip = document.createElement("div"); tip.className = "tip"; tip.setAttribute("role", "tooltip"); document.body.appendChild(tip); }
    tip.innerHTML = t;
    tip.hidden = false;
    var r = tip.getBoundingClientRect();
    var left = Math.min(window.innerWidth - r.width - 12, Math.max(12, x + 14));
    var top = y + 16 + r.height > window.innerHeight - 8 ? y - r.height - 12 : y + 16;
    tip.style.left = left + "px";
    tip.style.top = Math.max(8, top) + "px";
  }
  function hideTip() { if (tip) tip.hidden = true; }
  document.addEventListener("pointerover", function (e) {
    var el = e.target.closest && e.target.closest("[data-tip]");
    if (el) showTip(el, e.clientX, e.clientY); else hideTip();
  });
  document.addEventListener("pointermove", function (e) {
    var el = e.target.closest && e.target.closest("[data-tip]");
    if (el && tip && !tip.hidden) showTip(el, e.clientX, e.clientY);
  });
  document.addEventListener("focusin", function (e) {
    var el = e.target.closest && e.target.closest("[data-tip]");
    if (el) { var r = el.getBoundingClientRect(); showTip(el, r.left, r.bottom); } else hideTip();
  });
  document.addEventListener("scroll", hideTip, { passive: true });

  /* ---------------------------------------------------------- benchmarks */
  var BENCH = window.STRATA_BENCH || null;
  var STATUS_LABEL = { measured: "Measured", earlier: "Earlier run", placeholder: "Placeholder" };

  function fmt(v, digits) {
    if (digits != null) return v.toLocaleString("en-US", { minimumFractionDigits: digits, maximumFractionDigits: digits });
    var a = Math.abs(v);
    if (a >= 1000) return Math.round(v).toLocaleString("en-US");
    if (a >= 1) return v.toPrecision(3);
    return String(Number(v.toPrecision(3)));
  }
  function cellInfo(cell, row, col, chart) {
    if (cell == null) return { kind: "ph", status: "placeholder" };
    if (typeof cell === "string" && cell.indexOf("na:") === 0) return { kind: "na", why: cell.slice(3) };
    var o = typeof cell === "number" ? { v: cell } : cell;
    if (o.v == null) return { kind: "ph", status: "placeholder" };
    return { kind: "v", v: o.v, p90: o.p90, p99: o.p99, note: o.note, status: o.s || row.s || col.status || chart.status || "measured" };
  }
  function findChart(id) {
    if (!BENCH) return null;
    for (var i = 0; i < BENCH.suites.length; i++) {
      var s = BENCH.suites[i];
      for (var j = 0; j < s.charts.length; j++) if (s.charts[j].id === id) return s.charts[j];
    }
    return null;
  }
  function renderChart(chart) {
    var run = chart.run && BENCH.runs[chart.run];
    var infos = chart.rows.map(function (row) {
      return row.cells.map(function (c, i) { return cellInfo(c, row, chart.columns[i] || {}, chart); });
    });
    var all = [];
    infos.forEach(function (r) { r.forEach(function (c) { if (c.kind === "v" && c.v > 0) all.push(c.v); }); });
    var lo = all.length ? Math.min.apply(null, all) / 2 : 1, hi = all.length ? Math.max.apply(null, all) : 1;
    var statuses = {};
    infos.forEach(function (r) { r.forEach(function (c) { if (c.status) statuses[c.status] = true; }); });

    var h = '<div class="chart" id="' + esc(chart.id) + '">';
    h += '<div class="chart-head"><h4>' + esc(chart.title) + (chart.sub ? '<span class="sub">' + esc(chart.sub) + "</span>" : "") + "</h4>";
    h += '<div class="chart-tools">';
    ["measured", "earlier", "placeholder"].forEach(function (s) {
      if (statuses[s]) h += '<span class="tag ' + s + '">' + (s === "earlier" && run ? esc(run.label) : STATUS_LABEL[s]) + "</span>";
    });
    h += "</div></div>";
    h += '<div class="tb"><table class="tbars"><thead><tr><th scope="col"><span class="sr">Kind</span></th>';
    chart.columns.forEach(function (col) {
      h += '<th scope="col"' + (col.self ? ' class="self"' : "") + ">" + esc(col.name) + (col.ver ? "<small>" + esc(col.ver) + "</small>" : "") + "</th>";
    });
    h += "</tr></thead><tbody>";
    chart.rows.forEach(function (row, ri) {
      var r = infos[ri];
      var nums = r.filter(function (c) { return c.kind === "v"; }).map(function (c) { return c.v; });
      var rowMax = nums.length ? Math.max.apply(null, nums) : 1;
      var best = nums.length > 1 ? (chart.better === "higher" ? Math.max.apply(null, nums) : Math.min.apply(null, nums)) : null;
      var unit = row.unit != null ? row.unit : "";
      var digits = row.digits != null ? row.digits : chart.digits;
      h += '<tr><th scope="row">' + esc(row.label) + "</th>";
      r.forEach(function (c, ci) {
        var col = chart.columns[ci] || {};
        var self = col.self || row.self;
        var cls = "cell" + (self ? " self" : "");
        var who = esc(col.name + (col.ver ? " " + col.ver : ""));
        if (c.kind === "ph") {
          h += '<td><div class="' + cls + ' ph" tabindex="0" data-tip="<b>' + who + "</b> · " + esc(row.label) + '<br>Placeholder: no measurement yet. The laptop benchmark measures it."><span class="val">placeholder</span><span class="bar" style="width:46%"></span></div></td>';
          return;
        }
        if (c.kind === "na") {
          h += '<td><div class="' + cls + ' na" tabindex="0" data-tip="<b>' + who + "</b> · " + esc(row.label) + "<br>Not supported: " + esc(c.why) + '"><span class="val">' + esc(c.why) + "</span></div></td>";
          return;
        }
        var pct;
        if (chart.scale === "log") pct = (Math.log(c.v) - Math.log(lo)) / (Math.log(hi) - Math.log(lo));
        else if (chart.scale === "unit") pct = c.v;
        else pct = c.v / rowMax;
        pct = Math.max(0.02, Math.min(1, pct)) * 100;
        var val = fmt(c.v, digits) + (unit ? " " + unit : "");
        var tipText = "<b>" + who + "</b> · " + esc(row.label) + "<br>" + esc(val) + (chart.unit && !unit ? " " + esc(chart.unit) : "");
        if (c.p90 != null) tipText += " · p90 " + esc(fmt(c.p90)) + " · p99 " + esc(fmt(c.p99));
        if (c.note) tipText += "<br>" + esc(c.note);
        tipText += "<br>" + esc(c.status === "earlier" && run ? run.label : STATUS_LABEL[c.status] || "");
        h += '<td><div class="' + cls + '" tabindex="0" data-tip="' + esc(tipText) + '"><span class="val' + (c.v === best ? " best" : "") + '">' + esc(val) + (c.note ? " *" : "") + '</span><span class="bar" style="width:' + pct.toFixed(1) + '%"></span></div></td>';
      });
      h += "</tr>";
    });
    h += "</tbody></table></div>";
    h += '<div class="chart-foot"><span class="k"><i class="s"></i>Strata</span><span class="k"><i></i>others</span>';
    if (statuses.placeholder) h += '<span class="k"><i class="h"></i>placeholder, no value yet</span>';
    if (chart.scale === "row") h += "<span>bars scaled within each row</span>";
    if (chart.scale === "log") h += "<span>bars on a log scale</span>";
    h += "<span>bold: best in the row</span>";
    h += "</div>";
    if (chart.note || run) {
      h += '<div class="chart-foot">';
      if (chart.note) h += "<span>" + esc(chart.note) + "</span>";
      if (run) h += "<span>" + esc(run.machine) + ". Source: <code>" + esc(run.source) + "</code></span>";
      h += "</div>";
    }
    h += "</div>";
    return h;
  }
  function renderSuite(el) {
    var id = el.getAttribute("data-suite");
    var suite = BENCH && BENCH.suites.filter(function (s) { return s.id === id; })[0];
    if (!suite) return;
    var h = "";
    if (!el.hasAttribute("data-no-head")) {
      h += '<div class="suite-head"><span class="eyebrow">Part ' + esc(suite.part) + " · " + esc(suite.id.toUpperCase()) + "</span><h3>" + esc(suite.title) + '</h3><p class="muted">' + esc(suite.lede) + "</p>";
      h += '<p class="small muted"><b>Data:</b> ' + esc(suite.dataset) + "</p>";
      if (suite.plan) h += '<details><summary class="small">What the laptop benchmark measures</summary><ul class="plan">' + suite.plan.map(function (p) { return "<li>" + esc(p) + "</li>"; }).join("") + "</ul></details>";
      h += "</div>";
    }
    h += '<div class="grid">' + suite.charts.map(renderChart).join("") + "</div>";
    el.innerHTML = h;
  }
  function renderHeadlines(el) {
    if (!BENCH) return;
    var n = parseInt(el.getAttribute("data-headlines"), 10) || BENCH.headlines.length;
    el.innerHTML = BENCH.headlines.slice(0, n).map(function (s) {
      var run = s.run && BENCH.runs[s.run];
      var ph = s.status === "placeholder";
      return '<div class="stat' + (ph ? " ph" : "") + '"><span class="v">' + (ph ? "pending" : esc(s.value)) + "</span><p>" + esc(s.text) + '</p><span class="meta"><span class="tag ' + esc(s.status) + '">' + esc(s.status === "earlier" && run ? run.label : STATUS_LABEL[s.status]) + "</span>" + (run ? '<span data-tip="' + esc(run.machine) + '" tabindex="0">' + esc(run.machine.split(";")[0].split(":")[0]) + "</span>" : "<span>laptop benchmark</span>") + "</span></div>";
    }).join("");
  }
  function renderStages(el) {
    var st = BENCH && BENCH.stages;
    if (!st) return;
    var h = '<div class="table-wrap"><table class="data"><thead><tr><th>stage</th>' + st.metrics.map(function (m) { return "<th>" + esc(m) + "</th>"; }).join("") + "</tr></thead><tbody>";
    st.rows.forEach(function (r) {
      h += '<tr><th scope="row"><b>' + esc(r.stage) + '</b><br><span class="small muted">' + esc(r.detail) + "</span></th>";
      r.values.forEach(function (v) {
        h += v == null ? '<td><span class="tag placeholder">placeholder</span></td>' : '<td class="n">' + esc(v) + "</td>";
      });
      h += "</tr>";
    });
    h += "</tbody></table></div>";
    el.innerHTML = h;
  }

  /* ---------------------------------------------------------- matrix */
  var CMP = window.STRATA_COMPARE || null;
  var MARK_LABEL = { y: "Yes", p: "Partly, by extension or by hand", n: "No", $: "Paid tier only" };
  function parseMark(s) {
    if (!s) return { code: "n", verify: false, note: "" };
    var code = s.charAt(0), rest = s.slice(1), verify = false;
    if (rest.charAt(0) === "?") { verify = true; rest = rest.slice(1); }
    var note = rest.charAt(0) === ":" ? rest.slice(1) : "";
    return { code: code, verify: verify, note: note };
  }
  function renderMatrix(el) {
    if (!CMP) return;
    var state = { group: "all" };
    try { var saved = localStorage.getItem("strata-matrix-group"); if (saved) state.group = saved; } catch (e) {}
    var unverified = 0;
    CMP.sections.forEach(function (sec) { sec.rows.forEach(function (r) { Object.keys(r[1]).forEach(function (k) { if (parseMark(r[1][k]).verify) unverified++; }); }); });

    function draw() {
      var systems = CMP.systems.filter(function (s) { return state.group === "all" || s.group === "self" || s.group === state.group; });
      var h = '<div class="filters"><span class="label">Compare with</span>' + CMP.groups.map(function (g) {
        return '<button type="button" class="chipbtn" data-group="' + g.id + '" aria-pressed="' + (state.group === g.id) + '">' + esc(g.label) + "</button>";
      }).join("") + "</div>";
      h += '<div class="matrix-wrap"><table class="matrix"><thead><tr><th class="cap" scope="col">Capability</th>';
      systems.forEach(function (s) { h += '<th scope="col"' + (s.id === "strata" ? ' class="self"' : "") + ">" + esc(s.name) + (s.sub ? "<small>" + esc(s.sub) + "</small>" : "") + "</th>"; });
      h += "</tr></thead><tbody>";
      CMP.sections.forEach(function (sec) {
        h += '<tr class="grp"><td colspan="' + (systems.length + 1) + '">' + esc(sec.title) + "</td></tr>";
        sec.rows.forEach(function (r) {
          h += '<tr><td class="cap">' + esc(r[0]) + "</td>";
          systems.forEach(function (s) {
            var m = parseMark(r[1][s.id]);
            var cls = s.id === "strata" ? ' class="self"' : "";
            if (m.code === "t") { h += "<td" + cls + '><span class="small">' + esc(m.note) + "</span></td>"; return; }
            var code = m.code === "$" ? "paid" : m.code;
            var label = MARK_LABEL[m.code] || m.code;
            var tipText = "<b>" + esc(s.name) + "</b> · " + esc(r[0]) + "<br>" + esc(label) + (m.note ? ": " + esc(m.note) : "") + (m.verify ? "<br><i>Not yet checked against its current documentation</i>" : "");
            h += "<td" + cls + '><span class="mk ' + code + (m.verify ? " v" : "") + '" tabindex="0" role="img" aria-label="' + esc(label + (m.note ? ": " + m.note : "") + (m.verify ? " (to verify)" : "")) + '" data-tip="' + esc(tipText) + '"></span></td>';
          });
          h += "</tr>";
        });
      });
      h += "</tbody></table></div>";
      h += '<div class="legend"><span><span class="mk y"></span>built in</span><span><span class="mk p"></span>partly, by extension or by hand</span><span><span class="mk paid"></span>paid tier only</span><span><span class="mk n"></span>no</span><span><span class="mk y v"></span>' + unverified + " cells not yet checked against each project's documentation</span><span>Hover or focus a mark for the detail.</span></div>";
      el.innerHTML = h;
      $$(".chipbtn", el).forEach(function (b) {
        b.addEventListener("click", function () {
          state.group = b.getAttribute("data-group");
          try { localStorage.setItem("strata-matrix-group", state.group); } catch (e) {}
          draw();
        });
      });
    }
    draw();
  }

  /* ---------------------------------------------------------- boot */
  function boot() {
    $$("pre[data-lang]").forEach(wrapCode);
    $$("[data-tabs]").forEach(initTabs);
    $$("[data-suite]").forEach(renderSuite);
    $$("[data-chart]").forEach(function (el) { var c = findChart(el.getAttribute("data-chart")); if (c) el.innerHTML = renderChart(c); });
    $$("[data-headlines]").forEach(renderHeadlines);
    $$("[data-stages]").forEach(renderStages);
    $$("[data-matrix]").forEach(renderMatrix);
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot); else boot();
})();
