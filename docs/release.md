# Releasing AGI Mizar

A release is a tag. Pushing `vX.Y.Z` starts `.github/workflows/release.yml`,
which builds the macOS universal bundle and the Windows NSIS installer, signs
the updater artifacts, and leaves a **draft** GitHub release. Publishing the
draft is a human decision.

This document is the reference for that pipeline: what a release must contain,
which secrets the workflow reads, how `latest.json` is produced (locally and in
CI), and what to check before pressing “Publish”.

## 1. What a published release must contain

| Asset | Platform | Produced by |
|---|---|---|
| `AGI.Mizar.app.tar.gz` + `AGI.Mizar.app.tar.gz.sig` | macOS universal (`darwin-aarch64`, `darwin-x86_64`) | Tauri bundler on `macos-latest` |
| `AGI.Mizar.dmg` | macOS (manual install) | Tauri bundler on `macos-latest` |
| `AGI.Mizar_<version>_x64-setup.exe` + `.sig` | Windows x64 (`windows-x86_64`) | Tauri bundler on `windows-latest` |
| `latest.json` | all platforms | `updater-json` job (or the local packaging script) |

GitHub renames uploaded assets: spaces, brackets and braces become dots
(`AGI Mizar.app.tar.gz` → `AGI.Mizar.app.tar.gz`). The updater links the
*renamed* asset; never hardcode the name on disk when you write a URL by hand.

`latest.json` is the file the app polls:
`https://github.com/ShirazuNagisa/AGI-Mizar/releases/latest/download/latest.json`
(configured in `crates/agi-app/tauri.conf.json`). A published release without
it is not updatable: the settings page's 检查更新 shows
「更新通道暂未提供更新清单，请到发布页查看」 and offers the release page button
instead of 下载并安装. That fallback is deliberate, not a crash — but the point
of the pipeline is that `latest.json` exists.

## 2. The workflow

```
tag push v* 
   ├─ macos        create draft release, build universal .app/.dmg,
   │               sign updater artifacts, upload .sig to the workflow
   ├─ windows      (needs: macos — the draft exists by then) build NSIS,
   │               sign updater artifact, upload .sig to the workflow
   └─ updater-json (needs: macos + windows, ubuntu-latest, contents: write)
                   download both signatures → build asset URLs from the tag →
                   write merged latest.json →
                   gh release upload "$GITHUB_REF_NAME" latest.json --clobber
```

Notes on the job graph:

- **`releaseDraft: true` remains**: the workflow never publishes. The draft is
  visible to people with push access only; publishing is manual.
- **Version == tag guard**: each platform job runs `cargo metadata` and refuses
  to build when the workspace version and the tag disagree (an updater that
  announces the previous version offers an update forever). Keep
  `crates/agi-app/tauri.conf.json`'s `version` equal to the workspace version —
  the bundle and `latest.json` both use it.
- **`uploadUpdaterJson: false` on both `tauri-action` steps** (the input is
  `uploadUpdaterJson`, not `includeUpdaterJson`; the action warns about unknown
  inputs but falls back to its default `true`). The action can write a
  `latest.json` per platform, and two platform jobs would overwrite each other's
  file (and delete/re-upload the asset in the process). One release-level merge
  is done by `scripts/updater-json.mjs` instead.
- **`.sig` files are still uploaded as release assets**: the signature text is
  also embedded in `latest.json`, but the standalone files are the provenance
  record, and the merge job reads them from the workflow artifacts.
- **Draft assets do not live at their final URL.** A draft release's assets are
  reported as `/releases/download/untagged-<hash>/<name>`; publishing does not
  rewrite those paths. v0.2.0's first `latest.json` was generated that way and
  every platform URL 404'd, so the file had to be patched and re-uploaded. The
  merge job therefore never reads the release's asset list: it builds
  `https://github.com/<repo>/releases/download/<tag>/<normalised-name>` itself
  (see `normaliseAssetName` in `scripts/updater-json.mjs`).
- **Draft releases cannot be fetched by tag.** `GET /releases/tags/<tag>` is a
  404 while the release is a draft, which is why the old pipeline went through
  the release list in the first place; nothing in the current pipeline needs
  that lookup.

