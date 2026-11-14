// Dense views at scale (JOURNEYS.md, "Dense views" and "Scale rules"): the
// overwhelming case, one agent with a change of 38 files and 186 turns, and
// a project with 9 agents. Each view leads with one sentence and one small
// picture, groups and ranks, folds the noise, and shows the rest on a click.
// The pieces (blastCompact, churnBar, turnStrip, meter, testStrip) are what
// the rewrite would add to common.js; Review and Debug use them too.
import { h, useStyle, ctxChip, avatar } from "../../levels/common.js";
import { page, icon, btn } from "./ui.js";
import { A } from "./fixtures.js";

const SVG = "http://www.w3.org/2000/svg";
const BIG = { ...A.freeship, id: "c3e1a010", name: "checkout-v2", model: "sonnet", task: "Move checkout to a server-side order: per-line tax, an orders table, the page posts the order." };

// ------------------------------------------------------------ pieces

/** +/− as one short bar, exact numbers on hover (rule 5). */
export function churnBar(add, del, max = 600) {
  const w = (n) => Math.max(n ? 3 : 0, Math.round(Math.sqrt(n / max) * 64));
  // Exact numbers on hover on a desktop; on a phone, where nothing is hover-only, they are printed beside the bar.
  return h("span", { class: "jz-churn", title: `+${add} −${del} lines` }, h("i", { class: "a", style: { width: w(add) + "px" } }), h("i", { class: "d", style: { width: w(del) + "px" } }), h("span", { class: "jz-cn" }, `+${add} −${del}`));
}

const phone = () => matchMedia("(max-width: 640px)").matches;

/** The blast radius as a sentence and three rings (rule 2): changed functions, callers, public names. */
export function blastCompact({ verdict, tone = "warn", rings, exceptions, tests, actions, title = "Blast radius" }) {
  const size = 150, c = size / 2;
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", `0 0 ${size} ${size}`);
  svg.setAttribute("class", "jz-rings");
  const radii = [24, 46, 70];
  radii.slice().reverse().forEach((r, i) => {
    const k = radii.length - 1 - i;
    const circ = document.createElementNS(SVG, "circle");
    circ.setAttribute("cx", c); circ.setAttribute("cy", c); circ.setAttribute("r", r);
    circ.setAttribute("class", `r${k}`);
    svg.append(circ);
  });
  rings.forEach((ring, k) => {
    const t = document.createElementNS(SVG, "text");
    t.setAttribute("x", c); t.setAttribute("y", k === 0 ? c + 5 : c - radii[k] + 15);
    t.setAttribute("text-anchor", "middle");
    t.textContent = String(ring.n);
    svg.append(t);
  });
  const n = exceptions.length + (tests ? 1 : 0);
  // On a phone: the verdict, then Merge or Send back, then the rings beside their legend, then the exceptions folded.
  return h("section", { class: "jc jz-blast" },
    h("div", { class: `jz-verdict ${tone}` }, h("span", { class: "dot" }), h("span", {}, verdict)),
    actions ? h("div", { class: "jz-mact" }, actions()) : null,
    h("div", { class: "jz-bb" },
      svg,
      h("div", { class: "jz-rl" }, rings.map((ring, k) => h("div", { class: "jz-ring" },
        h("span", { class: `jz-sw r${k}` }), h("b", {}, `${ring.n} ${ring.label}`),
        h("span", { class: "jz-names" }, ring.names.slice(0, 3).map((n, i) => [i ? ", " : "", h(ring.code ? "code" : "span", { class: ring.code ? "lv-code" : null }, n)]), ring.names.length > 3 ? h("button", { class: "lv-link", type: "button" }, ` +${ring.names.length - 3} more`) : null)))),
      h("details", { class: "jz-exc", open: !phone() || null },
        h("summary", { class: "jz-h" }, `Exceptions`, h("span", { class: "jz-xn" }, String(n)), icon("down", "ji sm")),
        exceptions.map((e) => h("div", { class: `jz-e ${e.tone}` }, h("span", { class: "dot" }), h("span", {}, e.text))),
        tests ? h("div", { class: "jz-e ok" }, h("span", { class: "dot" }), h("span", {}, tests)) : null)));
}

