// The level screens' stand-in backend: serves ui/ and answers /api/* from
// the fixture modules in ./fixtures/ (each exports `routes`: a list of
// [method, RegExp over the path, (match, url, body) => json | [status, json]];
// and, for routes that are not JSON, `raw`: [method, RegExp, (match, url,
// req, res) => void], tried first).
// Node 22, standard library only.
//
//   node chloedb-workbench/ui/dev/levels-mock.mjs [--port 7790]
//   then open http://127.0.0.1:7790/dev/levels.html?screen=mission
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const UI = path.resolve(here, "..");
const i = process.argv.indexOf("--port");
const PORT = Number(i > 0 ? process.argv[i + 1] : 7790);
const routes = [];
const raw = [];
for (const f of fs.readdirSync(path.join(here, "fixtures")).filter((f) => f.endsWith(".mjs")).sort()) {
  const m = await import(pathToFileURL(path.join(here, "fixtures", f)));
  routes.push(...(m.routes || []));
  raw.push(...(m.raw || []));
}
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".css": "text/css", ".png": "image/png", ".svg": "image/svg+xml", ".json": "application/json" };
http.createServer(async (req, res) => {
  const url = new URL(req.url, "http://x");
  for (const [method, re, fn] of raw) {
    const m = method === req.method && url.pathname.match(re);
    if (m) return fn(m, url, req, res);
  }
  if (url.pathname.startsWith("/api/")) {
    let body = "";
    for await (const c of req) body += c;
    let json = null;
    try { json = body ? JSON.parse(body) : null; } catch {}
    for (const [method, re, fn] of routes) {
      const m = method === req.method && url.pathname.match(re);
      if (!m) continue;
      let out = await fn(m, url, json);
      let status = 200;
      if (Array.isArray(out) && typeof out[0] === "number") [status, out] = out;
      res.writeHead(status, { "content-type": "application/json" });
      return res.end(JSON.stringify(out));
    }
    res.writeHead(404, { "content-type": "application/json" });
    return res.end(JSON.stringify({ error: `no fixture for ${req.method} ${url.pathname}`, error_code: "not_found" }));
  }
  const file = path.join(UI, path.normalize(url.pathname === "/" ? "/dev/levels.html" : url.pathname));
  if (!file.startsWith(UI) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) { res.writeHead(404); return res.end("not found"); }
  res.writeHead(200, { "content-type": TYPES[path.extname(file)] || "application/octet-stream" });
  fs.createReadStream(file).pipe(res);
}).listen(PORT, "127.0.0.1", () => console.log(`levels mock: http://127.0.0.1:${PORT}/dev/levels.html?screen=mission`));
