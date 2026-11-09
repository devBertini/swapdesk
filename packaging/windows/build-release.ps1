<#
.SYNOPSIS
  Builds the Swapdesk release binaries for Windows and packs them into a zip (stage 39).

.DESCRIPTION
  Needs no administrator rights and touches nothing outside the cargo target folder.

    cargo build --profile release         -p swapdesk-app -p swapdesk-helper   (D15: panic = abort)
    cargo build --profile release-service -p swapdesk-service                  (D15: panic = unwind)

  Then it stages, in <target>\package\Swapdesk\:
    Swapdesk.exe, swapdesk-service.exe, swapdesk-helper.exe, the two licence files, THIRD-PARTY-LICENSES.md, and the install and uninstall
    scripts with this README, so the zip is self-contained. Nothing else is needed at run time: the font is
    embedded in the exe (include_bytes!) and the manifest is linked in by crates\swapdesk-app\build.rs.
  It also writes SHA256SUMS into the folder (lowercase hex, two spaces, file name, one line per other file, LF line
  ends, so `sha256sum -c` reads it); install.ps1 checks every file it installs against it.
  And zips the folder as <target>\package\Swapdesk-windows-x64-<version>.zip (version = workspace.package.version
  in Cargo.toml) and prints the SHA-256 of the zip.

  <target> is %CARGO_TARGET_DIR% when set (use a short folder such as C:\Users\<you>\t39: libopus is built by CMake
  and a deep worktree path overruns MAX_PATH), else <repo>\target.

.PARAMETER SkipBuild
  Stage and zip what the target folder already holds, without running cargo.

.EXAMPLE
  packaging\windows\build-release.cmd
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Set-Location -LiteralPath $Root

# Git Bash is not needed, but cargo lives in ~\.cargo\bin and a fresh shell may not have it on PATH.
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ((Test-Path -LiteralPath $cargoBin) -and ($env:PATH -notlike "*$cargoBin*")) {
    $env:PATH = "$cargoBin;$env:PATH"
}

