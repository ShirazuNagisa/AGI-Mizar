#!/usr/bin/env node
//
// Build (or merge) the Tauri v2 updater manifest, `latest.json`, for a release.
//
// Tauri's bundler writes the signed updater artifacts (`.app.tar.gz` and
// `*-setup.exe`, plus their `.sig` files) but it never writes `latest.json`:
// that file says, per platform, which release asset to download and which
// minisign signature to verify. It is a *release-level* file — one per version,
// not one per build — so something has to merge the platforms into one
// document. CI does it in the `updater-json` job from the assets the two
// platform jobs uploaded; the local packaging scripts call the same tool, which
// is why this is dependency-free Node: the manifest a local build writes has
// exactly the shape GitHub Releases serves.
//
// Usage
//
//   Local (the file names come from the bundle the Tauri CLI just wrote):
//
//     node scripts/updater-json.mjs --version 0.1.3 --out latest.json \
//       --platform darwin-aarch64="…/AGI Mizar.app.tar.gz.sig" \
//       --platform darwin-x86_64="…/AGI Mizar.app.tar.gz.sig" \
//       --platform windows-x86_64="…/AGI Mizar_0.1.3_x64-setup.exe.sig"
//
//   CI (the URLs must be the ones GitHub actually serves, so they are resolved
//   from the release's asset list instead of being reconstructed). The release
//   is still a draft at this point, and a draft cannot be fetched by tag — the
//   release list is the way in (tauri-action's create-release.ts lists releases
//   for exactly this reason). With an array, the entry whose tag_name matches
//   --tag is used:
//
//     gh api "repos/$GITHUB_REPOSITORY/releases?per_page=100" > releases.json
//     node scripts/updater-json.mjs --version 0.1.3 --release-json releases.json \
//       --manifest updater-macos/updater-manifest.json \
//       --manifest updater-windows/updater-manifest.json --out latest.json
//
// Options
//
//   --version X.Y.Z          required; a leading `v` is accepted and stripped
//   --tag vX.Y.Z             release tag; defaults to v<version>
//   --repo owner/name        GitHub repository; defaults to ShirazuNagisa/AGI-Mizar
//   --notes TEXT             release notes; defaults to the release body from
//                            --release-json, else "See the commits in this tag."
//   --pub-date RFC3339       defaults to now (UTC)
//   --out PATH               output file; defaults to ./latest.json
//   --platform KEY=SIG       repeatable; platform key + its .sig file (required
//                            for every platform unless a --manifest supplies it)
//   --asset-name KEY=NAME    repeatable; release asset file name to link for a
//                            platform (default: the .sig file name minus .sig)
//   --asset-match KEY=GLOB   repeatable; with --release-json, the glob that
//                            selects the platform's asset (e.g. "*-setup.exe")
//   --url KEY=URL            repeatable; use this download URL verbatim
//   --release-url BASE       download URL base for every platform, used as-is
//                            (no GitHub asset-name normalisation). Default:
//                            https://github.com/<repo>/releases/download/<tag>/
//   --release-json PATH      GitHub release JSON: either one release object (as
//                            returned by /releases/tags/<tag> or /releases/<id>)
//                            or a list of releases (as returned by /releases);
//                            with a list, the entry whose tag_name is --tag is
//                            used. Resolves each platform's URL from
//                            assets[].browser_download_url
//   --manifest PATH          repeatable; small JSON manifest written by the
//                            platform CI jobs (see below); "sig" paths are
//                            relative to the manifest file
//   --merge PATH             repeatable; existing latest.json to carry other
//                            platforms over from (same version only)
//   --quiet                  do not print the one-line summary
//   --help                   print this help
//
// Manifest input (`--manifest`) is how a platform job hands the merge job its
// signature file without a shell pipeline. Both `sig` and at least one of
// `asset` / `match` are what CI writes:
//
//   {
//     "platforms": [
//       {
//         "key": "windows-x86_64",
//         "sig": "AGI Mizar_0.1.3_x64-setup.exe.sig",
//         "asset": "AGI Mizar_0.1.3_x64-setup.exe",
//         "match": "*-setup.exe"
//       }
//     ]
//   }
//
// URL resolution per platform, first one that applies wins:
//
//   1. `--url KEY=URL` / manifest `url` — used verbatim (must be https).
//   2. `--release-json` — the asset selected by `--asset-match KEY=GLOB` /
//      manifest `match`, else `asset`, else the .sig file name minus `.sig`.
//      Asset names are compared after GitHub's own normalisation (spaces,
//      brackets and braces become dots), because that is what the release API
//      reports and it differs from the name on disk.
//   3. `--release-url BASE` (or the GitHub default derived from --repo/--tag)
//      with the asset name appended. The GitHub default normalises the name the
//      same way; a custom `--release-url` uses it as given.
//
// Platform keys are the ones Tauri v2 asks for (`OS-ARCH`): darwin-aarch64,
// darwin-x86_64, windows-x86_64, linux-x86_64. Anything else is rejected,
// because a typo there is a platform that silently never updates.
//
// A macOS universal bundle signs one artifact for both Apple architectures, so
// two `--platform` entries point at the same .sig file (and the same URL).
//
// Exit codes: 0 on success, 1 on invalid input. Nothing is written on error.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { basename, dirname, isAbsolute, resolve } from 'node:path';
import { parseArgs } from 'node:util';

