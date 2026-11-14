// Tests for scripts/updater-json.mjs, run with: node --test scripts/updater-json.test.mjs
//
// The tool runs as a child process on purpose: its contract is the command line
// and the file it writes, not an exported function. The temp directories keep
// the fixtures (signature files, release JSON) independent per test.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const SCRIPT = fileURLToPath(new URL('./updater-json.mjs', import.meta.url));
const SIGNATURE = 'untrusted comment: signature from tauri secret key\nRWQBIH8hY/bYILE7uvqitWD3teXKAX3IeYczqPT2XX1Gtt9aG2CFq2lC\n';

function workspace(t) {
  const dir = mkdtempSync(join(tmpdir(), 'updater-json-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function run(args, cwd) {
  return spawnSync(process.execPath, [SCRIPT, ...args], { cwd, encoding: 'utf8' });
}

function writeSignature(dir, name, contents = SIGNATURE) {
  const path = join(dir, name);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, contents);
  return path;
}

function readManifest(dir, name = 'latest.json') {
  return JSON.parse(readFileSync(join(dir, name), 'utf8'));
}

test('assembles a manifest whose GitHub URLs use the normalised asset names', (t) => {
  const dir = workspace(t);
  const macSig = writeSignature(dir, 'bundle/macos/AGI Mizar.app.tar.gz.sig');
  const winSig = writeSignature(dir, 'bundle/nsis/AGI Mizar_0.1.3_x64-setup.exe.sig');

  const result = run(
    [
      '--version', '0.1.3',
      '--out', 'out/latest.json',
      `--platform=darwin-aarch64=${macSig}`,
      `--platform=darwin-x86_64=${macSig}`,
      `--platform=windows-x86_64=${winSig}`,
    ],
    dir,
  );

  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /version 0\.1\.3, 3 platforms \(darwin-aarch64, darwin-x86_64, windows-x86_64\)/);

  const manifest = readManifest(dir, 'out/latest.json');
  assert.equal(manifest.version, '0.1.3');
  assert.equal(manifest.notes, 'See the commits in this tag.');
  assert.ok(!Number.isNaN(Date.parse(manifest.pub_date)), manifest.pub_date);
  assert.deepEqual(Object.keys(manifest.platforms), ['darwin-aarch64', 'darwin-x86_64', 'windows-x86_64']);

  const base = 'https://github.com/ShirazuNagisa/AGI-Mizar/releases/download/v0.1.3/';
  assert.equal(manifest.platforms['darwin-aarch64'].url, `${base}AGI.Mizar.app.tar.gz`);
  assert.equal(manifest.platforms['darwin-x86_64'].url, `${base}AGI.Mizar.app.tar.gz`);
  assert.equal(manifest.platforms['windows-x86_64'].url, `${base}AGI.Mizar_0.1.3_x64-setup.exe`);
  // The .sig file's trailing newline is not part of the signature string.
  assert.equal(manifest.platforms['windows-x86_64'].signature, SIGNATURE.trim());
});

test('merges an existing latest.json and lets an explicit platform win', (t) => {
  const dir = workspace(t);
  writeFileSync(
    join(dir, 'existing.json'),
    JSON.stringify({
      version: '0.1.4',
      notes: 'old notes',
      pub_date: '2026-01-01T00:00:00.000Z',
      platforms: {
        'darwin-aarch64': { signature: 'old signature', url: 'https://example.com/old/AGI.Mizar.app.tar.gz' },
        'linux-x86_64': { signature: 'linux signature', url: 'https://example.com/old/agi-mizar.AppImage' },
      },
    }),
  );
  const macSig = writeSignature(dir, 'AGI Mizar.app.tar.gz.sig', 'new signature\n');
  const winSig = writeSignature(dir, 'AGI Mizar_0.1.4_x64-setup.exe.sig');

  const result = run(
    [
      '--version', '0.1.4',
      '--out', 'merged.json',
      '--merge', 'existing.json',
      `--platform=darwin-aarch64=${macSig}`,
      `--platform=windows-x86_64=${winSig}`,
    ],
    dir,
  );

  assert.equal(result.status, 0, result.stderr);
  const manifest = readManifest(dir, 'merged.json');
  assert.equal(manifest.version, '0.1.4');
  assert.deepEqual(Object.keys(manifest.platforms), ['darwin-aarch64', 'linux-x86_64', 'windows-x86_64']);
  assert.equal(manifest.platforms['darwin-aarch64'].signature, 'new signature');
  assert.equal(manifest.platforms['linux-x86_64'].signature, 'linux signature');
});

