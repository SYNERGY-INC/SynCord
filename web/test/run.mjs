// Bundles the tests (so imports without ".ts" work) and runs them with node's test runner.
import { readdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import * as esbuild from "esbuild";

const files = readdirSync("test").filter((f) => f.endsWith(".test.ts"));
await esbuild.build({ entryPoints: files.map((f) => `test/${f}`), bundle: true, platform: "node", format: "esm", outdir: ".test-out", outExtension: { ".js": ".mjs" }, logLevel: "warning" });
const r = spawnSync(process.execPath, ["--test", ...files.map((f) => `.test-out/${f.replace(/\.ts$/, ".mjs")}`)], { stdio: "inherit" });
process.exit(r.status ?? 1);