const PLATFORM_KEYS = ['darwin-aarch64', 'darwin-x86_64', 'windows-x86_64', 'linux-x86_64'];
const DEFAULT_REPO = 'ShirazuNagisa/AGI-Mizar';
const DEFAULT_NOTES = 'See the commits in this tag.';
// SemVer 2.0.0, with or without the leading `v` the updater also accepts.
const SEMVER = /^v?(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)$/;

class UsageError extends Error {}

function fail(message) {
  throw new UsageError(message);
}

function optionalString(value, what) {
  if (value === undefined || value === null) return null;
  if (typeof value !== 'string') fail(`${what} must be a string`);
  return value;
}

function readJsonFile(path, what) {
  const absolute = resolve(path);
  if (!existsSync(absolute)) fail(`${what} not found: ${absolute}`);
  let text;
  try {
    text = readFileSync(absolute, 'utf8');
  } catch (error) {
    fail(`cannot read ${what} ${absolute}: ${error.message}`);
  }
  try {
    return JSON.parse(text.replace(/^\uFEFF/, ''));
  } catch (error) {
    fail(`${what} ${absolute} is not valid JSON: ${error.message}`);
  }
}

// GitHub renames uploaded release assets: whitespace, brackets and braces
// become dots, and accents are stripped. A local path is therefore not the name
// the release serves; the updater must be pointed at the served one. tauri-action
// reproduces the same rule when it maps local paths to the assets it uploaded,
// which is where this form comes from.
function githubAssetName(name) {
  return name
    .trim()
    .replace(/[ ()[\]{}]/g, '.')
    .replace(/\.\./g, '.')
    .normalize('NFD')
    .replace(/[\u0300-\u036f]/g, '');
}

function globToRegExp(glob) {
  let pattern = '^';
  for (const character of glob) {
    if (character === '*') pattern += '.*';
    else if (character === '?') pattern += '.';
    else pattern += character.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  }
  return new RegExp(`${pattern}$`);
}

function assertPlatformKey(key) {
  if (!PLATFORM_KEYS.includes(key)) {
    fail(`unknown platform key "${key}"; Tauri v2 uses ${PLATFORM_KEYS.join(', ')}`);
  }
}

function assertHttpsUrl(url, what) {
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    fail(`${what}: not a URL: ${url}`);
  }
  if (parsed.protocol !== 'https:') fail(`${what}: expected an https URL, got ${url}`);
}

function derivedAssetName(spec) {
  return basename(spec.sig).replace(/\.sig$/i, '');
}

function parseVersion(value) {
  if (typeof value !== 'string' || !value.trim()) {
    fail('--version is required (e.g. --version 0.1.3)');
  }
  const match = SEMVER.exec(value.trim());
  if (!match) fail(`--version is not a SemVer version: ${value}`);
  return match[1];
}

function parseRepo(value) {
  const repo = (value ?? DEFAULT_REPO).trim();
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repo)) {
    fail(`--repo expects owner/name, got: ${repo}`);
  }
  return repo;
}

