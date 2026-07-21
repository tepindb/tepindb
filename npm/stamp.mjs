// Stamp every npm package manifest with the release version and pin the
// intra-project dependency ranges to exactly that version, so a given
// tepindb release always pulls the platform binaries built alongside it.
//
// Usage: node npm/stamp.mjs <version>   (e.g. node npm/stamp.mjs 0.1.2)
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const version = process.argv[2];
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version ?? "")) {
  console.error("usage: node npm/stamp.mjs <version>");
  process.exit(1);
}

const npmDir = dirname(fileURLToPath(import.meta.url));
const manifests = [
  "tepindb/package.json",
  "platform/darwin-arm64/package.json",
  "platform/linux-x64/package.json",
  "platform/win32-x64/package.json",
].map((p) => join(npmDir, p));

for (const file of manifests) {
  const pkg = JSON.parse(readFileSync(file, "utf8"));
  pkg.version = version;
  for (const section of ["dependencies", "optionalDependencies"]) {
    for (const name of Object.keys(pkg[section] ?? {})) {
      if (name === "tepindb" || name.startsWith("tepindb-")) {
        pkg[section][name] = version;
      }
    }
  }
  writeFileSync(file, `${JSON.stringify(pkg, null, 2)}\n`);
  console.log(`stamped ${file} -> ${version}`);
}
