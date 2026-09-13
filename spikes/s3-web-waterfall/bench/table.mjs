// Print results/*.json as a markdown table. Usage: node bench/table.mjs
import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
const dir = path.join(path.dirname(fileURLToPath(import.meta.url)), "../results");
const rows = readdirSync(dir).filter((f) => f.endsWith(".json")).map((f) => JSON.parse(readFileSync(path.join(dir, f), "utf8")));
rows.sort((a, b) => a.date.localeCompare(b.date));
console.log("| mode | bins | in fps | wire | persist | notes | rAF fps | frame p50 / p99 ms | rows/s drawn | seq gaps | client drops | ts→rAF p50 / p99 ms | rAF work p50 / p99 ms | browser CPU % | server CPU % |");
console.log("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
for (const r of rows) {
  const c = r.cfg;
  const notes = [c.throttle > 1 ? `CPU×${c.throttle}` : "", c.finish === "1" ? "gl.finish" : c.finish === "2" ? "readPixels sync" : "", `${c.w}×${c.h}`].filter(Boolean).join(", ");
  console.log(`| ${c.mode} | ${c.bins} | ${c.fps} | ${c.dtype} | ${c.persist} | ${notes} | ${r.rafFps} | ${r.frameMs.p50} / ${r.frameMs.p99} | ${r.dataRowsPerSec} | ${r.seqGaps} | ${r.clientDropped} | ${r.latTsMs.p50} / ${r.latTsMs.p99} | ${r.workMs.p50} / ${r.workMs.p99} | ${r.browserCpuPct} | ${r.serverCpuPct} |`);
}
