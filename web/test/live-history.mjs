// Stored chart history in the real app (jsdom) against a running web server:
// the range bar, charts drawn from `GET /history`, and the hover readout.
// The server must have been collecting for ~30 s (it samples every 10 s).
//
//   node test/live-history.mjs [base]      (default http://127.0.0.1:8799)

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");
const conn = (await (await fetch(base + "/connections")).json())[0];

const results = [];
const check = (name, ok, detail = "") => results.push([!!ok, name, detail]);

const dom = new JSDOM(`<!doctype html><html><body><div id="app"></div></body></html>`, {
  url: `${base}/${encodeURIComponent(conn.name)}`,
  runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: new VirtualConsole(),
});
const w = dom.window;
const d = w.document;
const requests = [];
w.fetch = (i, init) => {
  const u = new URL(typeof i === "string" ? i : i.url, base).toString();
  requests.push(u);
  return fetch(u, init);
};
w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
w.localStorage.removeItem("virt-man.range");
w.eval(code);
const wait = (ms) => new Promise((r) => w.setTimeout(r, ms));
const until = async (f, ms = 8000) => {
  for (let t = 0; t < ms; t += 100) { if (f()) return true; await wait(100); }
  return false;
};
const cpuChart = () => d.querySelector("#dash .metric svg.spark");
const values = (svg) => (svg?.dataset.v ?? "").split(",");

await until(() => cpuChart());
const rangeSel = () => d.querySelector("#dash .range-sel");
const pick = (key) => { rangeSel().value = key; rangeSel().dispatchEvent(new w.Event("change", { bubbles: true })); };
check("every chart has a range dropdown, Live selected",
  rangeSel()?.value === "live" && rangeSel().options.length === 6);
check("live charts span the last minute", values(cpuChart()).length === 60);
check("live mode reads no history", !requests.some((u) => u.includes("/history")));

pick("1h");
check("choosing 1h redraws from stored history",
  await until(() => rangeSel()?.value === "1h" && values(cpuChart()).length > 60),
  `${values(cpuChart()).length} points`);
check("…with the axis labelled -1h", d.querySelector("#dash .chart-axis")?.textContent.includes("-1h"));
check("…read from /history for this server", requests.some((u) => u.includes("/history") && u.includes("kind=host") && u.includes(`conn=${conn.id}`)));
const v = values(cpuChart());
check("the stored hour has samples", v.some((x) => x !== ""), `${v.filter((x) => x !== "").length} non-empty of ${v.length}`);
check("the choice survives the 1s poll", await wait(2500).then(() => rangeSel()?.value === "1h" && values(cpuChart()).length > 60));
check("…and is remembered", w.localStorage.getItem("virt-man.range") === "1h");

// Hover: jsdom has no layout, so give the chart a box, then point at the
// newest stored sample.
const svg = cpuChart();
const box = { left: 100, top: 200, width: 240, height: 74, right: 340, bottom: 274, x: 100, y: 200, toJSON() {} };
svg.getBoundingClientRect = () => box;
const vals = values(svg);
const last = vals.map((x, i) => [x, i]).filter(([x]) => x !== "").at(-1)[1];
const clientX = box.left + (last / (vals.length - 1)) * box.width;
svg.dispatchEvent(new w.PointerEvent("pointermove", { bubbles: true, clientX, clientY: 230 }));
const tip = d.querySelector(".spark-tip");
const expect = `${Number(vals[last]).toFixed(Number(vals[last]) >= 10 ? 0 : 1)}%`;
check("hovering a point shows its time and value", tip && /^\d\d:\d\d(:\d\d)? · /.test(tip.textContent) && tip.textContent.endsWith(expect),
  `${tip?.textContent} (expected …${expect})`);
check("…with a marker on that point", Math.abs(parseFloat(d.querySelector(".spark-mark")?.style.left) - clientX) < 0.01);
// A gap: the 14-day range starts before this server ever sampled (a full
// hour may have no gap at all once collection is an hour old).
pick("14d");
await until(() => rangeSel()?.value === "14d" && cpuChart()?.dataset.v);
const long = cpuChart();
long.getBoundingClientRect = () => box;
const lv = values(long);
const gap = lv.findIndex((x) => x === "");
check("the 14-day range has gaps where nothing was sampled", gap >= 0 && lv.some((x) => x !== ""));
long.dispatchEvent(new w.PointerEvent("pointermove", { bubbles: true, clientX: box.left + (gap / (lv.length - 1)) * box.width, clientY: 230 }));
check("a gap reads as no data, not zero", d.querySelector(".spark-tip")?.textContent.endsWith("no data"), d.querySelector(".spark-tip")?.textContent);
check("…and a long range names the date", /^\d+\/\d+ /.test(d.querySelector(".spark-tip")?.textContent ?? ""), d.querySelector(".spark-tip")?.textContent);
d.body.dispatchEvent(new w.PointerEvent("pointermove", { bubbles: true, clientX: 5, clientY: 5 }));
check("moving off the chart hides the readout", !d.querySelector(".spark-tip") && !d.querySelector(".spark-mark"));

// Live charts get the readout too.
pick("live");
await until(() => values(cpuChart()).length === 60);
const live = cpuChart();
live.getBoundingClientRect = () => box;
live.dispatchEvent(new w.PointerEvent("pointermove", { bubbles: true, clientX: box.right, clientY: 230 }));
check("live charts show a readout for the newest second", /· \d/.test(d.querySelector(".spark-tip")?.textContent ?? ""), d.querySelector(".spark-tip")?.textContent);

w.localStorage.removeItem("virt-man.range");
w.close();
for (const [ok, name, detail] of results) console.log(`${ok ? "\u2714" : "\u2716"} ${name}${ok ? "" : `  — ${detail}`}`);
const failed = results.filter(([ok]) => !ok);
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall history checks passed");
