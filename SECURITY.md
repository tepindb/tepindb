# Security

Security is a headline feature of TepinDB, not an afterthought: agents will
auto-download binaries, models, and database files, so every one of those
paths is treated as hostile until verified.

## Threat model & measures

**Untrusted `.tepin` files.** A database file an agent receives is
attacker-controlled input. The container parser is fuzzed in CI
(`fuzz/fuzz_targets/preamble.rs`) and must reject any garbage without
panicking. The redb payload is parsed by redb's own checked reader.

**Model downloads.** The embedding model (bge-small) is downloaded lazily,
exclusively from this project's GitHub releases, and verified against a
SHA-256 pinned in the source. There is no third-party fetch path (no
HuggingFace, no CDN) in the runtime.

**Release artifacts.** All binaries and models ship from GitHub releases,
with an SBOM attached to every release. The npm packages (`tepindb`,
`tepin`, `@tepindb/*`) are published by the same release workflow from the
same tag, with npm provenance attestations linking each package back to
this repository and workflow run; the binary inside a platform package is
byte-identical to the `tepin-slim-*` asset on the corresponding GitHub
release. GitHub releases remain the authoritative channel.

**Supply chain.** Dependencies are locked (`Cargo.lock` committed);
CI builds from the lockfile.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting on this repository.
We'll acknowledge within a few days.