/** A session as a strip: one tick per turn, coloured by kind, split into named phases (rule 8). */
export function turnStrip(turns, phases, marks = [], notes = {}) {
  // A tap (or a click) names the turn under the strip: nothing about a turn is hover-only.
  const cap = h("div", { class: "jz-cap" }, "Tap a tick to see its turn.");
  const say = (i, k) => { cap.replaceChildren(h("b", {}, `Turn ${i + 1}`), ` · ${k}`, notes[i + 1] ? ` · ${notes[i + 1]}` : "", " ", h("a", { class: "lv-link", href: "#/debug/c3e1a004/2" }, "Open")); };
  // On a phone the phases and ticks scroll sideways inside their own box; the page itself never does.
  return h("div", { class: "jz-strip" },
    h("div", { class: "jz-sx" }, h("div", { class: "jz-sxi" },
      h("div", { class: "jz-ph" }, phases.map((p) => h("span", { style: { flex: p.to - p.from + 1 } }, p.name))),
      h("div", { class: "jz-ticks" }, turns.map((k, i) => h("i", { class: `k-${k}${marks.includes(i + 1) ? " mark" : ""}`, title: `turn ${i + 1}: ${k}`, onclick: () => say(i, k) }))))),
    cap,
    h("div", { class: "jz-legend" }, [["read", "read"], ["edit", "edit"], ["run", "ran tests"], ["fail", "failed"], ["you", "you"], ["ctx", "memory"]].map(([k, l]) => h("span", {}, h("i", { class: `k-${k}` }), l))));
}

/** "41% full" (rule 11). */
export function meter(pct, label) {
  return h("div", { class: "jz-meter" }, h("div", { class: "jz-mbar" }, h("b", { style: { width: pct + "%" } })), h("span", {}, h("b", {}, `${pct}% full`), label ? ` · ${label}` : null));
}

/** The last runs as a strip of squares. */
export function testStrip(runs) {
  return h("span", { class: "jz-ts" }, runs.split("").map((r) => h("i", { class: r === "p" ? "p" : r === "f" ? "f" : "s" })));
}

function piece({ title, files, add, del, risk, words, hunk, open }) {
  return h("details", { class: `jz-piece ${risk}`, open: open || null },
    h("summary", {},
      h("span", { class: `jz-risk ${risk}` }, risk),
      h("b", {}, title),
      h("span", { class: "jnote" }, files),
      h("span", { class: "sep" }),
      churnBar(add, del)),
    h("p", { class: "jz-words" }, words),
    hunk ? h("div", { class: "jz-hunk" }, h("div", { class: "jz-hh" }, h("span", { class: "jtag" }, "the hunk that matters"), h("code", {}, hunk.where)),
      hunk.lines.map(([s, t]) => h("div", { class: `ja-dl ${s === "+" ? "add" : s === "-" ? "del" : ""}` }, h("span", { class: "ja-ln" }), h("span", { class: "ja-sg" }, s), h("code", {}, t)))) : null,
    h("button", { class: "lv-link", type: "button" }, "All hunks of this piece"));
}

