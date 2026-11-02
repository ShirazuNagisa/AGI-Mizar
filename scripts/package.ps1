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
      - Authenticode (the Windows / SmartScreen signature) is applied by this
        script after the bundles are written, with signtool:
        -PfxPath/-PfxPassword (a .pfx file) or -Thumbprint (a certificate in
        CurrentUser\My or LocalMachine\My); -TimestampUrl adds an RFC 3161
        timestamp. Signed installers are then verified with signtool verify /pa
        and Get-AuthenticodeSignature. -SelfSigned creates a temporary
        self-signed code-signing certificate so the sign -> verify chain can be
        exercised on a machine that has no certificate at all; it does not make
        SmartScreen trust the build and it is cleaned up before the script
        exits.

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

.PARAMETER PfxPath
    PFX (PKCS#12) file holding a code-signing certificate and its private key.
    The installers are signed with it after the build; a missing file or a
    wrong password fails the script instead of leaving unsigned installers.

.PARAMETER PfxPassword
    Password for -PfxPath.

.PARAMETER Thumbprint
    SHA1 thumbprint of a code-signing certificate in CurrentUser\My or
    LocalMachine\My, used instead of -PfxPath. LocalMachine\My adds /sm.

.PARAMETER TimestampUrl
    RFC 3161 timestamp server, e.g. http://timestamp.digicert.com. Without it
    the signature is still valid but expires with the certificate.

.PARAMETER SelfSigned
    Create a temporary self-signed code-signing certificate, sign the
    installers with it, verify them, then remove the certificate and temporary
    files. This tests the signing pipeline on a machine with no certificate; it
    does NOT make SmartScreen trust the build (that needs a CA-issued
    certificate and reputation). Cannot be combined with -PfxPath/-Thumbprint.

.EXAMPLE
    scripts/package.ps1 -FetchLlama

.EXAMPLE
    scripts/package.ps1 -Archive D:\downloads\llama-b12000-bin-win-cpu-x64.zip

.EXAMPLE
    scripts/package.ps1 -Bundles nsis,msi

.EXAMPLE
    scripts/package.ps1 -PfxPath C:\certs\agi-mizar.pfx -PfxPassword $env:PFX_PASSWORD -TimestampUrl http://timestamp.digicert.com

.EXAMPLE
    scripts/package.ps1 -Thumbprint 0123456789ABCDEF0123456789ABCDEF01234567 -TimestampUrl http://timestamp.digicert.com

.EXAMPLE
    scripts/package.ps1 -SelfSigned -Bundles nsis,msi

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
    [switch]$Force,
    [string]$PfxPath,
    [string]$PfxPassword,
    [string]$Thumbprint,
    [string]$TimestampUrl,
    [switch]$SelfSigned
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

# signtool ships with the Windows SDK but is not on PATH by default.
function Find-SignTool {
    if ($env:TAURI_WINDOWS_SIGNTOOL_PATH -and (Test-Path -LiteralPath $env:TAURI_WINDOWS_SIGNTOOL_PATH)) {
        return $env:TAURI_WINDOWS_SIGNTOOL_PATH
    }
    $onPath = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    $roots = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'),
        (Join-Path $env:ProgramFiles 'Windows Kits\10\bin')
    )
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        # Newest SDK first; "10.0.26100.0" sorts after "10.0.22621.0".
        foreach ($version in (Get-ChildItem -LiteralPath $root -Directory | Sort-Object Name -Descending)) {
            $candidate = Join-Path $version.FullName 'x64\signtool.exe'
            if (Test-Path -LiteralPath $candidate) { return $candidate }
        }
    }
    return $null
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

# Authenticode request: validated before the compile (a wrong PFX path must not
# cost a five-minute build), applied after the bundles exist.
$signMode = 'none'
$signState = @{ Mode = 'none'; Pfx = $null; Password = $null; Thumb = $null; Machine = $false }
$signTool = $null
if ($SelfSigned -and ($PfxPath -or $Thumbprint)) {
    Fail '-SelfSigned cannot be combined with -PfxPath or -Thumbprint'
}
if ($PfxPath -and $Thumbprint) {
    Fail 'pass either -PfxPath or -Thumbprint, not both'
}
if ($SelfSigned) {
    $signMode = 'selfsigned'
} elseif ($PfxPath) {
    if (-not (Test-Path -LiteralPath $PfxPath)) {
        Fail ("-PfxPath does not exist: {0}" -f $PfxPath)
    }
    $signMode = 'pfx'
    $signState = @{ Mode = 'pfx'; Pfx = (Resolve-Path -LiteralPath $PfxPath).Path; Password = $PfxPassword; Thumb = $null; Machine = $false }
} elseif ($Thumbprint) {
    $signMode = 'thumbprint'
    $normalized = ($Thumbprint -replace '\s', '').ToUpperInvariant()
    $inCurrentUser = Get-ChildItem 'Cert:\CurrentUser\My' -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $normalized }
    if ($inCurrentUser) {
        $signState = @{ Mode = 'thumbprint'; Pfx = $null; Password = $null; Thumb = $normalized; Machine = $false }
    } else {
        $inMachine = Get-ChildItem 'Cert:\LocalMachine\My' -ErrorAction SilentlyContinue |
            Where-Object { $_.Thumbprint -eq $normalized }
        if (-not $inMachine) {
            Fail ("no code-signing certificate with thumbprint {0} in CurrentUser\My or LocalMachine\My" -f $normalized)
        }
        $signState = @{ Mode = 'thumbprint'; Pfx = $null; Password = $null; Thumb = $normalized; Machine = $true }
    }
}
if ($signMode -eq 'none' -and ($PfxPassword -or $TimestampUrl)) {
    Note '-PfxPassword/-TimestampUrl are ignored without -PfxPath, -Thumbprint or -SelfSigned'
}
if ($signMode -ne 'none') {
    $signTool = Find-SignTool
    if (-not $signTool) {
        Fail 'signtool.exe was not found: install the Windows SDK signing tools, put signtool on PATH, or set TAURI_WINDOWS_SIGNTOOL_PATH'
    }
    Write-Host ("signtool: {0}" -f $signTool)
    Write-Host ("authenticode: {0}" -f $signMode)
}

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

