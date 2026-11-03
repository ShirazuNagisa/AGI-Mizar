<#
.SYNOPSIS
    Install the pinned llama.cpp runtime that the Windows bundle ships.

.DESCRIPTION
    Reads the pinned tag from scripts/llama-version.txt (the same file
    scripts/fetch-llama.sh reads on macOS) and installs the *whole*
    llama-<tag>-bin-win-cpu-x64 archive into crates/agi-app/resources/bin.

    Why the whole directory: on Windows llama-server.exe is a small loader.
    The code lives in llama-server-impl.dll, llama.dll and ggml-*.dll next to
    it, and the loader resolves them from its own directory. Shipping just the
    .exe gives an app whose model sidecar refuses to start.

    The tag is pinned rather than taken from "latest": llama.cpp publishes the
    binaries on rolling bNNNNN tags, and /releases/latest has no assets at all.
    Pinning makes "which llama.cpp is in this release" a one-line answer, and
    the SHA256 table below makes the download verifiable.

.PARAMETER Archive
    Use an already-downloaded llama-<tag>-bin-win-cpu-x64.zip instead of
    downloading. Offline path (CI caches, slow networks). Still hashed.

.PARAMETER Proxy
    Prefix the download URL, e.g. -Proxy https://gh-proxy.com/ for the
    GitHub release CDN. A literal {url} in the value is replaced with the
    real URL instead of prefixed, e.g. -Proxy "https://gh-proxy.com/{url}".

.PARAMETER Stub
    Create only the paths tauri-build needs to compile agi-app, with no
    download: crates/agi-app/resources/bin/ and a 0-byte
    crates/agi-app/resources/agi-mizar-hook.exe placeholder.

    Use this for `cargo clippy` / `cargo test` on a machine that does not
    need to run the model (CI does). A bundle built from a stubbed tree ships
    the placeholder instead of the real runtime, so run the script for real
    before scripts/package.ps1.

.PARAMETER Force
    Download again even when the cached archive is present and verifies.

.EXAMPLE
    scripts/fetch-llama.ps1

.EXAMPLE
    scripts/fetch-llama.ps1 -Archive D:\downloads\llama-b12000-bin-win-cpu-x64.zip

.EXAMPLE
    scripts/fetch-llama.ps1 -Proxy https://gh-proxy.com/

.EXAMPLE
    scripts/fetch-llama.ps1 -Stub
#>
[CmdletBinding()]
param(
    [string]$Archive,
    [string]$Proxy,
    [switch]$Stub,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$versionFile = Join-Path $root 'scripts/llama-version.txt'
$resourcesDir = Join-Path $root 'crates/agi-app/resources'
$outDir = Join-Path $resourcesDir 'bin'
$hookStub = Join-Path $resourcesDir 'agi-mizar-hook.exe'

# sha256 of llama-<tag>-bin-win-cpu-x64.zip. Adding a tag to
# llama-version.txt means adding its hash here; verify it against the archive
# the release actually serves before typing it in.
$SHA256 = @{
    'b12000' = '6FB512A768E6B93C9CED1D01E1F84A3A96BC16B5F0A5948E38FB320D6E067220'
}

$version = (Get-Content $versionFile -Raw).Trim()
if (-not $version) {
    throw "cannot read a llama.cpp tag from $versionFile"
}

if ($Stub) {
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    if (-not (Test-Path $hookStub)) {
        New-Item -ItemType File -Path $hookStub | Out-Null
    }
    Write-Host "stub: $outDir"
    Write-Host "stub: $hookStub (0-byte placeholder; scripts/package.ps1 replaces it with the real shim)"
    Write-Host "stub: no llama.cpp files fetched - do not ship an installer built from this tree"
    exit 0
}

if (-not $SHA256.ContainsKey($version)) {
    throw ("no pinned sha256 for llama.cpp tag '{0}'. Verify the archive against the release and add '{0}' to `$SHA256 in {1}" -f $version, $PSCommandPath)
}
$expected = $SHA256[$version]

$zipName = "llama-$version-bin-win-cpu-x64.zip"
$cacheDir = Join-Path $root 'target/llama-cache'
New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null

if ($Archive) {
    $zipPath = (Resolve-Path $Archive).Path
    Write-Host "archive: $zipPath"
} else {
    $zipPath = Join-Path $cacheDir $zipName
    $download = $true
    if ((Test-Path $zipPath) -and (-not $Force)) {
        $cachedHash = (Get-FileHash -Algorithm SHA256 -Path $zipPath).Hash
        if ($cachedHash -eq $expected) {
            Write-Host "cache: $zipPath (sha256 ok)"
            $download = $false
        } else {
            Write-Warning "cached archive does not match the pinned sha256; replacing it"
            Remove-Item -Path $zipPath -Force
        }
    }
    if ($download) {
        $url = "https://github.com/ggml-org/llama.cpp/releases/download/$version/$zipName"
        if ($Proxy) {
            if ($Proxy -like '*{url}*') {
                $url = $Proxy.Replace('{url}', $url)
            } else {
                if (-not $Proxy.EndsWith('/')) { $Proxy = "$Proxy/" }
                $url = "$Proxy$url"
            }
        }
        Write-Host "download: $url"
        $started = Get-Date
        Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing
        $elapsed = (Get-Date) - $started
        $mib = (Get-Item $zipPath).Length / 1MB
        Write-Host ("download: {0:N1} MiB in {1:N0}s" -f $mib, $elapsed.TotalSeconds)
    }
}

$actual = (Get-FileHash -Algorithm SHA256 -Path $zipPath).Hash
if ($actual -ne $expected) {
    throw ("sha256 mismatch for {0}`n  expected {1}`n  actual   {2}" -f $zipPath, $expected, $actual)
}
Write-Host "sha256: $actual"

$work = Join-Path $cacheDir ("extract-" + [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Force -Path $work | Out-Null
    Write-Host "extract: $zipPath"
    Expand-Archive -Path $zipPath -DestinationPath $work -Force

    $server = Get-ChildItem -Path $work -Recurse -Filter 'llama-server.exe' -File | Select-Object -First 1
    if (-not $server) {
        throw "llama-server.exe is missing from $zipName"
    }
    # Archives are normally flat, but following the file keeps the script
    # working if a future tag nests them under a directory again.
    $binRoot = $server.Directory.FullName

    foreach ($required in @('llama-server-impl.dll', 'llama.dll', 'ggml.dll')) {
        if (-not (Test-Path (Join-Path $binRoot $required))) {
            Write-Warning "$required is missing from the archive; llama-server.exe will not start"
        }
    }

    if (Test-Path $outDir) {
        Remove-Item -Path $outDir -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    Copy-Item -Path (Join-Path $binRoot '*') -Destination $outDir -Recurse -Force
} finally {
    if (Test-Path $work) {
        Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$files = @(Get-ChildItem $outDir -Recurse -File)
$total = ($files | Measure-Object -Property Length -Sum).Sum
Write-Host ("== {0:N1} MiB in {1} ({2} files)" -f ($total / 1MB), $outDir, $files.Count)

Write-Host "== $outDir\llama-server.exe --version"
$reported = & (Join-Path $outDir 'llama-server.exe') --version 2>&1 | Select-Object -First 3
$reported | ForEach-Object { Write-Host "   $_" }
Write-Host "== done"