// ------------------------------------------------------------ the data
const PHASES = [{ name: "Explore", from: 1, to: 22 }, { name: "Orders table", from: 23, to: 61 }, { name: "Totals per line", from: 62, to: 118 }, { name: "The page", from: 119, to: 160 }, { name: "Tests and fixes", from: 161, to: 186 }];
const TURNS = (() => {
  const out = [];
  let x = 7;
  const rnd = () => ((x = (x * 1103515245 + 12345) % 2147483648) / 2147483648);
  for (let i = 1; i <= 186; i++) {
    const ph = PHASES.find((p) => i >= p.from && i <= p.to).name;
    const r = rnd();
    let k = ph === "Explore" ? (r < .7 ? "read" : r < .85 ? "ctx" : "run") : ph === "Tests and fixes" ? (r < .45 ? "run" : r < .75 ? "edit" : "read") : (r < .5 ? "edit" : r < .72 ? "read" : r < .92 ? "run" : "ctx");
    if ([14, 52].includes(i)) k = "you";
    if ([47, 48, 163, 164].includes(i)) k = "fail";
    out.push(k);
  }
  return out;
})();
const STANDOUT = [
  [14, "you", "Asked you: tax per line or on the total? You said per line."],
  [47, "fail", "The orders migration failed twice (column type); fixed at 49."],
  [88, "edit", "Changed total(): per-line tax. 9 callers reached."],
  [121, "ctx", "Missed “Shipping is not taxed” (0.58, under 0.60)."],
  [163, "fail", "2 tests failed after turn 88: test_receipt_total, test_ledger_export."],
  [180, "run", "Rebased on cents; 214 pass, 2 fail."],
];
for (const [n, k] of STANDOUT) TURNS[n - 1] = k; // a marked tick has the colour of its row in the list
const NOTES = Object.fromEntries(STANDOUT.map(([n, , t]) => [n, t]));
const shipActs = () => [btn("Merge into main", { primary: true, iconName: "branch" }), btn("Send back with a note")];

// ------------------------------------------------------------ pages
const crumbs = (ctx, label) => ctx.crumbs([{ label: "Home", href: "#/" }, { label: "shop", href: "#/p/shop" }, { label: "At scale", href: "#/dense" }, { label, href: location.hash }]);
const tabs = (now) => h("div", { class: "jd-ways" }, h("span", { class: "jnote" }, "checkout-v2: 38 files, 186 turns"),
  [["#/dense/ship", "Ship", "ship"], ["#/dense/diff", "Diff", "diff"], ["#/dense/session", "Session", "session"], ["#/dense/tests", "Tests", "tests"], ["#/dense/continue", "Continue, 9 agents", "continue"], ["#/dense/rules", "Scale rules", "rules"]].map(([href, l, k]) => h("a", { href, class: k === now ? "on" : null }, l)));

export const BIG_BLAST = {
  verdict: "Medium risk: changes how every cart total is computed; 2 changed functions have no test.",
  rings: [
    { n: 6, label: "functions changed", names: ["total()", "line_tax()", "Order.create()", "post_checkout()", "to_cents()", "Cart.lines"], code: true },
    { n: 23, label: "callers", names: ["receipt", "ledger export", "coupons", "admin report", "order email"] },
    { n: 4, label: "public names", names: ["POST /checkout", "the total on the page", "the receipt email", "ledger.csv"] },
  ],
  exceptions: [
    { tone: "crit", text: "Untested: line_tax(), Order.create()" },
    { tone: "warn", text: "freeship also changes cart.py (total())" },
    { tone: "ctx", text: "Replaces “Round the order total once, at the end” (Oct 21)" },
  ],
  tests: "214 pass · 2 fail: test_receipt_total, test_ledger_export",
};

export function denseShip(root, ctx) {
  crumbs(ctx, "Ship");
  root.append(page({ title: "Review checkout-v2", sub: "The verdict first, then what the change reaches, then only the exceptions." },
    tabs("ship"),
    blastCompact({ ...BIG_BLAST, actions: shipActs }),
    h("div", { class: "jgrid2", style: { marginTop: "14px" } },
      h("section", { class: "jc" }, h("h2", {}, "The pieces of work", h("span", { class: "n" }, "5, riskiest first")),
        h("div", { class: "jlist" }, [["Totals per line on the server", "high", 212, 96, "3 files"], ["Orders table and migration", "medium", 180, 0, "2 files"], ["The checkout page posts the order", "medium", 240, 130, "4 files"], ["Tests", "low", 410, 40, "9 files"]].map(([t, r, a, d, f]) =>
          h("div", { class: "jrow" }, h("span", { class: `jz-risk ${r}` }, r), h("span", { style: { flex: 1 } }, t), h("span", { class: "jnote" }, f), churnBar(a, d))),
          h("div", { class: "jrow muted" }, icon("chev", "ji sm"), "Folded: renames and imports, 14 files; package-lock.json; 3 formatting-only hunks")),
        h("a", { class: "lv-link", href: "#/dense/diff" }, "Open the diff by piece →")),
      h("aside", { class: "jc" }, h("h2", {}, "Decisions behind it", h("span", { class: "n" }, "2 that matter, 9 routine folded")),
        h("div", { class: "ju-k" }, ctxChip("replaces"), " Tax per line in integer cents, rounded half up (you, turn 14)", h("div", { class: "jnote" }, "replaces “Round the order total once, at the end”")),
        h("div", { class: "ju-k" }, ctxChip("new rule"), " Orders are immutable once paid (you, turn 52)"),
        h("button", { class: "lv-link", type: "button" }, "9 routine decisions"))),
    h("div", { class: "jr-act jz-act jm-hide" }, btn("Send back with a note", { sm: true }), h("button", { class: "lv-link", type: "button" }, "Open a pull request instead"), btn("Merge into main", { primary: true, iconName: "branch" }))));
}