test('resolves each URL from the release asset list via a glob match', (t) => {
  const dir = workspace(t);
  mkdirSync(join(dir, 'updater-macos'), { recursive: true });
  mkdirSync(join(dir, 'updater-windows'), { recursive: true });
  // The names on disk keep the spaces; the release API reports the dots.
  writeSignature(dir, 'updater-macos/AGI Mizar.app.tar.gz.sig');
  writeSignature(dir, 'updater-windows/AGI Mizar_0.1.3_x64-setup.exe.sig');
  const macAsset = 'https://github.com/ShirazuNagisa/AGI-Mizar/releases/download/v0.1.3/AGI.Mizar.app.tar.gz';
  const winAsset =
    'https://github.com/ShirazuNagisa/AGI-Mizar/releases/download/v0.1.3/AGI.Mizar_0.1.3_x64-setup.exe';
  // The release is a draft when the merge job runs, and a draft cannot be
  // fetched by tag: CI dumps the release list and the tool picks the entry by
  // tag_name. Put a newer release first to prove the selection is by tag.
  writeFileSync(
    join(dir, 'releases.json'),
    JSON.stringify([
      { tag_name: 'v0.1.4', draft: true, body: 'newer', assets: [] },
      {
        tag_name: 'v0.1.3',
        draft: true,
        body: 'See the commits in this tag.',
        assets: [
          { name: 'AGI.Mizar.app.tar.gz', browser_download_url: macAsset },
          { name: 'AGI.Mizar.app.tar.gz.sig', browser_download_url: `${macAsset}.sig` },
          { name: 'AGI.Mizar_0.1.3_x64-setup.exe', browser_download_url: winAsset },
          { name: 'AGI.Mizar_0.1.3_x64-setup.exe.sig', browser_download_url: `${winAsset}.sig` },
        ],
      },
    ]),
  );
  writeFileSync(
    join(dir, 'updater-macos/updater-manifest.json'),
    JSON.stringify({
      platforms: [
        { key: 'darwin-aarch64', sig: 'AGI Mizar.app.tar.gz.sig', asset: 'AGI Mizar.app.tar.gz', match: '*.app.tar.gz' },
        { key: 'darwin-x86_64', sig: 'AGI Mizar.app.tar.gz.sig', asset: 'AGI Mizar.app.tar.gz', match: '*.app.tar.gz' },
      ],
    }),
  );
  writeFileSync(
    join(dir, 'updater-windows/updater-manifest.json'),
    JSON.stringify({
      platforms: [
        {
          key: 'windows-x86_64',
          sig: 'AGI Mizar_0.1.3_x64-setup.exe.sig',
          asset: 'AGI Mizar_0.1.3_x64-setup.exe',
          match: '*-setup.exe',
        },
      ],
    }),
  );

  const result = run(
    [
      '--version', '0.1.3',
      '--out', 'latest.json',
      '--release-json', 'releases.json',
      '--manifest', 'updater-macos/updater-manifest.json',
      '--manifest', 'updater-windows/updater-manifest.json',
    ],
    dir,
  );

  assert.equal(result.status, 0, result.stderr);
  const manifest = readManifest(dir);
  assert.equal(manifest.notes, 'See the commits in this tag.');
  assert.equal(manifest.platforms['darwin-aarch64'].url, macAsset);
  assert.equal(manifest.platforms['darwin-x86_64'].url, macAsset);
  assert.equal(manifest.platforms['windows-x86_64'].url, winAsset);
});

