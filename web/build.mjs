// Builds the web app into dist/, which the server embeds. Run: npm run build
import { createHash } from "node:crypto";
import { cpSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import * as esbuild from "esbuild";

const out = "dist";
const hash = (data) => createHash("sha256").update(data).digest("hex").slice(0, 10);
rmSync(out, { recursive: true, force: true });
mkdirSync(`${out}/assets`, { recursive: true });

// Opus WebAssembly fallback (used where the browser can't do Opus itself, mostly iPhones).
const wasm = readFileSync("node_modules/@evan/opus/wasm/opus.wasm");
const wasmName = `/assets/opus-${hash(wasm)}.wasm`;
writeFileSync(out + wasmName, wasm);

const bundle = async (entry, define = {}) => {
  const r = await esbuild.build({
    entryPoints: [entry],
    bundle: true,
    minify: true,
    format: "iife",
    target: ["es2020", "safari15"],
    write: false,
    legalComments: "none",
    define,
  });
  return r.outputFiles[0].contents;
};

const js = await bundle("src/main.ts", { OPUS_WASM_URL: JSON.stringify(wasmName) });
const jsName = `/assets/app-${hash(js)}.js`;
writeFileSync(out + jsName, js);

const css = (await esbuild.transform(readFileSync("public/styles.css", "utf8"), { loader: "css", minify: true })).code;
const cssName = `/assets/app-${hash(css)}.css`;
writeFileSync(out + cssName, css);

const html = readFileSync("public/index.html", "utf8").replace("%CSS%", cssName).replace("%JS%", jsName);
writeFileSync(`${out}/index.html`, html);
for (const f of ["manifest.webmanifest", "icon-192.png", "icon-512.png", "apple-touch-icon.png"]) cpSync(`public/${f}`, `${out}/${f}`);

const precache = ["/", jsName, cssName, "/manifest.webmanifest", "/icon-192.png"];
const version = hash(js + css + html);
const sw = await bundle("src/sw.ts", { PRECACHE: JSON.stringify(precache), VERSION: JSON.stringify(version) });
writeFileSync(`${out}/sw.js`, sw);

console.log(`built ${out}/ (${jsName}, ${(js.length / 1024).toFixed(0)} KB js, ${(wasm.length / 1024).toFixed(0)} KB wasm)`);