export function denseDiff(root, ctx) {
  crumbs(ctx, "Diff");
  root.append(page({ title: "What checkout-v2 changed", sub: "38 files, grouped into the 5 pieces of work it did, riskiest first. Each piece says what it does and shows the one hunk that matters." },
    tabs("diff"),
    h("div", { class: "jz-sum jc" }, h("b", {}, "Totals are now computed per line on the server; the page posts an order instead of computing it."), h("span", { class: "sep" }), churnBar(1240, 610, 1400), h("span", { class: "jnote" }, "38 files")),
    piece({ title: "Totals per line on the server", files: "cart.py, money.py, orders/total.py", add: 212, del: 96, risk: "high", open: true,
      words: "total() sums each line's tax rounded half up, instead of taxing the subtotal once; 9 callers, 2 of them untested.",
      hunk: { where: "cart.py · total()", lines: [[" ", "    def total(self) -> int:"], ["-", "        return self.subtotal() + round(self.subtotal() * TAX_BP / 10_000)"], ["+", "        return sum(l.cents + line_tax(l) for l in self.lines)"]] } }),
    piece({ title: "Orders table and migration", files: "migrations/003_orders.sql, orders/model.py", add: 180, del: 0, risk: "medium", words: "A new orders table; Order.create() writes one row per checkout. Untested." }),
    piece({ title: "The checkout page posts the order", files: "web/checkout.js, web/index.html, +2", add: 240, del: 130, risk: "medium", words: "The page no longer adds up the total; it posts the cart and shows what the server answers." }),
    piece({ title: "Tests", files: "9 files", add: 410, del: 40, risk: "low", words: "31 new tests; 214 pass, 2 fail (test_receipt_total, test_ledger_export)." }),
    h("details", { class: "jz-fold" }, h("summary", {}, icon("chev", "ji sm"), h("b", {}, "Mechanical, skipped: 19 files"), h("span", { class: "jnote" }, "rename Cart.items → Cart.lines (14 files), imports, package-lock.json, 3 formatting-only hunks"), h("span", { class: "sep" }), churnBar(198, 344)))));
}

