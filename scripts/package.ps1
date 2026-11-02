<#
.SYNOPSIS
    Build the Windows installers: hook shim resource, llama.cpp runtime, NSIS (or MSI).

.DESCRIPTION
    The Windows counterpart of scripts/package.sh. It does the same things in
    the same order, with the substitutions Windows needs:

      1. build crates/agi-hook-shim and copy the .exe into the bundle resources
         (the installer writes an absolute path to it into
         %USERPROFILE%\.claude\settings.json, so it has to be in the bundle);
      2. make sure crates/agi-app/resources/bin exists with the pinned
         llama.cpp runtime in it (-FetchLlama runs scripts/fetch-llama.ps1);
      3. run the Tauri CLI through npx (@tauri-apps/cli 2.x; `cargo tauri` is
         not a thing on a clean Windows machine) with an explicit --bundles;
      4. print the installers and their SHA256.

    Signing:
      - The updater manifest is minisign-signed by the Tauri CLI. Without
        TAURI_SIGNING_PRIVATE_KEY (or _PATH) the bundler fails after writing
        the installer, so overrides are written that turn
        bundle.createUpdaterArtifacts off for local builds; -WithUpdater turns
        that off the other way and requires the key.
      - Authenticode is separate and optional: set bundle.windows.
        certificateThumbprint / signCommand in a config file passed via
        `--config` (see docs/windows.md). Without it Windows SmartScreen warns
        on first run; the installer itself still works.

.PARAMETER Bundles
    Comma separated bundles: nsis (default), msi, or nsis,msi. MSI needs the
    WiX toolset cached in %LOCALAPPDATA%\tauri\WixTools314.

.PARAMETER FetchLlama
    Run scripts/fetch-llama.ps1 first (downloads the pinned bNNNNN archive,
    verifies its SHA256 and installs it into resources/bin).

.PARAMETER Archive
    Passed to fetch-llama.ps1: use an already-downloaded zip instead.

.PARAMETER Proxy
    Passed to fetch-llama.ps1: prefix the GitHub URL, e.g.
    https://gh-proxy.com/ .

.PARAMETER WithUpdater
    Require TAURI_SIGNING_PRIVATE_KEY / TAURI_SIGNING_PRIVATE_KEY_PATH and let
    the bundler produce the updater artifacts (.sig + latest.json). Fails when
    neither is set.

.PARAMETER Force
    Passed to fetch-llama.ps1: re-download the archive even if cached.

.EXAMPLE
    scripts/package.ps1 -FetchLlama

.EXAMPLE
    scripts/package.ps1 -Archive D:\downloads\llama-b12000-bin-win-cpu-x64.zip

.EXAMPLE
    scripts/package.ps1 -Bundles nsis,msi

.EXAMPLE
    $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content ... -Raw
    scripts/package.ps1 -WithUpdater
#>
[CmdletBinding()]
param(
    [string]$Bundles = 'nsis',
    [switch]$FetchLlama,
    [string]$Archive,
    [string]$Proxy,
    [switch]$WithUpdater,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
# The script can be invoked from any directory; cargo and the Tauri CLI both
# resolve the project relative to the current one, so pin it to the repo root.
Set-Location -LiteralPath $root
$resourcesDir = Join-Path $root 'crates/agi-app/resources'
$shimResource = Join-Path $resourcesDir 'agi-mizar-hook.exe'
$llamaDir = Join-Path $resourcesDir 'bin'
$llamaServer = Join-Path $llamaDir 'llama-server.exe'
$bundleRoot = Join-Path $root 'target/release/bundle'
$tauriTools = Join-Path $env:LOCALAPPDATA 'tauri'

function Step([string]$name) {
    Write-Host ''
    Write-Host "== $name" -ForegroundColor Cyan
}

function Note([string]$text) {
    Write-Host "note: $text" -ForegroundColor Yellow
}

function Fail([string]$text) {
    throw $text
}

# Rules from package.sh that still apply on Windows: a release tag must be
# buildable somewhere, so refuse obvious tool gaps early instead of after a
# five-minute compile.
Step 'toolchain'
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) { Fail 'cargo is not on PATH; install Rust stable from https://rustup.rs' }
$npx = Get-Command npx -ErrorAction SilentlyContinue
if (-not $npx) { Fail 'npx is not on PATH; install Node.js 20+ (the Tauri CLI is not a cargo subcommand here)' }
Write-Host ("cargo: {0}" -f ((& $cargo.Source --version) -join ' '))
Write-Host ("node:  {0}" -f ((& node --version) -join ' '))

