// `npm test`: bundles every ui/test/*.test.ts (old-UI suites and the MUI `app-*.test.ts` files)
// with esbuild and runs them under node's test runner. New test files are picked up by name, so
// panel tasks never edit package.json. Run from ui/ (tests read fixtures relative to it).
import { readdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { build } from "esbuild";

const outdir = "node_modules/hk-ui-test";
const only = process.argv.slice(2); // optional substrings: `npm test -- app-store listen`
const files = readdirSync("test")
  .filter((f) => f.endsWith(".test.ts") && (only.length === 0 || only.some((o) => f.includes(o))))
  .sort();
if (files.length === 0) {
  console.error("no test files matched");
  process.exit(1);
}

await build({
  entryPoints: files.map((f) => `test/${f}`),
  bundle: true, platform: "node", format: "esm", outdir,
  outExtension: { ".js": ".mjs" }, logLevel: "warning",
});

// One `node <file>` per suite, in order (node:test reports and sets the exit code per file). Not
// `node --test <files>`: its arguments are globs, and globs skip node_modules/.
const failed = [];
for (const f of files) {
  const r = spawnSync(process.execPath, [`${outdir}/${f.replace(/\.ts$/, ".mjs")}`], { stdio: "inherit" });
  if (r.status !== 0) failed.push(f);
}
console.log(`\n${files.length - failed.length}/${files.length} test files passed${failed.length ? `; failed: ${failed.join(", ")}` : ""}`);
process.exit(failed.length ? 1 : 0);