export function denseSession(root, ctx) {
  crumbs(ctx, "Session");
  root.append(page({ title: "checkout-v2's session", sub: "186 turns in 5 phases. Only the turns that stand out are listed; any tick opens its turn." },
    tabs("session"),
    h("section", { class: "jc" }, h("div", { class: "jrow", style: { marginBottom: "10px" } }, avatar(BIG, 22), h("b", {}, "checkout-v2"), h("span", { class: "jnote" }, "3 h 10 min · sonnet · 186 turns"), h("span", { class: "sep", style: { flex: 1 } }), meter(41, "context now")),
      turnStrip(TURNS, PHASES, STANDOUT.map((s) => s[0]), NOTES)),
    h("div", { class: "jgrid2", style: { marginTop: "14px" } },
      h("section", { class: "jc" }, h("h2", {}, "Turns that stand out", h("span", { class: "n" }, `${STANDOUT.length} of 186`)),
        h("div", { class: "jlist" }, STANDOUT.map(([n, k, t]) => h("div", { class: "jrow" }, h("i", { class: `jz-dotk k-${k}` }), h("b", { class: "mono", style: { width: "42px" } }, `#${n}`), h("span", { style: { flex: 1 } }, t), h("a", { class: "lv-link", href: "#/debug/c3e1a004/2" }, "Open")))),
        h("button", { class: "lv-link", type: "button" }, "All 186 turns")),
      h("aside", { class: "jc" }, h("h2", {}, "Memory it was given"),
        h("div", { class: "jz-kinds" }, [["decisions", 14], ["rules", 2], ["facts", 9], ["fixes", 3]].map(([k, n]) => h("span", { class: "jchip ctx" }, `${n} ${k}`))),
        h("div", { class: "jz-mrec" }, h("div", { class: "jnote" }, "Top record (used 31 times)"), h("div", {}, ctxChip("decision"), " Money is integer cents everywhere.")),
        h("div", { class: "jz-mrec miss" }, h("div", { class: "jnote" }, "Missed at turn 121"), h("div", {}, ctxChip("decision"), " Shipping is not taxed."), h("div", { class: "jnote" }, "scored 0.58, under 0.60")),
        h("button", { class: "lv-link", type: "button" }, "All 28 records")))));
}

export function denseTests(root, ctx) {
  crumbs(ctx, "Tests");
  const row = (state, name, runs, note) => h("div", { class: "jz-trow" }, h("span", { class: `jchip ${state === "fail" ? "crit" : state === "flaky" ? "warn" : ""}` }, state), h("code", { class: "lv-code" }, name), testStrip(runs), h("span", { class: "jnote" }, note));
  root.append(page({ title: "Tests in shop", sub: "One line for what passes; a row only for what fails, flickers or has no test." },
    tabs("tests"),
    h("section", { class: "jc jz-tline" }, h("span", { class: "jz-big ok" }, "214 pass"), h("span", { class: "jz-big crit" }, "2 fail"), h("span", { class: "jz-big warn" }, "1 flaky"), h("span", { class: "jz-big" }, "2 untested"),
      h("span", { class: "sep" }), h("span", { class: "jnote" }, "last 20 runs"), testStrip("ppppppppppppppfpppff"), h("span", { class: "jnote" }, "+31 tests since yesterday")),
    h("section", { class: "jc", style: { marginTop: "14px" } },
      h("h2", {}, "Needs a look", h("span", { class: "n" }, "5")),
      h("div", { class: "jlist" },
        row("fail", "test_receipt_total", "ppppppppppppppppppff", "fails since turn 88 of checkout-v2 (total())"),
        row("fail", "test_ledger_export", "ppppppppppppppppppff", "fails since turn 88 of checkout-v2"),
        row("flaky", "test_promo_window", "ppfppppppfpppppppppp", "2 of 20 on one commit; fixed on wb/flakehunt"),
        row("untested", "line_tax()", "", "changed by checkout-v2; no test names it"),
        row("untested", "Order.create()", "", "new; no test names it")),
      h("button", { class: "lv-link", type: "button" }, "214 passing tests"))));
}