# 6. Authenticode. Signing after the bundle exists covers everything the
#    installer will write out, and it is the signature SmartScreen looks at.
#    Failing here is deliberate: being asked to sign and then quietly shipping an
#    unsigned installer is the worst possible outcome.
Step 'authenticode'
$tempSignDir = $null
$tempCert = $null
try {
    if ($signMode -eq 'none') {
        Note 'no Authenticode signing requested: installers stay unsigned and SmartScreen warns on first run'
        Note 'local signing: -PfxPath/-PfxPassword or -Thumbprint; -SelfSigned exercises the pipeline without a certificate'
    } else {
        if ($signMode -eq 'selfsigned') {
            Write-Host ''
            Write-Host '== self-signed test certificate'
            $tempSignDir = Join-Path ([System.IO.Path]::GetTempPath()) ('agi-mizar-sign-' + [guid]::NewGuid().ToString('N'))
            New-Item -ItemType Directory -Force -Path $tempSignDir | Out-Null
            $password = [guid]::NewGuid().ToString('N')
            $secure = ConvertTo-SecureString -String $password -AsPlainText -Force
            $tempCert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=AGI Mizar Test Signing' `
                -CertStoreLocation 'Cert:\CurrentUser\My' -HashAlgorithm SHA256 -KeyLength 2048 `
                -KeyAlgorithm RSA -KeyExportPolicy Exportable -NotAfter (Get-Date).AddDays(1)
            $pfx = Join-Path $tempSignDir 'agi-mizar-test.pfx'
            $cer = Join-Path $tempSignDir 'agi-mizar-test.cer'
            Export-PfxCertificate -Cert $tempCert -FilePath $pfx -Password $secure | Out-Null
            Export-Certificate -Cert $tempCert -FilePath $cer | Out-Null
            # TrustedPeople imports silently. CurrentUser\Root would make
            # Get-AuthenticodeSignature report Valid, but installing a root
            # raises an interactive consent dialog that would hang CI, so this
            # test cannot establish chain trust unattended - see docs/windows.md.
            Import-Certificate -FilePath $cer -CertStoreLocation 'Cert:\CurrentUser\TrustedPeople' | Out-Null
            Write-Host ("certificate: {0}" -f $tempCert.Thumbprint)
            Write-Host ("subject:     {0}" -f $tempCert.Subject)
            Write-Host 'trust:       imported into CurrentUser\TrustedPeople (removed again before exit)'
            Write-Host 'expected:    signature applied and intact; Get-AuthenticodeSignature says UnknownError'
            Write-Host '             because a self-signed certificate is not a trusted root. Only a'
            Write-Host '             CA-issued certificate becomes Valid and SmartScreen-ready.'
            $signState = @{ Mode = 'pfx'; Pfx = $pfx; Password = $password; Thumb = $null; Machine = $false }
        }

        if ($tempCert) {
            $expectedThumb = $tempCert.Thumbprint
        } elseif ($signState.Mode -eq 'thumbprint') {
            $expectedThumb = $signState.Thumb
        } else {
            $expectedThumb = $null
        }

        foreach ($artifact in ($artifacts | Sort-Object LastWriteTime)) {
            Write-Host ''
            Write-Host ("sign: {0}" -f $artifact.FullName)
            $signArgs = @('sign', '/fd', 'SHA256')
            if ($signState.Mode -eq 'thumbprint') {
                $signArgs += @('/sha1', $signState.Thumb)
                if ($signState.Machine) { $signArgs += '/sm' }
            } else {
                $signArgs += @('/f', $signState.Pfx)
                if ($signState.Password) { $signArgs += @('/p', $signState.Password) }
            }
            if ($TimestampUrl) { $signArgs += @('/tr', $TimestampUrl, '/td', 'SHA256') }
            $signArgs += $artifact.FullName
            $signOutput = & $signTool @signArgs 2>&1
            $signCode = $LASTEXITCODE
            $signOutput | ForEach-Object { Write-Host ("  {0}" -f $_) }
            if ($signCode -ne 0) {
                Fail ("signtool sign failed (exit {0}); refusing to report an unsigned bundle" -f $signCode)
            }

            $verifyOutput = & $signTool verify /pa /v $artifact.FullName 2>&1
            $verifyCode = $LASTEXITCODE
            $verifyText = ($verifyOutput | Out-String)
            $signature = Get-AuthenticodeSignature -FilePath $artifact.FullName
            Write-Host ("  status     {0}" -f $signature.Status)
            if ($signature.SignerCertificate) {
                Write-Host ("  subject    {0}" -f $signature.SignerCertificate.Subject)
                Write-Host ("  thumbprint {0}" -f $signature.SignerCertificate.Thumbprint)
            }
            if ($signature.TimeStamperCertificate) {
                Write-Host ("  timestamp  {0}" -f $signature.TimeStamperCertificate.Subject)
            }
            Write-Host ("  sha256     {0}" -f (Get-FileHash -Algorithm SHA256 -Path $artifact.FullName).Hash)
            if (-not $signature.SignerCertificate) {
                Fail ("no Authenticode signature is present on {0} after signing" -f $artifact.FullName)
            }
            if ($expectedThumb -and $signature.SignerCertificate.Thumbprint -ne $expectedThumb) {
                Fail ("signature on {0} is from {1}, expected {2}" -f $artifact.FullName, $signature.SignerCertificate.Thumbprint, $expectedThumb)
            }
            # signtool localizes its messages; the trustworthy, locale-free
            # signal is Get-AuthenticodeSignature: a present signer with
            # UnknownError/NotTrusted means "signature intact, chain not
            # anchored in a trusted root" - exactly the self-signed case.
            if ($verifyCode -eq 0) {
                Write-Host '  signtool   verify /pa ok'
            } elseif (
                $verifyText -match 'terminated in a root\s+certificate which is not trusted' -or
                $signature.Status -in @('UnknownError', 'NotTrusted')
            ) {
                Write-Host '  signtool   verify /pa: chain untrusted (expected without a CA-issued certificate)'
            } else {
                $verifyOutput | Select-Object -Last 6 | ForEach-Object { Write-Host ("  {0}" -f $_) }
                Fail ("signtool verify /pa failed for a reason other than an untrusted chain ({0}); not a usable signature" -f $artifact.FullName)
            }
            switch ($signature.Status) {
                'Valid' { Write-Host '  result     Valid (chain trusted on this machine)' }
                'UnknownError' { Write-Host '  result     signature present and intact; chain not trusted on this machine' }
                'NotTrusted' { Write-Host '  result     signature present; certificate chain not trusted on this machine' }
                default {
                    Fail ("Get-AuthenticodeSignature Status={0} for {1}; expected Valid, UnknownError or NotTrusted" -f $signature.Status, $artifact.FullName)
                }
            }
        }
    }
} finally {
    if ($tempCert) {
        Get-ChildItem 'Cert:\CurrentUser\TrustedPeople' -ErrorAction SilentlyContinue |
            Where-Object { $_.Thumbprint -eq $tempCert.Thumbprint } |
            Remove-Item -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath ('Cert:\CurrentUser\My\' + $tempCert.Thumbprint) -Force -ErrorAction SilentlyContinue
        Write-Host ''
        Write-Host ("cleanup: removed the test certificate {0} from My and TrustedPeople" -f $tempCert.Thumbprint)
    }
    if ($tempSignDir) {
        Remove-Item -LiteralPath $tempSignDir -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host 'cleanup: removed the temporary PFX and certificate files'
    }
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
if ($signMode -eq 'none') {
    Write-Host '  Windows code signing  NOT applied: unsigned installer, SmartScreen warns on first run.'
    Write-Host '                        Use -PfxPath/-PfxPassword or -Thumbprint; a CA-issued certificate'
    Write-Host '                        plus download reputation is what removes the prompt.'
} else {
    Write-Host ("  Windows code signing  {0} signature applied and verified above (see Status)." -f $signMode)
    Write-Host '                        SmartScreen still needs a CA-issued certificate and reputation; a'
    Write-Host '                        self-signed test certificate never removes the warning.'
}
Write-Host '  Updater               TAURI_SIGNING_PRIVATE_KEY for .sig + latest.json; without it this build'
Write-Host '                        is a local installer only.'
Write-Host '  MSI                   installs per-machine (Tauri Wix InstallScope=perMachine): installing needs'
Write-Host '                        an elevated shell; WiX is cached in %LOCALAPPDATA%\tauri\WixTools314.'
exit 0