## 2.2 Signing the updater artifact locally

`TAURI_SIGNING_PRIVATE_KEY` is a repository secret and is the only key whose
public half is baked into `crates/agi-app/tauri.conf.json`
(`plugins > updater > pubkey`, key id `3c925fb34b368af9`). A machine that keeps
an older copy of the key will still bundle fine but print:

```
Warn The updater secret key from `TAURI_SIGNING_PRIVATE_KEY` does not match
     the public key from `plugins > updater > pubkey`.
```

Signatures from that key are rejected at update time, so **do not publish
artifacts signed with a local key**. Check which key you have before trusting a
local build:

```sh
cargo tauri signer sign --private-key-path ~/.tauri/agi-mizar.key \
  --password "$(cat ~/.tauri/agi-mizar.key.password)" /tmp/probe.txt
# decode /tmp/probe.txt.sig, first blob's bytes 2..10 are the key id
```

Verify a published signature against the configured public key (minisign
prehashed mode, `ED`):

```python
import base64, json, hashlib
cfg = json.load(open("crates/agi-app/tauri.conf.json"))
pub = base64.b64decode([l for l in base64.b64decode(
    cfg["plugins"]["updater"]["pubkey"]).decode().splitlines()
    if l.startswith("RW")][0])
sig = base64.b64decode(open("AGI.Mizar_universal.app.tar.gz.sig").read()).decode()
blob = base64.b64decode([l for l in sig.splitlines()
                         if l and not l.startswith(("untrusted", "trusted"))][0])
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
Ed25519PublicKey.from_public_bytes(pub[10:42]).verify(
    blob[10:74], hashlib.blake2b(open("AGI.Mizar_universal.app.tar.gz", "rb").read(),
                                 digest_size=64).digest())   # raises on mismatch
```

Run this for every release: a mismatched key means updates would be offered and
then refused, which users experience as "downloads but never installs".

## 2.1 Linux artifacts

The workflow above builds macOS and Windows only. Linux is packaged separately
by `scripts/package-linux.sh` (portable tarball + `.deb` of the headless
kernel), because it is the *daemon* that ships there, not the desktop bundle.

**Do not run that build on a relay host.** It was done that way for v0.2.0 and
v0.2.1; on the 3.6 GB box the release build swapped until sshd and the relay it
also serves stopped answering, taking the free relay down with it. Options, in
order of preference:

1. a GitHub Actions runner (v0.2.1 used a one-off `linux-package.yml` on
   `ubuntu-22.04`; the binary needs glibc ≥ 2.35 there), or
2. a dedicated build machine, or
3. the relay host only with an added swap file and `CARGO_BUILD_JOBS=1`, off
   peak hours.

`scripts/package-linux.sh --out` accepts an absolute or relative path now; the
packaging steps `cd` into staging directories, so a relative path used to fail
with `tar: dist/control.tar.gz: No such file` (fixed in `a84b9f4`).

## 3. Secrets

Configured in the repository's **Settings → Secrets and variables → Actions**.
Only the first one is required for the release to be updatable; everything else
is optional and env-gated.