export function denseContinue(root, ctx) {
  crumbs(ctx, "Continue");
  const one = (name, line, k = "acc", st = "Working") => h("div", { class: "jrow jz-one" }, avatar({ name, id: name }, 20), h("b", {}, name), h("span", { class: `jstate ${k}` }, st), h("span", { class: "jz-ol" }, line));
  root.append(page({ title: "Continue shop", sub: "Since you left at 4:10 PM, 15 h ago: 3 agents finished, 2 wait on you, 4 are working. 186 things happened; these are the ones that matter." },
    tabs("continue"),
    h("div", { class: "jgrid2" },
      h("section", { class: "jstack" },
        h("h2", { class: "ju-h" }, "Needs you (2)"),
        h("div", { class: "jc jk-a needs" }, h("div", { class: "jrow" }, avatar(BIG, 22), h("b", {}, "checkout-v2"), h("span", { class: "jstate warn" }, "Waiting for you · 41 min")),
          h("div", {}, "Run the migration on the dev database?"), h("pre", { class: "jq-cmd compact" }, "python3 -m orders.migrate --db dev.sqlite3 003_orders"),
          h("div", { class: "jrow" }, btn("Allow once", { primary: true, sm: true, kbd: "1" }), btn("Deny", { sm: true, kbd: "3" }))),
        h("div", { class: "jc jk-a needs" }, h("div", { class: "jrow" }, avatar(A.paybutton, 22), h("b", {}, "paybutton"), h("span", { class: "jstate warn" }, "Waiting for you · 9 min")),
          h("div", {}, "Check the page's script in node?"), h("div", { class: "jrow" }, btn("Allow once", { primary: true, sm: true, kbd: "1" }), btn("Deny", { sm: true, kbd: "3" }), h("button", { class: "lv-link", type: "button" }, "Show the command (6 lines)"))),
        h("h2", { class: "ju-h" }, "Working (4)"),
        h("div", { class: "jc" }, one("freeship", "the shipping line; 2 of 3 new tests pass"), one("search", "indexing product names"), one("a11y", "labels on the checkout form"), one("docs", "the README's run section")),
        h("details", { class: "jz-fold" }, h("summary", {}, icon("chev", "ji sm"), h("b", {}, "Finished (3)"), h("span", { class: "jnote" }, "cents and flakehunt are ready to review; explain"), h("span", { class: "sep" }), btn("Review 2", { sm: true, href: "#/p/shop/ship" })))),
      h("aside", { class: "jstack" },
        h("section", { class: "jc" }, h("h2", {}, "What changed", h("span", { class: "n" }, "by piece of work")),
          [["checkout-v2", "Totals per line on the server", 1240, 610], ["cents", "Customer totals round half up", 18, 4], ["freeship", "The shipping line", 31, 2], ["flakehunt", "test_promo_window is stable", 6, 4]].map(([a, t, x, d]) => h("div", { class: "jrow ju-k" }, h("b", { style: { width: "92px" } }, a), h("span", { style: { flex: 1 } }, t), churnBar(x, d, 1400))),
          h("button", { class: "lv-link", type: "button" }, "and 3 smaller pieces")),
        h("section", { class: "jc" }, h("h2", {}, "Decisions", h("span", { class: "n" }, "2 change something, 31 routine")),
          h("div", { class: "ju-k" }, ctxChip("replaces"), " Tax per line, rounded half up (you)"), h("div", { class: "ju-k" }, ctxChip("new rule"), " Orders are immutable once paid (you)"),
          h("button", { class: "lv-link", type: "button" }, "31 routine decisions")),
        h("section", { class: "jc" }, h("h2", {}, "Time waiting on you"), h("div", { class: "jz-wait" }, h("b", { style: { width: "18%" } })), h("div", { class: "jnote" }, "18% of the agents' time overnight; 12 of 14 waits were python3 commands you always allow. ", h("button", { class: "lv-link", type: "button" }, "Allow them")))))));
}

export const SCALE_RULES = [
  ["Test every screen against the overwhelming case", "a change of 38 files and 186 turns, a project with 9 agents"],
  ["Lead with one sentence and one small picture", "the sentence answers the page's question; the picture is a ring, a bar, a strip or a meter"],
  ["Ship opens with a one-line verdict", "then rings: functions, callers, public names (user-facing names only); then only the exceptions"],
  ["Debug shows the blast radius of the bad turn", "“changed total(), 9 callers reached, 2 tests caught it, 0 reached main”"],
  ["Diffs are grouped into pieces of work, riskiest first", "mechanical changes (renames, imports, lockfiles, formatting) folded and skipped; each piece shows its one hunk that matters"],
  ["+/− are short bars", "exact numbers on hover; one total per piece"],
  ["Tests read “214 pass” on one line", "a row only for failing, flaky or untested"],
  ["Agents in three groups", "Needs you (open, with buttons), Working (one line each), Finished (folded)"],
  ["A session is a strip", "one tick per turn, coloured by kind, split into named phases; the list shows only the turns that stand out (6 of 186)"],
  ["Memory given: counts by kind", "plus the record it missed and the top record; the rest one click away"],
  ["Decisions: only those that change an earlier one or add a rule", "routine ones fold into a count"],
  ["Tokens as a meter", "“41% full”"],
  ["Top 3, then “N more”", "and deltas (+31 tests since yesterday), not totals"],
];