function parseTag(value, version) {
  const tag = (value ?? `v${version}`).trim();
  if (!tag || /\s/.test(tag)) fail(`--tag cannot be empty or contain whitespace: "${tag}"`);
  return tag;
}

function parseDate(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) fail(`--pub-date is not a date: ${value}`);
  return date.toISOString();
}

function parseMapping(entries, option) {
  const map = new Map();
  for (const entry of entries ?? []) {
    const separator = entry.indexOf('=');
    if (separator <= 0) fail(`${option} expects KEY=VALUE, got: ${entry}`);
    const key = entry.slice(0, separator).trim();
    const value = entry.slice(separator + 1);
    if (!key || !value) fail(`${option} expects KEY=VALUE, got: ${entry}`);
    map.set(key, value);
  }
  return map;
}

function specFromManifestEntry(entry, manifestPath) {
  if (!entry || typeof entry !== 'object' || Array.isArray(entry)) {
    fail(`manifest ${manifestPath}: every platform entry must be an object`);
  }
  const key = optionalString(entry.key, `manifest ${manifestPath}: "key"`);
  if (!key) fail(`manifest ${manifestPath}: every platform entry needs a "key"`);
  assertPlatformKey(key);
  const sig = optionalString(entry.sig, `manifest ${manifestPath}: "sig"`);
  if (!sig) fail(`manifest ${manifestPath}: platform ${key} has no "sig" file`);
  return {
    key,
    sig: isAbsolute(sig) ? sig : resolve(dirname(resolve(manifestPath)), sig),
    asset: optionalString(entry.asset, `manifest ${manifestPath}: "asset"`),
    match: optionalString(entry.match, `manifest ${manifestPath}: "match"`),
    url: optionalString(entry.url, `manifest ${manifestPath}: "url"`),
  };
}

function loadManifests(paths) {
  const specs = [];
  for (const path of paths ?? []) {
    const json = readJsonFile(path, 'manifest');
    const entries = Array.isArray(json) ? json : json && json.platforms;
    if (!Array.isArray(entries)) {
      fail(`manifest ${resolve(path)} has no "platforms" array`);
    }
    for (const entry of entries) specs.push(specFromManifestEntry(entry, path));
  }
  return specs;
}

function loadRelease(path, tag) {
  const json = readJsonFile(path, 'release JSON');
  let release = json;
  // A draft release cannot be fetched by tag; CI dumps the release list and the
  // matching draft is selected here (the list is newest-first).
  if (Array.isArray(json)) {
    release = json.find((entry) => entry && entry.tag_name === tag);
    if (!release) {
      fail(`release JSON ${resolve(path)} has no release with tag "${tag}"`);
    }
  }
  if (!release || typeof release !== 'object' || !Array.isArray(release.assets)) {
    fail(`release JSON ${resolve(path)} has no "assets" array`);
  }
  const assets = release.assets.filter(
    (asset) => asset && typeof asset.name === 'string' && typeof asset.browser_download_url === 'string',
  );
  return { assets, body: typeof release.body === 'string' ? release.body : '' };
}

function loadMerge(path, version) {
  const json = readJsonFile(path, 'existing latest.json');
  if (!json || typeof json !== 'object' || Array.isArray(json) || !json.platforms) {
    fail(`existing latest.json ${resolve(path)} has no "platforms" object`);
  }
  const mergedVersion = String(json.version ?? '').replace(/^v/, '');
  if (mergedVersion && mergedVersion !== version) {
    fail(
      `cannot merge ${resolve(path)}: it is version ${json.version}, this manifest is ${version}; ` +
        'platform entries from another version would point at the wrong artifacts',
    );
  }
  const platforms = new Map();
  for (const [key, entry] of Object.entries(json.platforms)) {
    assertPlatformKey(key);
    if (!entry || typeof entry !== 'object' || typeof entry.signature !== 'string' || !entry.signature.trim()) {
      fail(`existing latest.json ${resolve(path)}: platform ${key} has no signature`);
    }
    assertHttpsUrl(entry.url, `existing latest.json platform ${key}`);
    platforms.set(key, { signature: entry.signature.trim(), url: entry.url });
  }
  return platforms;
}

