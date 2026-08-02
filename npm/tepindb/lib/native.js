// Loads the tepin.node addon shipped in the tepindb-<platform>
// optionalDependency (same distribution as the CLI binary). TEPIN_NODE_ADDON
// overrides with an explicit path — used by tests and source builds.

const PLATFORM_PACKAGES = {
  "darwin arm64": "tepindb-darwin-arm64",
  "linux x64": "tepindb-linux-x64",
  "win32 x64": "tepindb-win32-x64",
};

let cached = null;

function loadAddon() {
  if (cached) return cached;
  const explicit = process.env.TEPIN_NODE_ADDON;
  let file;
  if (explicit) {
    file = explicit;
  } else {
    const key = `${process.platform} ${process.arch}`;
    const pkg = PLATFORM_PACKAGES[key];
    if (!pkg) {
      throw new Error(
        `tepindb: no prebuilt driver addon for ${key}. ` +
          `Supported: ${Object.keys(PLATFORM_PACKAGES).join(", ")}. ` +
          `Build from source (cargo build -p tepin-node) and set TEPIN_NODE_ADDON to the built library.`,
      );
    }
    try {
      file = require.resolve(`${pkg}/lib/tepin.node`);
    } catch {
      throw new Error(
        `tepindb: platform package ${pkg} is missing its driver addon. ` +
          `This usually means npm ran with --omit=optional or an older ${pkg} is installed. ` +
          `Try: npm install ${pkg}@latest --save-optional.`,
      );
    }
  }
  // dlopen instead of require: it accepts any file extension, so a
  // cargo-built .dylib/.so/.dll works directly during development.
  const mod = { exports: {} };
  process.dlopen(mod, file);
  cached = mod.exports;
  return cached;
}

module.exports = { loadAddon };