export function denseRules(root, ctx) {
  crumbs(ctx, "Scale rules");
  root.append(page({ title: "Scale rules", sub: "What every dense view follows. The same list is in JOURNEYS.md, “Dense views”." },
    tabs("rules"),
    h("section", { class: "jc" }, h("div", { class: "jlist" }, SCALE_RULES.map(([r, d], i) => h("div", { class: "jrow", style: { alignItems: "baseline" } }, h("span", { class: "jr-ord" }, String(i + 1)), h("b", { style: { width: "420px" } }, r), h("span", { class: "muted" }, d)))))));
}

useStyle("j-dense", `
.jz-churn{display:inline-flex;align-items:center;gap:1px;height:8px}
.jz-churn i{display:block;height:8px;border-radius:2px}.jz-churn .a{background:var(--good)}.jz-churn .d{background:var(--crit)}
.jz-blast{display:flex;flex-direction:column;gap:12px}
.jz-verdict{display:flex;align-items:center;gap:10px;font-size:16px;font-weight:650}
.jz-verdict .dot{width:10px;height:10px;border-radius:50%;background:var(--warn);flex:none}
.jz-verdict.good .dot{background:var(--good)}.jz-verdict.crit .dot{background:var(--crit)}
.jz-bb{display:grid;grid-template-columns:150px minmax(0,1fr) minmax(0,.9fr);gap:20px;align-items:center}
.jz-rings{width:150px;height:150px}
.jz-rings circle{stroke:var(--surface);stroke-width:2}
.jz-rings .r0{fill:var(--accent)}.jz-rings .r1{fill:color-mix(in srgb,var(--accent) 35%,var(--surface))}.jz-rings .r2{fill:color-mix(in srgb,var(--accent) 14%,var(--surface))}
.jz-rings text{font:700 12px var(--sans);fill:var(--text)}
.jz-rings text:first-of-type{fill:var(--accent-ink)}
.jz-rl{display:flex;flex-direction:column;gap:9px}
.jz-ring{display:grid;grid-template-columns:12px auto;gap:2px 8px;align-items:center;font-size:13px}
.jz-names{grid-column:2;color:var(--text-2)}
.jz-sw{width:12px;height:12px;border-radius:50%}
.jz-sw.r0{background:var(--accent)}.jz-sw.r1{background:color-mix(in srgb,var(--accent) 35%,var(--surface))}.jz-sw.r2{background:color-mix(in srgb,var(--accent) 14%,var(--surface));border:1px solid var(--line-2)}
.jz-exc{display:flex;flex-direction:column;gap:6px;border-left:1px solid var(--line);padding-left:18px}
.jz-h{font-size:11.5px;font-weight:700;color:var(--muted);text-transform:uppercase;letter-spacing:.06em}
.jz-e{display:flex;gap:8px;align-items:center;font-size:13px}
.jz-e .dot{width:8px;height:8px;border-radius:50%;flex:none;background:var(--muted)}
.jz-e.crit .dot{background:var(--crit)}.jz-e.warn .dot{background:var(--warn)}.jz-e.ctx .dot{background:var(--ctx);border-radius:2px;transform:rotate(45deg)}.jz-e.ok .dot{background:var(--good)}
.jz-risk{font:700 10.5px var(--sans);text-transform:uppercase;letter-spacing:.05em;border-radius:5px;padding:2px 6px;background:var(--surface-3);color:var(--text-2)}
.jz-risk.high{background:var(--crit-soft);color:var(--crit-ink)}.jz-risk.medium{background:var(--warn-soft);color:var(--warn-ink)}.jz-risk.low{background:var(--good-soft);color:var(--good-ink)}
.jr-act.jz-act{flex-direction:row;justify-content:flex-end;align-items:center;margin-top:16px;height:auto}
.jz-sum{display:flex;align-items:center;gap:12px;margin-bottom:12px}.jz-sum .sep{flex:1}
.jz-piece{background:var(--surface);border:1px solid var(--line);border-radius:12px;margin-bottom:10px;overflow:hidden}
.jz-piece summary{display:flex;align-items:center;gap:10px;padding:11px 14px;cursor:pointer;list-style:none}
.jz-piece summary .sep{flex:1}
.jz-piece.high{border-left:3px solid var(--crit)}.jz-piece.medium{border-left:3px solid var(--warn)}
.jz-words{margin:0;padding:0 14px 10px;color:var(--text-2)}
.jz-piece .lv-link{margin:0 14px 12px}
.jz-hunk{margin:0 14px 10px;border:1px solid var(--line);border-radius:8px;overflow:hidden}
.jz-hh{display:flex;gap:8px;align-items:center;padding:6px 10px;background:var(--surface-2);font-size:12px}
.jz-fold{border:1px dashed var(--line-2);border-radius:12px;background:var(--surface-2)}
.jz-fold summary{display:flex;align-items:center;gap:10px;padding:10px 14px;cursor:pointer;list-style:none}
.jz-fold summary .sep{flex:1}
.jz-strip{display:flex;flex-direction:column;gap:4px}
.jz-ph{display:flex;gap:2px;font-size:11.5px;font-weight:650;color:var(--text-2)}
.jz-ph span{border-bottom:2px solid var(--line-2);padding-bottom:3px}
.jz-ticks{display:flex;gap:1px;height:34px;align-items:stretch}
.jz-ticks i{flex:1;border-radius:1px;opacity:.85}
.jz-ticks i.mark{outline:2px solid var(--text);outline-offset:1px;opacity:1;z-index:1}
.k-read{background:var(--lane-1)}.k-edit{background:var(--good)}.k-run{background:var(--warn)}.k-fail{background:var(--crit)}.k-you{background:var(--text)}.k-ctx{background:var(--ctx)}
.jz-legend{display:flex;gap:14px;font-size:11.5px;color:var(--muted);margin-top:4px}
.jz-legend span{display:inline-flex;align-items:center;gap:5px}.jz-legend i{width:9px;height:9px;border-radius:2px;display:inline-block}
.jz-dotk{width:10px;height:10px;border-radius:3px;display:inline-block;flex:none}
.jz-meter{display:flex;align-items:center;gap:8px;font-size:12.5px;color:var(--text-2)}
.jz-mbar{width:110px;height:8px;border-radius:99px;background:var(--surface-3);overflow:hidden}.jz-mbar b{display:block;height:100%;background:var(--ctx)}
.jz-kinds{display:flex;flex-wrap:wrap;gap:6px;margin-bottom:10px}
.jz-mrec{padding:8px 10px;border:1px solid var(--line);border-radius:9px;margin-bottom:8px;font-size:13px}
.jz-mrec.miss{background:var(--warn-soft);border-color:var(--warn-line)}
.jz-ts{display:inline-flex;gap:2px}.jz-ts i{width:7px;height:12px;border-radius:2px;background:var(--good)}.jz-ts i.f{background:var(--crit)}.jz-ts i.s{background:var(--surface-3)}
.jz-tline{display:flex;align-items:center;gap:16px}.jz-tline .sep{flex:1}
.jz-big{font-size:20px;font-weight:700;color:var(--text-2)}.jz-big.ok{color:var(--good-ink)}.jz-big.crit{color:var(--crit-ink)}.jz-big.warn{color:var(--warn-ink)}
.jz-trow{display:grid;grid-template-columns:80px 240px 160px 1fr;gap:12px;align-items:center}
.jz-one{padding:6px 0;gap:8px}.jz-ol{color:var(--text-2);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.jz-wait{height:10px;border-radius:99px;background:var(--surface-3);overflow:hidden;margin-bottom:6px}.jz-wait b{display:block;height:100%;background:var(--warn)}
`);