test('honours --release-url, --asset-name, --url and --pub-date overrides', (t) => {
  const dir = workspace(t);
  const sig = writeSignature(dir, 'setup.exe.sig');

  const result = run(
    [
      '--version', 'v2.0.0',
      '--tag', 'v2.0.0',
      '--pub-date', '2026-11-14T12:00:00Z',
      '--out', 'latest.json',
      '--release-url', 'https://downloads.example.com/agi-mizar/v2.0.0',
      '--platform', `windows-x86_64=${sig}`,
      '--asset-name', 'windows-x86_64=AGI Mizar_2.0.0_x64-setup.exe',
      '--platform', `linux-x86_64=${sig}`,
      '--url', 'linux-x86_64=https://mirror.example.com/agi-mizar.AppImage',
    ],
    dir,
  );

  assert.equal(result.status, 0, result.stderr);
  const manifest = readManifest(dir);
  assert.equal(manifest.version, '2.0.0');
  assert.equal(manifest.pub_date, '2026-11-14T12:00:00.000Z');
  // A custom base is used verbatim: the space is URL-encoded, not turned into a dot.
  assert.equal(
    manifest.platforms['windows-x86_64'].url,
    'https://downloads.example.com/agi-mizar/v2.0.0/AGI%20Mizar_2.0.0_x64-setup.exe',
  );
  assert.equal(manifest.platforms['linux-x86_64'].url, 'https://mirror.example.com/agi-mizar.AppImage');
});

test('rejects invalid input and writes nothing', (t) => {
  const dir = workspace(t);
  const sig = writeSignature(dir, 'setup.exe.sig');
  const empty = writeSignature(dir, 'empty.exe.sig', '   \n');

  const cases = [
    {
      name: 'no platforms',
      args: ['--version', '0.1.3', '--out', 'out.json'],
      message: /no platforms/,
    },
    {
      name: 'unknown platform key',
      args: ['--version', '0.1.3', '--platform', `darwin-universal=${sig}`],
      message: /unknown platform key "darwin-universal"/,
    },
    {
      name: 'invalid version',
      args: ['--version', 'latest', '--platform', `windows-x86_64=${sig}`],
      message: /not a SemVer version/,
    },
    {
      name: 'empty signature',
      args: ['--version', '0.1.3', '--platform', `windows-x86_64=${empty}`],
      message: /signature file is empty/,
    },
    {
      name: 'missing signature file',
      args: ['--version', '0.1.3', '--platform', 'windows-x86_64=does-not-exist.sig'],
      message: /signature file not found/,
    },
    {
      name: 'http url',
      args: ['--version', '0.1.3', '--platform', `windows-x86_64=${sig}`, '--url', 'windows-x86_64=http://example.com/x.exe'],
      message: /must be https|expected an https URL/,
    },
    {
      name: 'release list without a matching tag',
      args: [
        '--version', '0.1.3',
        '--platform', `windows-x86_64=${sig}`,
        '--release-json', 'releases.json',
      ],
      message: /has no release with tag "v0\.1\.3"/,
      before: () =>
        writeFileSync(join(dir, 'releases.json'), JSON.stringify([{ tag_name: 'v9.9.9', assets: [] }])),
    },
    {
      name: 'merge version mismatch',
      args: [
        '--version', '0.1.4',
        '--platform', `windows-x86_64=${sig}`,
        '--merge', 'old.json',
      ],
      message: /cannot merge/,
      before: () =>
        writeFileSync(
          join(dir, 'old.json'),
          JSON.stringify({
            version: '0.1.3',
            platforms: {
              'linux-x86_64': { signature: 's', url: 'https://example.com/a.AppImage' },
            },
          }),
        ),
    },
  ];

  for (const item of cases) {
    if (item.before) item.before();
    const result = run([...item.args, '--out', 'should-not-exist.json'], dir);
    assert.notEqual(result.status, 0, `${item.name}: expected a non-zero exit`);
    assert.match(result.stderr, item.message, `${item.name}: stderr was ${result.stderr}`);
    assert.equal(exists(join(dir, 'should-not-exist.json')), false, `${item.name}: wrote output anyway`);
  }
});

function exists(path) {
  try {
    readFileSync(path);
    return true;
  } catch {
    return false;
  }
}