| Secret | Required | Purpose |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | **yes** | minisign private key that signs the updater artifacts. Either the key content or, for the Tauri CLI, a path (`TAURI_SIGNING_PRIVATE_KEY_PATH` locally). |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | only if the key is encrypted | password for the private key |
| `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD` | **yes for the macOS bundle** | Developer ID Application certificate (base64 `.p12`) and its export password. Without them the macOS job now fails in its first minute; before the preflight was added it built for twenty minutes and then died in `security import` with "Unable to decode the provided data", because GitHub defines the variables as empty strings rather than leaving them unset. |
| `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | no | code signing / notarization by the Tauri CLI |
| `WINDOWS_CERTIFICATE_BASE64`, `WINDOWS_CERTIFICATE_PASSWORD` | no | Authenticode `.pfx`; imported into `CurrentUser\My`, thumbprint handed to the bundler |
| `WINDOWS_THUMBPRINT` | no | certificate already in the runner store (custom runner) |

Two separate things are called “signing” and must not be conflated:

1. **Updater signing (minisign, always required)**. The private key is a
   long-lived credential: it lives in the workspace credentials file
   `docs/servers-ssh.md` (outside this repository, mode 600), never in git and
   never in a workflow file. The matching public key is
   `plugins.updater.pubkey` in `crates/agi-app/tauri.conf.json` and ships inside
   every installed app.
2. **OS code signing (optional in the table, required in practice)**.
   Authenticode decides whether SmartScreen warns; Apple signing/notarization
   decides whether Gatekeeper opens the app. A `.dmg` built without the Apple
   certificate is refused by Gatekeeper on a stranger's Mac, so treat the two
   `APPLE_CERTIFICATE*` secrets as required for any release that ships macOS.

   **Local packaging caveat:** `~/.tauri/agi-mizar.key` on a developer machine
   may be an older updater key. The updater verifies against
   `plugins.updater.pubkey` in `crates/agi-app/tauri.conf.json`; if the local
   public key does not match it, a locally built `.sig` is accepted by nobody.
   Compare the two before using the "build locally and upload" fallback, and
   get the current private key from the repository secret rather than assuming
   the file in `~/.tauri` is the live one.
   without right-click → Open. Neither affects the updater signature, but a
   release built without them is visibly unsigned.

### Key rotation

`plugins.updater.pubkey` is compiled into the app, so rotating the key is a
breaking change: already-installed apps reject updates signed with the new key.

**Rotated on 2026-11-14.** The key that signed 0.1.0/0.1.1 was lost, so
`plugins.updater.pubkey` in `crates/agi-app/tauri.conf.json` now names a new
key; the private half is in the workspace credentials file and in the
`TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` repository
secrets. Consequences:

- installs of 0.1.0–0.1.3 embed the **old** public key: they cannot verify a
  release signed with the new key, so their 下载并安装 will fail. They must
  install the first release built after the rotation **manually**, once; from
  that build on, automatic updates work again.
- the 检查更新 fallback still names the newest release and opens its page, so an
  old install can find out that it needs a manual download.

To rotate again: keep signing releases with the old key while you still have
it, ship a build containing the new public key, then switch the signing key.
Losing the private key forces the rotate-and-reinstall-once path above.

## 4. `latest.json`

Shape (Tauri v2 static JSON; the platform keys are `OS-ARCH`):

```json
{
  "version": "0.1.3",
  "notes": "See the commits in this tag.",
  "pub_date": "2026-11-14T12:00:00.000Z",
  "platforms": {
    "darwin-aarch64": {
      "signature": "untrusted comment: …\nRWQ…",
      "url": "https://github.com/ShirazuNagisa/AGI-Mizar/releases/download/v0.1.3/AGI.Mizar.app.tar.gz"
    },
    "darwin-x86_64": { "signature": "…", "url": "…/AGI.Mizar.app.tar.gz" },
    "windows-x86_64": {
      "signature": "…",
      "url": "https://github.com/ShirazuNagisa/AGI-Mizar/releases/download/v0.1.3/AGI.Mizar_0.1.3_x64-setup.exe"
    }
  }
}
```

- A universal macOS bundle is one artifact, so `darwin-aarch64` and
  `darwin-x86_64` share a signature and URL.
- `signature` is the `.sig` file's content (trimmed), not a URL — the updater
  verifies the downloaded artifact against it before installing.
- `url` must be https and must be the URL GitHub actually serves.
- The updater validates the **whole file** before comparing versions, so an
  invalid or missing platform entry poisons the release for everyone.

### In CI

`scripts/updater-json.mjs` writes the merged file. The `updater-json` job:

1. downloads the `updater-macos` / `updater-windows` workflow artifacts (each
   holds the `.sig` file and a small `updater-manifest.json` naming the platform
   keys, the local asset name and a glob for the release asset);
2. runs the tool with `--version`, `--tag`, `--repo` and the manifests only —
   the tool builds each URL from the tag and normalises the asset name the same
   way GitHub does (`AGI Mizar…` → `AGI.Mizar…`, `*.app.tar.gz` and
   `*-setup.exe`);
3. `gh release upload "$GITHUB_REF_NAME" latest.json --clobber`.

Do **not** pass `--release-json` in CI: the release is a draft, and its asset
URLs are the temporary `untagged-<hash>` paths that 404 after publishing. If a
`latest.json` ever does ship those URLs, publish the release, download the file,
replace the path segment with the tag, delete the asset and upload it again.

### Locally

`scripts/package.sh` (macOS) and `scripts/package.ps1 -WithUpdater` (Windows)
call the same tool after a successful bundle and write `latest.json` next to the
bundle:

- macOS: `target/release/bundle/latest.json` (or
  `target/universal-apple-darwin/release/bundle/latest.json` with `--universal`);
- Windows: `target/release/bundle/latest.json`.

The URLs are built with the GitHub release base
`https://github.com/ShirazuNagisa/AGI-Mizar/releases/download/v<version>/` and
the asset name normalised the way GitHub normalises it. `RELEASE_REPO` can be
set to point a locally built manifest at a fork. Node is only needed when
updater artifacts are enabled (the key is present); a build without the key does
not call the tool.

