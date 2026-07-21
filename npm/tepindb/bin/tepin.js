#!/usr/bin/env node
// Launcher for the tepin CLI: resolves the platform-specific binary shipped
// in a tepindb-<platform> optionalDependency and execs it with the caller's args.
// The binary is the slim build (no ONNX) — semantic search needs the full
// binary from GitHub releases or `cargo install tepin-cli`.

const { spawnSync } = require("node:child_process");

const PLATFORM_PACKAGES = {
  "darwin arm64": "tepindb-darwin-arm64",
  "linux x64": "tepindb-linux-x64",
  "win32 x64": "tepindb-win32-x64",
};

function resolveBinary() {
  const key = `${process.platform} ${process.arch}`;
  const pkg = PLATFORM_PACKAGES[key];
  if (!pkg) {
    console.error(
      `tepin: no prebuilt binary for ${key}.\n` +
        `Supported platforms: ${Object.keys(PLATFORM_PACKAGES).join(", ")}.\n` +
        `Build from source instead: cargo install tepin-cli ` +
        `(https://github.com/tepindb/tepindb)`,
    );
    process.exit(1);
  }
  const bin = process.platform === "win32" ? "tepin.exe" : "tepin";
  try {
    return require.resolve(`${pkg}/bin/${bin}`);
  } catch {
    console.error(
      `tepin: platform package ${pkg} is not installed.\n` +
        `This usually means npm was run with --no-optional or --omit=optional, ` +
        `or the package cache is stale.\n` +
        `Try: npm install ${pkg} --save-optional, or reinstall tepindb.`,
    );
    process.exit(1);
  }
}

const result = spawnSync(resolveBinary(), process.argv.slice(2), {
  stdio: "inherit",
});
if (result.error) {
  console.error(`tepin: failed to run binary: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  // Mirror shell convention for signal deaths (e.g. SIGINT -> 130).
  process.exit(128 + (result.signal === "SIGINT" ? 2 : 15));
}
process.exit(result.status ?? 1);