$bundleList = @($Bundles.Split(',') | ForEach-Object { $_.Trim().ToLowerInvariant() } | Where-Object { $_ })
if (-not $bundleList) { Fail '-Bundles is empty; use nsis, msi or nsis,msi' }
foreach ($bundle in $bundleList) {
    if ($bundle -notin @('nsis', 'msi')) {
        Fail ("unknown bundle '{0}'; on Windows tauri supports nsis and msi" -f $bundle)
    }
}
Write-Host ("bundles: {0}" -f ($bundleList -join ','))

# Fail on a missing signing key before the compile, not after it.
$signingKey = $env:TAURI_SIGNING_PRIVATE_KEY
$signingKeyPath = $env:TAURI_SIGNING_PRIVATE_KEY_PATH
$hasUpdaterKey = ($signingKey -and $signingKey.Trim()) -or ($signingKeyPath -and (Test-Path $signingKeyPath))
if ($WithUpdater -and -not $hasUpdaterKey) {
    Fail 'WithUpdater needs TAURI_SIGNING_PRIVATE_KEY or TAURI_SIGNING_PRIVATE_KEY_PATH; the updater manifest cannot be signed without it'
}

# 1. The hook shim, in the bundle resources. (package.sh does the same into
#    crates/agi-app/resources/agi-mizar-hook; Windows uses the .exe name and
#    the platform config maps it to the install directory root, next to
#    agi-mizar.exe, which is where agi-boot's shim_path() looks.)
Step 'hook shim'
& cargo build --release -p agi-hook-shim
if ($LASTEXITCODE -ne 0) { Fail "cargo build --release -p agi-hook-shim failed (exit $LASTEXITCODE)" }
$shimBuilt = Join-Path $root 'target/release/agi-mizar-hook.exe'
if (-not (Test-Path $shimBuilt)) { Fail "expected $shimBuilt to exist after the build" }
New-Item -ItemType Directory -Force -Path $resourcesDir | Out-Null
Copy-Item -Path $shimBuilt -Destination $shimResource -Force
Write-Host ("shim: {0} ({1:N0} KiB)" -f $shimResource, ((Get-Item $shimResource).Length / 1KB))

# 2. The packaged llama.cpp runtime. tauri-build validates and copies
#    bundle.resources while compiling agi-app, so both source paths must exist
#    before the Tauri CLI starts; a missing resources/bin fails the build with
#    `resource path ... doesn't exist`, not with a bundler error.
Step 'llama.cpp runtime'
if ($FetchLlama) {
    $fetchArgs = @{}
    if ($Archive) { $fetchArgs['Archive'] = $Archive }
    if ($Proxy) { $fetchArgs['Proxy'] = $Proxy }
    if ($Force) { $fetchArgs['Force'] = $true }
    & (Join-Path $PSScriptRoot 'fetch-llama.ps1') @fetchArgs
}
if (-not (Test-Path $llamaDir)) {
    Fail ('the bundle resource path {0} is missing. Run scripts/fetch-llama.ps1 (or pass -FetchLlama). tauri-build treats a missing resource path as a build error.' -f $llamaDir)
}
if (-not (Test-Path $llamaServer)) {
    Note ("$llamaServer is missing - the installer will not ship the model runtime. Run scripts/fetch-llama.ps1 unless this is a stubbed tree.")
} else {
    $llamaFiles = @(Get-ChildItem $llamaDir -Recurse -File)
    $llamaSize = ($llamaFiles | Measure-Object -Property Length -Sum).Sum
    Write-Host ("llama: {0} files, {1:N1} MiB" -f $llamaFiles.Count, ($llamaSize / 1MB))
}

# 3. Signing prerequisites, reported rather than assumed.
Step 'signing'
$configArgs = @()
if (-not $hasUpdaterKey) {
    Note 'no TAURI_SIGNING_PRIVATE_KEY: updater artifacts (.sig / latest.json) will not be produced for this local build'
    # Deliberately outside the repository: npx on Windows re-parses its command
    # line through cmd.exe, and a repo path with a space ("AGI Mizar") in a
    # --config argument is the kind of thing that quietly breaks there.
    $overridePath = Join-Path ([System.IO.Path]::GetTempPath()) 'agi-mizar-tauri-package-overrides.json'
    $overrideJson = '{"bundle":{"createUpdaterArtifacts":false}}'
    # .NET write keeps the file BOM-free; PowerShell 5.1's Set-Content -Encoding UTF8 does not.
    [System.IO.File]::WriteAllText($overridePath, $overrideJson)
    $configArgs = @('--config', $overridePath)
    Write-Host "updater: disabled for this build via --config $overridePath"
} else {
    Write-Host 'updater: signing key present - updater artifacts will be produced'
}
if ($env:WINDOWS_CERTIFICATE) {
    Note 'WINDOWS_CERTIFICATE is set, but tauri-bundler does not import a PFX from the environment on Windows; see docs/windows.md for the signing setup (it still works unsigned)'
} else {
    Note 'no Windows code-signing certificate configured: the installer is unsigned and SmartScreen will warn on first run (More info -> Run anyway)'
}