Manual runs:

```bash
# macOS local build, writes .app.tar.gz + .sig + latest.json when the key is set
export TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/agi-mizar.key)"
scripts/package.sh --universal

# Windows local build
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content .\updater.key -Raw
scripts\package.ps1 -FetchLlama -WithUpdater
```

`scripts/updater-json.mjs --help` documents every flag (`--platform`,
`--asset-name`, `--asset-match`, `--release-json`, `--merge`, …); its header has
the full rationale. Its tests run with:

```bash
node --test scripts/updater-json.test.mjs
```

## 5. Cutting a release

1. Bump the version in `Cargo.toml` **and**
   `crates/agi-app/tauri.conf.json` together; commit and push; make sure CI on
   `main` is green.
2. `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. Wait for the three release jobs. A failure in `macos` or `windows` stops the
   run before `updater-json`; a failure in `updater-json` leaves the draft
   without `latest.json` — fix the cause and re-run the job.
4. Open the draft release and check the asset list:
   - macOS: `.dmg`, `AGI.Mizar.app.tar.gz`, its `.sig`;
   - Windows: `AGI.Mizar_<version>_x64-setup.exe`, its `.sig`;
   - `latest.json`, with all three platform keys and https URLs.
5. Inspect `latest.json` (download it, or read the `latest-json` workflow
   artifact). It must not still name the previous version.
6. Publish the draft. `releases/latest/download/latest.json` only resolves for
   published releases.
7. Smoke-test: install the previous release, run 检查更新, expect the new
   version; 下载并安装 must download, verify and restart.

## 6. Known limitations

- **macOS notarization and the bundled llama.cpp runtime**: the release bundle
  now stages `scripts/fetch-llama.sh` output and the `agi-remote` sidecar in
  `Contents/Resources/bin`. The bundler does not sign `Resources/`, so those
  binaries are only ad-hoc signed; `package.sh` re-signs them after the build for
  local Developer ID releases, but `tauri-action` has no hook between bundling
  and notarizing. A notarized macOS CI release must therefore sign
  `Resources/bin` before the app is packed (or accept that notarization will
  reject it). Today the repository has no Apple secrets, so CI builds are
  unsigned; this becomes a problem only when Apple signing is enabled.
- **Authenticode and `-WithUpdater` locally**: `scripts/package.ps1` applies
  Authenticode **after** the bundler wrote the installer, which changes the file
  bytes the `.sig` covered. The script warns when both are requested. For a
  signed local release either let the bundler sign (as CI does, via
  `bundle.windows.certificateThumbprint`) or re-sign the installer with
  `npx @tauri-apps/cli@2 signer sign <installer> --app-version <version>` and
  regenerate `latest.json`.
- **No Linux release artifact yet**: `scripts/package-linux.sh` exists for local
  Linux builds and `scripts/updater-json.mjs` accepts `linux-x86_64`, but the
  workflow has no Linux job, so released `latest.json` files have no Linux entry.
- **CI-only behaviour**: the workflow's macOS/Windows steps cannot be executed on
  this workstation. They were validated statically (actionlint, YAML parse,
  every `run:` block parsed with `bash -n` / the PowerShell parser); the real
  build is verified when the tag is pushed.