function resolveFromRelease(spec, release) {
  const target = spec.match ?? spec.asset ?? derivedAssetName(spec);
  let candidates;
  if (/[*?]/.test(target)) {
    const pattern = globToRegExp(target);
    // Match both the name the API reports and its normalised form: a manifest
    // written from a local file name has spaces where the release has dots.
    candidates = release.assets.filter(
      (asset) => pattern.test(asset.name) || pattern.test(githubAssetName(asset.name)),
    );
  } else {
    const wanted = githubAssetName(target).toLowerCase();
    candidates = release.assets.filter(
      (asset) => asset.name === target || githubAssetName(asset.name).toLowerCase() === wanted,
    );
  }
  if (candidates.length === 0) {
    const available = release.assets.map((asset) => asset.name).join(', ') || 'none';
    fail(`no release asset matches "${target}" for platform ${spec.key} (release assets: ${available})`);
  }
  if (candidates.length > 1) {
    const exact = candidates.filter((asset) => asset.name === target);
    if (exact.length !== 1) {
      const names = candidates.map((asset) => asset.name).join(', ');
      fail(`"${target}" matches ${candidates.length} release assets for platform ${spec.key} (${names}); pass a narrower --asset-match`);
    }
    candidates = exact;
  }
  return candidates[0].browser_download_url;
}

function buildUrl(spec, repo, tag, releaseBase) {
  const asset = spec.asset ?? derivedAssetName(spec);
  if (releaseBase) return `${releaseBase}${encodeURIComponent(asset)}`;
  const name = githubAssetName(asset);
  const base = `https://github.com/${repo}/releases/download/${encodeURIComponent(tag)}/`;
  return `${base}${encodeURIComponent(name)}`;
}

function readSignature(file) {
  const path = resolve(file);
  if (!existsSync(path)) fail(`signature file not found: ${path}`);
  let contents;
  try {
    contents = readFileSync(path, 'utf8');
  } catch (error) {
    fail(`cannot read signature file ${path}: ${error.message}`);
  }
  const signature = contents.replace(/^\uFEFF/, '').trim();
  if (!signature) fail(`signature file is empty: ${path}`);
  return signature;
}

function resolvePlatform(spec, context) {
  if (!spec.sig) {
    fail(`platform ${spec.key} has no signature file; pass --platform ${spec.key}=<file> or a manifest entry`);
  }
  const signature = readSignature(spec.sig);
  let url = spec.url ?? null;
  if (url === null && context.release) url = resolveFromRelease(spec, context.release);
  if (url === null) url = buildUrl(spec, context.repo, context.tag, context.releaseBase);
  assertHttpsUrl(url, `platform ${spec.key}`);
  return { signature, url };
}

function buildDocument(options) {
  const version = parseVersion(options.values.version);
  const repo = parseRepo(options.values.repo);
  const tag = parseTag(options.values.tag, version);
  const release = options.values['release-json'] ? loadRelease(options.values['release-json'], tag) : null;
  const notes =
    options.values.notes !== undefined
      ? options.values.notes
      : release && release.body
        ? release.body
        : DEFAULT_NOTES;
  const pubDate = options.values['pub-date'] ? parseDate(options.values['pub-date']) : new Date().toISOString();

  let releaseBase = null;
  if (options.values['release-url']) {
    let parsed;
    try {
      parsed = new URL(options.values['release-url']);
    } catch {
      fail(`--release-url is not a URL: ${options.values['release-url']}`);
    }
    if (parsed.protocol !== 'https:') fail('--release-url must be https');
    releaseBase = parsed.href.endsWith('/') ? parsed.href : `${parsed.href}/`;
  }

  // Explicit platforms come from manifests and --platform; later entries win so
  // a command line flag can correct a manifest without editing it.
  const specs = new Map();
  for (const spec of loadManifests(options.values.manifest)) specs.set(spec.key, spec);
  for (const [key, sig] of parseMapping(options.values.platform, '--platform')) {
    assertPlatformKey(key);
    specs.set(key, { ...(specs.get(key) ?? { key }), key, sig: resolve(sig) });
  }
  for (const [key, asset] of parseMapping(options.values['asset-name'], '--asset-name')) {
    if (!specs.has(key)) fail(`--asset-name names a platform that has no --platform/manifest entry: ${key}`);
    specs.get(key).asset = asset;
  }
  for (const [key, match] of parseMapping(options.values['asset-match'], '--asset-match')) {
    if (!specs.has(key)) fail(`--asset-match names a platform that has no --platform/manifest entry: ${key}`);
    specs.get(key).match = match;
  }
  for (const [key, url] of parseMapping(options.values.url, '--url')) {
    if (!specs.has(key)) fail(`--url names a platform that has no --platform/manifest entry: ${key}`);
    specs.get(key).url = url;
  }

  // --merge carries over platforms another run already built for this version.
  const platforms = new Map();
  for (const path of options.values.merge ?? []) {
    for (const [key, entry] of loadMerge(path, version)) platforms.set(key, entry);
  }
  const context = { repo, tag, releaseBase, release };
  for (const spec of specs.values()) {
    platforms.set(spec.key, resolvePlatform(spec, context));
  }
  if (platforms.size === 0) fail('no platforms: pass at least one --platform KEY=SIG or a manifest');

  const ordered = {};
  for (const key of [...platforms.keys()].sort()) ordered[key] = platforms.get(key);
  return { document: { version, notes, pub_date: pubDate, platforms: ordered }, out: options.values.out };
}