# 4. Build. The Tauri CLI is npm, not cargo: package.sh's `cargo tauri` has no
#    equivalent on a clean Windows box.
Step 'tauri build'
if ($bundleList -contains 'msi') {
    if (-not (Test-Path (Join-Path $tauriTools 'WixTools314/candle.exe'))) {
        Note ("WiX is not cached in {0}\tauri\WixTools314; tauri-bundler downloads wix314-binaries.zip from GitHub - if that stalls, place it there manually (see docs/windows.md)" -f $env:LOCALAPPDATA)
    }
}
if ($bundleList -contains 'nsis') {
    if (-not (Test-Path (Join-Path $tauriTools 'NSIS/makensis.exe'))) {
        Note ("NSIS is not cached in {0}\tauri\NSIS; tauri-bundler downloads nsis-3.13.zip from GitHub - if that stalls, place it there manually (see docs/windows.md)" -f $env:LOCALAPPDATA)
    }
}

$cliArgs = @('--yes', '@tauri-apps/cli@2', 'build', '--bundles', ($bundleList -join ',')) + $configArgs
$startedAt = Get-Date
Write-Host ("run: npx {0}" -f ($cliArgs -join ' '))
& npx @cliArgs
$exitCode = $LASTEXITCODE

# 5. Report what actually exists. A non-zero exit with an installer on disk is
#    still a usable local build (the classic case is updater signing), and the
#    reverse - exit 0 with no installer - is not.
Step 'artifacts'
$artifacts = @()
foreach ($kind in @('nsis', 'msi')) {
    $dir = Join-Path $bundleRoot $kind
    if (Test-Path $dir) {
        $artifacts += @(Get-ChildItem -Path $dir -File | Where-Object {
            $_.Extension -in @('.exe', '.msi')
        })
    }
}
if ($artifacts) {
    # Only this run's files count: an older installer from a previous version
    # stays in the bundle directory and must not be reported as if it were just
    # built. "The file exists" is the success test, but it has to be a file this
    # run wrote.
    $artifacts = @($artifacts | Where-Object { $_.LastWriteTime -ge $startedAt.AddSeconds(-2) })
    if (-not $artifacts) {
        Write-Warning ("installers older than this run are present under {0}; ignoring them" -f $bundleRoot)
    }
}
if (-not $artifacts) {
    Fail ("tauri build exited {0} and produced no installer under {1}. Re-run with 'npx --yes @tauri-apps/cli@2 build -v' for the full log." -f $exitCode, $bundleRoot)
}
if ($exitCode -ne 0) {
    Note ("tauri build exited {0}, but installer(s) exist - treating them as the result (usually a missing updater/metadata signature)" -f $exitCode)
}

foreach ($artifact in ($artifacts | Sort-Object LastWriteTime)) {
    $hash = (Get-FileHash -Algorithm SHA256 -Path $artifact.FullName).Hash
    Write-Host ''
    Write-Host ("installer: {0}" -f $artifact.FullName)
    Write-Host ("  size    {0:N1} MiB" -f ($artifact.Length / 1MB))
    Write-Host ("  sha256  {0}" -f $hash)
    Write-Host ("  built   {0}" -f $artifact.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))
}
$signatures = @(Get-ChildItem -Path $bundleRoot -Recurse -File -Filter '*.sig' -ErrorAction SilentlyContinue)
if ($signatures) {
    Write-Host ''
    foreach ($signature in $signatures) { Write-Host ("updater signature: {0}" -f $signature.FullName) }
}

Write-Host ''
if ($exitCode -eq 0) {
    Write-Host 'Built.' -ForegroundColor Green
} else {
    Write-Host ("Built (the Tauri CLI exited {0}; see the notes above)." -f $exitCode) -ForegroundColor Green
}
Write-Host 'Before this installer can be handed out:'
Write-Host '  Windows code signing (optional)  bundle.windows.certificateThumbprint or signCommand, passed via --config.'
Write-Host '                                   Without it SmartScreen warns once per download (More info -> Run anyway).'
Write-Host '  Updater                          TAURI_SIGNING_PRIVATE_KEY for .sig + latest.json; without it this build'
Write-Host '                                   is a local installer only.'
Write-Host '  MSI                              needs the WiX toolset in %LOCALAPPDATA%\tauri\WixTools314; NSIS is the default bundle.'
exit 0
