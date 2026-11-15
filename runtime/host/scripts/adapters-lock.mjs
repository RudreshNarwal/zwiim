#!/usr/bin/env node
// Regenerates runtime/host/resources/adapters/adapters.lock.json: the exact
// Node runtime and npm package trees the Host installs as its managed Claude
// and Codex adapters, and the native OpenCode binary.
//
//   node runtime/host/scripts/adapters-lock.mjs claude=0.99.0 codex=2.4.0 opencode=1.19.5
//
// An adapter left off the command line keeps its entry from the current lock,
// so bumping one never re-resolves another's dependency tree.
//
// OpenCode is its own executable, published as one npm package per platform.
// Those packages are locked directly (no `npm install`, so the `opencode-ai`
// wrapper's postinstall never runs), each extracted to the same directory;
// exactly one applies to each platform the Host supports.
//
// For every package it records the tarball URL, the registry's sha512
// integrity and the registry's ECDSA signature over
// `<name>@<version>:<integrity>`; the Host verifies both before extracting.
// Packages with install scripts are refused: the Host never runs them. For
// Node it records the SHA-256 from the release's SHASUMS256.txt, and checks
// that file's PGP signature when `gpg` has the Node release keys.
//
// Review the diff before committing. Bump the matching pin in
// src/acp/registry.rs in the same change; a unit test keeps them equal.

import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const REGISTRY = "https://registry.npmjs.org";
const NODE_VERSION = "24.21.0";
const NODE_PLATFORMS = ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"];
const PACKAGES = {
  claude: { package: "@agentclientprotocol/claude-agent-acp", cmd: "claude-agent-acp" },
  codex: { package: "@agentclientprotocol/codex-acp", cmd: "codex-acp" },
};
const NATIVE = {
  opencode: {
    packages: ["opencode-darwin-arm64", "opencode-darwin-x64", "opencode-linux-x64", "opencode-linux-arm64"],
    path: "opencode",
    bin: "bin/opencode",
    args: ["acp"],
  },
};

const here = dirname(fileURLToPath(import.meta.url));
const out = join(here, "..", "resources", "adapters", "adapters.lock.json");

const pins = Object.fromEntries(process.argv.slice(2).map((a) => a.split("=")));
for (const id of Object.keys(pins)) {
  if (!PACKAGES[id] && !NATIVE[id]) throw new Error(`unknown adapter ${id}`);
}
const current = JSON.parse(readFileSync(out, "utf8"));

async function json(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  return res.json();
}

async function text(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  return res.text();
}

// A retired key still vouches for what it signed before it expired (the rule
// `npm audit signatures` applies), so old packages keep verifying.
const keys = (await json(`${REGISTRY}/-/npm/v1/keys`)).keys
  .filter((k) => k.keytype === "ecdsa-sha2-nistp256")
  .map((k) => ({ keyid: k.keyid, key: k.key, expires: k.expires }));
if (!keys.some((k) => k.expires === null)) throw new Error("no current npm registry signing key");

// The registry signature on `meta`, from a key valid when it was published.
function signature(what, meta, published) {
  const valid = (k) => k.expires === null || published < new Date(k.expires);
  const sig = (meta.dist.signatures ?? []).find((s) =>
    keys.some((k) => k.keyid === s.keyid && valid(k)),
  );
  if (!sig) throw new Error(`${what}: no registry signature from a key valid at publication`);
  return { keyid: sig.keyid, sig: sig.sig };
}

async function native(id, version) {
  const { packages: names, path, bin, args } = NATIVE[id];
  const packages = [];
  for (const name of names) {
    const packument = await json(`${REGISTRY}/${name}`);
    const meta = packument.versions[version];
    if (!meta) throw new Error(`${name}@${version} is not published`);
    if (meta.scripts?.preinstall || meta.scripts?.install || meta.scripts?.postinstall) {
      throw new Error(`${name} has an install script`);
    }
    if (meta.libc) throw new Error(`${name} is libc-specific; lock the glibc build`);
    packages.push({
      path,
      name,
      version,
      url: meta.dist.tarball,
      integrity: meta.dist.integrity,
      signature: signature(name, meta, new Date(packument.time[version])),
      ...(meta.os ? { os: meta.os } : {}),
      ...(meta.cpu ? { cpu: meta.cpu } : {}),
    });
  }
  return { version, runtime: "native", entry: `${path}/${bin}`, args, packages };
}

