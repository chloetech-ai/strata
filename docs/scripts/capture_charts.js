// Screenshots the charts and tables the documentation shows from the long-form
// reports in docs/reports/, into docs/assets/img/. Run after copying freshly
// built reports into docs/reports/.
//
//   NODE_PATH=<dir with playwright> node docs/scripts/capture_charts.js
//
// Each entry names a report, the element to capture (the frame around a chart,
// or a table) and any clicks that select the view the docs describe.
const path = require("path");
const { chromium } = require("playwright");

const docs = path.resolve(__dirname, "..");
const img = process.env.OUT || path.join(docs, "assets", "img");

const SHOTS = [
  // Strata
  { report: "strata.html", out: "chart-strata-latency.png", frameOf: "#latency-chart" },
  { report: "strata.html", out: "chart-strata-fusion.png", frameOf: "#hybrid-sweep" },
  { report: "strata.html", out: "chart-strata-recall-latency.png", frameOf: "#vector-curves" },
  { report: "strata.html", out: "chart-strata-filtered-vectors.png", frameOf: "#filtered-chart" },
  { report: "strata.html", out: "chart-strata-wal.png", frameOf: "#wal-chart" },
  // Posting lists: the three new designs
  { report: "posting-list-designs.html", out: "chart-postings-and-clustered.png", frameOf: "#clustered-and" },
  { report: "posting-list-designs.html", out: "chart-postings-and-wikitext.png", frameOf: "#wikitext-and" },
  { report: "posting-list-designs.html", out: "chart-postings-or-wikitext.png", frameOf: "#wikitext-or" },
  // Strata graphs
  { report: "graph.html", out: "table-graph-speed.png", element: "#speed-tabs table" },
  { report: "graph.html", out: "table-graph-size.png", element: "#size-table table" },
  { report: "graph.html", out: "table-graph-quality.png", element: "#quality-table table" },
];

(async () => {
  const only = process.argv.slice(2);
  const browser = await chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 }, deviceScaleFactor: 2, colorScheme: "light" });
  let current = null;
  for (const s of SHOTS) {
    if (only.length && !only.includes(s.out)) continue;
    if (current !== s.report) {
      await page.goto("file://" + path.join(docs, "reports", s.report));
      await page.waitForTimeout(500);
      current = s.report;
    }
    for (const c of s.clicks || []) await page.click(c);
    const target = s.frameOf ? page.locator(s.frameOf).locator("xpath=ancestor::div[contains(@class,'frame')][1]") : page.locator(s.element).first();
    await target.scrollIntoViewIfNeeded();
    await page.waitForTimeout(200);
    await target.screenshot({ path: path.join(img, s.out) });
    console.log("wrote", s.out);
  }
  await browser.close();
})();