const USAGE = `usage: node scripts/updater-json.mjs --version X.Y.Z [options]

  --version X.Y.Z          required; a leading v is accepted
  --tag vX.Y.Z             release tag (default: v<version>)
  --repo owner/name        GitHub repository (default: ${DEFAULT_REPO})
  --notes TEXT             release notes
  --pub-date RFC3339       publication date (default: now, UTC)
  --out PATH               output file (default: latest.json)
  --platform KEY=SIG       repeatable; platform key + .sig file
  --asset-name KEY=NAME    repeatable; release asset name for a platform
  --asset-match KEY=GLOB   repeatable; glob selecting the platform's release asset
  --url KEY=URL            repeatable; full download URL for a platform
  --release-url BASE       download URL base (custom hosts keep asset names as-is)
  --release-json PATH      GitHub release JSON; resolves URLs from its assets
  --manifest PATH          repeatable; platform manifest JSON (sig paths are relative)
  --merge PATH             repeatable; existing latest.json to carry platforms over from
  --quiet                  do not print the one-line summary
  --help                   print this help

Platform keys: ${PLATFORM_KEYS.join(', ')}
Run without arguments to see the full documentation in the file header.`;

function main() {
  let parsed;
  try {
    parsed = parseArgs({
      allowPositionals: false,
      options: {
        version: { type: 'string' },
        tag: { type: 'string' },
        repo: { type: 'string' },
        notes: { type: 'string' },
        'pub-date': { type: 'string' },
        out: { type: 'string' },
        platform: { type: 'string', multiple: true },
        'asset-name': { type: 'string', multiple: true },
        'asset-match': { type: 'string', multiple: true },
        url: { type: 'string', multiple: true },
        'release-url': { type: 'string' },
        'release-json': { type: 'string' },
        manifest: { type: 'string', multiple: true },
        merge: { type: 'string', multiple: true },
        quiet: { type: 'boolean' },
        help: { type: 'boolean' },
      },
    });
  } catch (error) {
    throw new UsageError(error.message);
  }

  if (parsed.values.help) {
    console.log(USAGE);
    return 0;
  }

  const { document, out } = buildDocument(parsed);
  const output = resolve(out ?? 'latest.json');
  mkdirSync(dirname(output), { recursive: true });
  writeFileSync(output, `${JSON.stringify(document, null, 2)}\n`, 'utf8');

  if (!parsed.values.quiet) {
    const keys = Object.keys(document.platforms);
    console.log(
      `latest.json: version ${document.version}, ${keys.length} platform${keys.length === 1 ? '' : 's'} ` +
        `(${keys.join(', ')}) -> ${output}`,
    );
  }
  return 0;
}

try {
  process.exitCode = main();
} catch (error) {
  if (error instanceof UsageError) {
    console.error(`updater-json: ${error.message}`);
    console.error('run `node scripts/updater-json.mjs --help` for the options');
    process.exitCode = 1;
  } else {
    throw error;
  }
}