async function adapter(id, version) {
  const { package: name, cmd } = PACKAGES[id];
  const dir = mkdtempSync(join(tmpdir(), `zwiim-lock-${id}-`));
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify({ private: true, dependencies: { [name]: version } }),
  );
  execFileSync(
    "npm",
    ["install", "--package-lock-only", "--ignore-scripts", `--registry=${REGISTRY}`],
    { cwd: dir, stdio: "inherit" },
  );
  const lock = JSON.parse(readFileSync(join(dir, "package-lock.json"), "utf8"));
  const packages = [];
  for (const [path, entry] of Object.entries(lock.packages)) {
    if (!path) continue;
    if (entry.hasInstallScript) throw new Error(`${path} has an install script`);
    // Aliased platform packages (`@openai/codex-darwin-arm64`) carry their
    // real name; everything else is named by its path.
    const real = entry.name ?? path.slice(path.lastIndexOf("node_modules/") + 13);
    const packument = await json(`${REGISTRY}/${real.replace("/", "%2f")}`);
    const meta = packument.versions[entry.version];
    const published = new Date(packument.time[entry.version]);
    if (meta.dist.integrity !== entry.integrity) throw new Error(`${path}: integrity mismatch`);
    if (meta.dist.tarball !== entry.resolved) throw new Error(`${path}: tarball mismatch`);
    const sig = signature(path, meta, published);
    packages.push({
      path,
      name: real,
      version: entry.version,
      url: entry.resolved,
      integrity: entry.integrity,
      signature: sig,
      ...(entry.os ? { os: entry.os } : {}),
      ...(entry.cpu ? { cpu: entry.cpu } : {}),
      ...(entry.libc ? { libc: entry.libc } : {}),
    });
  }
  const top = lock.packages[`node_modules/${name}`];
  const bin = typeof top.bin === "string" ? top.bin : top.bin?.[cmd];
  if (!bin) throw new Error(`${name} has no ${cmd} bin`);
  return { version, entry: `node_modules/${name}/${bin}`, packages };
}

async function node() {
  const base = `https://nodejs.org/dist/v${NODE_VERSION}`;
  const sums = await text(`${base}/SHASUMS256.txt`);
  try {
    const sigDir = mkdtempSync(join(tmpdir(), "zwiim-node-sums-"));
    writeFileSync(join(sigDir, "SHASUMS256.txt"), sums);
    writeFileSync(
      join(sigDir, "SHASUMS256.txt.sig"),
      Buffer.from(await (await fetch(`${base}/SHASUMS256.txt.sig`)).arrayBuffer()),
    );
    execFileSync("gpg", ["--verify", "SHASUMS256.txt.sig", "SHASUMS256.txt"], {
      cwd: sigDir,
      stdio: "inherit",
    });
  } catch (e) {
    console.warn(`WARNING: SHASUMS256.txt signature not verified (${e.message.split("\n")[0]})`);
  }
  const platforms = {};
  for (const platform of NODE_PLATFORMS) {
    const file = `node-v${NODE_VERSION}-${platform}.tar.gz`;
    const line = sums.split("\n").find((l) => l.endsWith(`  ${file}`));
    if (!line) throw new Error(`no checksum for ${file}`);
    platforms[platform] = {
      url: `${base}/${file}`,
      sha256: line.split(" ")[0],
      bin: `node-v${NODE_VERSION}-${platform}/bin/node`,
    };
  }
  return { version: NODE_VERSION, platforms };
}

const result = {
  schema: 1,
  registry_keys: keys,
  node: await node(),
  adapters: {},
};
for (const id of [...Object.keys(PACKAGES), ...Object.keys(NATIVE)]) {
  const version = pins[id];
  if (version) result.adapters[id] = NATIVE[id] ? await native(id, version) : await adapter(id, version);
  else if (current.adapters[id]) result.adapters[id] = current.adapters[id];
  else throw new Error(`missing ${id}=<version>`);
}
writeFileSync(out, `${JSON.stringify(result, null, 2)}\n`);
console.log(`wrote ${out}`);