if ($env:CARGO_TARGET_DIR) {
    $Target = [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
} else {
    $Target = Join-Path $Root 'target'
}

# ---- version from Cargo.toml [workspace.package] ----
$toml = Get-Content -LiteralPath (Join-Path $Root 'Cargo.toml') -Raw
$section = [regex]::Match($toml, '(?ms)^\[workspace\.package\]\s*(.*?)(?=^\[|\z)')
if (-not $section.Success) { throw 'Cargo.toml has no [workspace.package] section.' }
$ver = [regex]::Match($section.Groups[1].Value, '(?m)^\s*version\s*=\s*"([^"]+)"')
if (-not $ver.Success) { throw 'Cargo.toml [workspace.package] has no version.' }
$Version = $ver.Groups[1].Value
Write-Host ("Swapdesk {0}, target folder {1}" -f $Version, $Target)

# ---- build ----
function Invoke-Cargo([string[]]$CargoArgs) {
    Write-Host ("> cargo " + ($CargoArgs -join ' '))
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'   # cargo prints its progress on stderr
    & cargo @CargoArgs
    $code = $LASTEXITCODE
    $ErrorActionPreference = $saved
    if ($code -ne 0) { throw ("cargo {0} failed with exit code {1}" -f ($CargoArgs -join ' '), $code) }
}

if (-not $SkipBuild) {
    Invoke-Cargo @('build', '--profile', 'release', '-p', 'swapdesk-app', '-p', 'swapdesk-helper')
    Invoke-Cargo @('build', '--profile', 'release-service', '-p', 'swapdesk-service')
}

# ---- stage ----
$PackageRoot = Join-Path $Target 'package'
$Stage = Join-Path $PackageRoot 'Swapdesk'
$Zip = Join-Path $PackageRoot ("Swapdesk-windows-x64-{0}.zip" -f $Version)

$files = @(
    @{ From = (Join-Path $Target 'release\Swapdesk.exe');                      Name = 'Swapdesk.exe' },
    @{ From = (Join-Path $Target 'release-service\swapdesk-service.exe');      Name = 'swapdesk-service.exe' },
    @{ From = (Join-Path $Target 'release\swapdesk-helper.exe');               Name = 'swapdesk-helper.exe' },
    @{ From = (Join-Path $Root 'LICENSE-MIT');                                 Name = 'LICENSE-MIT' },
    @{ From = (Join-Path $Root 'LICENSE-APACHE');                              Name = 'LICENSE-APACHE' },
    @{ From = (Join-Path $Root 'THIRD-PARTY-LICENSES.md');                     Name = 'THIRD-PARTY-LICENSES.md' },
    @{ From = (Join-Path $PSScriptRoot 'install.ps1');                         Name = 'install.ps1' },
    @{ From = (Join-Path $PSScriptRoot 'install.cmd');                         Name = 'install.cmd' },
    @{ From = (Join-Path $PSScriptRoot 'uninstall.ps1');                       Name = 'uninstall.ps1' },
    @{ From = (Join-Path $PSScriptRoot 'uninstall.cmd');                       Name = 'uninstall.cmd' },
    @{ From = (Join-Path $PSScriptRoot 'README.md');                           Name = 'README.md' }
)
foreach ($f in $files) {
    if (-not (Test-Path -LiteralPath $f.From -PathType Leaf)) { throw ("missing {0}: build first (or check the target folder)" -f $f.From) }
}

# The stage folder is this script's own scratch under the target folder: only that exact folder is emptied.
if (-not ($Stage -like '*\package\Swapdesk')) { throw "refusing to clear an unexpected folder: $Stage" }
if (Test-Path -LiteralPath $Stage) { Remove-Item -LiteralPath $Stage -Recurse -Force }
New-Item -ItemType Directory -Path $Stage -Force | Out-Null
foreach ($f in $files) { Copy-Item -LiteralPath $f.From -Destination (Join-Path $Stage $f.Name) -Force }

# ---- SHA256SUMS (one line per shipped file; install.ps1 verifies against it) ----
$sumLines = foreach ($f in ($files | Sort-Object { $_.Name })) {
    '{0}  {1}' -f (Get-FileHash -LiteralPath (Join-Path $Stage $f.Name) -Algorithm SHA256).Hash.ToLowerInvariant(), $f.Name
}
$sumsText = ($sumLines -join "`n") + "`n"
[System.IO.File]::WriteAllText((Join-Path $Stage 'SHA256SUMS'), $sumsText, (New-Object System.Text.UTF8Encoding($false)))

# ---- zip ----
if (Test-Path -LiteralPath $Zip) { Remove-Item -LiteralPath $Zip -Force }
Add-Type -AssemblyName System.IO.Compression.FileSystem
# includeBaseDirectory: the zip holds one top folder, Swapdesk\.
[System.IO.Compression.ZipFile]::CreateFromDirectory($Stage, $Zip, [System.IO.Compression.CompressionLevel]::Optimal, $true)

$hash = (Get-FileHash -LiteralPath $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -LiteralPath ($Zip + '.sha256') -Value ("{0} *{1}" -f $hash, (Split-Path -Leaf $Zip)) -Encoding Ascii

Write-Host ''
Write-Host 'Staged files:'
Get-ChildItem -LiteralPath $Stage | ForEach-Object { Write-Host ("  {0,-24} {1,12:N0} bytes" -f $_.Name, $_.Length) }
Write-Host ''
Write-Host 'Zip entries:'
$za = [System.IO.Compression.ZipFile]::OpenRead($Zip)
try { $za.Entries | ForEach-Object { Write-Host ("  {0}  ({1:N0} bytes)" -f $_.FullName, $_.Length) } } finally { $za.Dispose() }
Write-Host ''
Write-Host ("Zip:     {0}" -f $Zip)
Write-Host ("Size:    {0:N0} bytes" -f (Get-Item -LiteralPath $Zip).Length)
Write-Host ("SHA-256: {0}   (of the zip: put it in the release notes)" -f $hash)
