// Copy the Next static export into `web/dist`, which is what rust-embed reads.
//
// `web/dist` stays committed so `cargo build` never needs Node: the binary
// always has a UI to embed, and a release is still one executable. Run this
// after `next build` (see the `build:ui` target in the Makefile).

import { cp, mkdir, readdir, rm, stat } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const webRoot = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const outDir = path.join(webRoot, "out");
const distDir = path.join(webRoot, "dist");

if (!existsSync(path.join(outDir, "index.html"))) {
  console.error(`No static export at ${outDir}. Run \`next build\` first.`);
  process.exit(1);
}

async function countFiles(dir) {
  let files = 0;
  let bytes = 0;
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      const nested = await countFiles(full);
      files += nested.files;
      bytes += nested.bytes;
    } else {
      files += 1;
      bytes += (await stat(full)).size;
    }
  }
  return { files, bytes };
}

await rm(distDir, { recursive: true, force: true });
await mkdir(distDir, { recursive: true });
await cp(outDir, distDir, {
  recursive: true,
  // `index.txt` is the RSC flight payload and `404.html` is only reached when a
  // static host resolves paths itself; the server has a fallback to index.html
  // and never links to either, so they would just be dead weight in the binary.
  filter: (src) => {
    const name = path.basename(src);
    return name !== "index.txt" && name !== "404.html";
  },
});

const { files, bytes } = await countFiles(distDir);
console.log(
  `Synced ${files} files (${(bytes / 1024).toFixed(0)} KiB) into web/dist`,
);
