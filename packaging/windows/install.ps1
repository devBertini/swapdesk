<#
.SYNOPSIS
  Installs Swapdesk for all users on this PC (per machine, never per user: F-SYS-01, CONTRACTS C2.1).

.DESCRIPTION
  Asks for administrator rights once (UAC), then:
    1. stops the service and any Swapdesk process running from the install folder (an upgrade re-runs this);
    2. creates %ProgramFiles%\Swapdesk and sets its ACL to the shape InstallPolicy::production demands: owner
       Administrators, inheritance off, full control only for SYSTEM, Administrators and TrustedInstaller, read and
       execute for Users;
    3. copies Swapdesk.exe, swapdesk-service.exe, swapdesk-helper.exe, uninstall.cmd and uninstall.ps1 (and the licences) there and checks each
       copy against the SHA-256 in SHA256SUMS (the file the build wrote beside this script; read before the UAC
       prompt). A mismatch or a missing entry aborts and removes what was copied. An existing install folder that
       holds files that are not ours (not in SHA256SUMS) is refused;
    4. registers the service SwapdeskService (display name Swapdesk, LocalSystem, automatic start, recovery: restart
       after 1 s, 5 s and 30 s, counter reset after one day, also for non-crash failures) and the Event Log source
       Swapdesk in the Application log (one event, id 1000, at the end);
    5. adds inbound UDP firewall rules for swapdesk-service.exe: 47820 to 47883 (the session and pairing ports of each
       account's slot, ACCOUNTS.md 5.5) and 5353
       (mDNS discovery), only on the Private and Domain profiles and only from the local subnet;
    6. only with -EnableSas: saves the old SoftwareSASGeneration policy value and sets it to 1 (Services), so the
       service may simulate Ctrl+Alt+Del for UNATTENDED sessions; uninstall restores the saved value, and only when
       this install recorded that it set it. Without -EnableSas the value is not touched;
    7. creates the all-users Start menu shortcut Swapdesk.lnk to Swapdesk.exe;
    8. adds the entry of the installed apps list (Settings > Apps): HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Swapdesk
       with DisplayName, DisplayVersion (the VERSION file of the package, checked against SHA256SUMS), Publisher,
       InstallLocation, DisplayIcon, UninstallString and QuietUninstallString (the uninstall.cmd copied into the
       install folder in step 3), NoModify, NoRepair and EstimatedSize;
    9. starts the service and checks it.
  Run it again to upgrade: the service is stopped, the files are replaced, everything else is made equal again.

.PARAMETER Source
  Folder holding the files to install. Default: the folder of this script when Swapdesk.exe sits beside it (the
  unzipped package), else <target>\package\Swapdesk (what build-release.ps1 stages).

.PARAMETER DryRun
  Prints every action without doing any of it. Needs no administrator rights and does not elevate.

.PARAMETER EnableSas
  Opt-in. Sets HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration = 1, which lets
  services simulate Ctrl+Alt+Del. The number 1 and its effect are unproven until the unattended stage. The old value
  is saved and a marker is stored in HKLM\SOFTWARE\Swapdesk\Installer so uninstall reverts only what this install set.
  Without this switch the value is left untouched and unattended Ctrl+Alt+Del will not work.

.PARAMETER SkipSasPolicy
  Kept for old command lines: SoftwareSASGeneration is untouched by default now. If given together with -EnableSas,
  it wins and the value is left alone.

.PARAMETER SumsB64
  Internal: the elevated copy receives the text of SHA256SUMS (read by the first window before the prompt) here.

.PARAMETER LogFile
  Internal: the elevated copy of this script writes what it prints here so the first window can show it.

.NOTES
  Exit codes: 0 done, 1 failed, 2 installed but the service did not start, 1223 the UAC prompt was cancelled.
  Never reads, prints or copies a secret. No network access.
#>
[CmdletBinding()]
param(
    [string]$Source,
    [switch]$DryRun,
    [switch]$EnableSas,
    [switch]$SkipSasPolicy,
    [string]$LogFile,
    [string]$SumsB64
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------- constants (one place)
$ServiceName    = 'SwapdeskService'
$ServiceDisplay = 'Swapdesk'
$ServiceText    = 'Swapdesk remote desktop service (LAN only): trust store, sessions, helpers.'
$FirewallGroup  = 'Swapdesk'
$RuleSession    = 'Swapdesk-UDP-Session-Pairing'
$RuleMdns       = 'Swapdesk-UDP-mDNS'
$PortsSession   = '47820-47883'      # ACCOUNTS.md 5.5: 32 accounts, a session and a pairing port each
$PortMdns       = 5353
$ShortcutName   = 'Swapdesk.lnk'
$SasKey         = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
$SasValue       = 'SoftwareSASGeneration'
$SasWanted      = 1                      # "Services" (see README: the number is UNPROVEN until the unattended stage runs on this PC)
$OwnKey         = 'HKLM:\SOFTWARE\Swapdesk\Installer'
$Sid_System     = 'S-1-5-18'
$Sid_Admins     = 'S-1-5-32-544'
$Sid_Users      = 'S-1-5-32-545'
$Sid_TrustedInstaller = 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464'
$Required       = @('Swapdesk.exe', 'swapdesk-service.exe', 'swapdesk-helper.exe', 'uninstall.cmd', 'uninstall.ps1')
$VersionName    = 'VERSION'              # in the package and in SHA256SUMS, read for DisplayVersion, never installed
$UninstallKey   = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Swapdesk'   # the installed apps list (64-bit view)
$Publisher      = 'devBertini'
$Optional       = @('LICENSE-MIT', 'LICENSE-APACHE')
$ProcessNames   = @('Swapdesk', 'swapdesk-helper', 'swapdesk-service')
$EventSource    = 'Swapdesk'
$EventLogName   = 'Application'
$SumsName       = 'SHA256SUMS'

# C2.1 reads %ProgramFiles% of the 64-bit service process: never the x86 folder of a 32-bit shell.
$PfBase = $env:ProgramW6432
if (-not $PfBase) { $PfBase = $env:ProgramFiles }
$InstallDir = Join-Path $PfBase 'Swapdesk'
$ServiceExe = Join-Path $InstallDir 'swapdesk-service.exe'
$DataDir    = Join-Path $env:ProgramData 'Swapdesk'
# A path is inside the install folder only when it starts with "<folder>\": Swapdesk-other\ never matches.
$InstallPrefix = $InstallDir.TrimEnd('\') + '\'

# ---------------------------------------------------------------- output
function Say([string]$Text) {
    Write-Host $Text
    if ($LogFile) { try { Add-Content -LiteralPath $LogFile -Value $Text -Encoding UTF8 } catch { } }
}

function Test-Admin {
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    return ([System.Security.Principal.WindowsPrincipal]$id).IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
}

# ---------------------------------------------------------------- source folder and SHA256SUMS (read before elevation)
if ($Source) { $Source = [System.IO.Path]::GetFullPath($Source) }
if (-not $Source) {
    if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'Swapdesk.exe')) {
        $Source = $PSScriptRoot
    } else {
        $targetDir = $env:CARGO_TARGET_DIR
        if (-not $targetDir) { $targetDir = Join-Path (Join-Path $PSScriptRoot '..\..') 'target' }
        $Source = [System.IO.Path]::GetFullPath((Join-Path (Join-Path $targetDir 'package') 'Swapdesk'))
    }
}

# Parses the text of SHA256SUMS ("<64 lowercase hex>  <name>" per line) into a hashtable name -> hash.
function ConvertFrom-Sums([string]$Text) {
    $map = @{}
    foreach ($line in ($Text -split "`r?`n")) {
        if ($line.Trim().Length -eq 0) { continue }
        $m = [regex]::Match($line, '^([0-9a-f]{64})  ([^\\/:*?"<>|]+)$')
        if (-not $m.Success) { throw ("{0}: malformed line: {1}" -f $SumsName, $line) }
        $map[$m.Groups[2].Value] = $m.Groups[1].Value
    }
    if ($map.Count -eq 0) { throw ("{0} is empty" -f $SumsName) }
    return $map
}

$SumsText = $null
$SumsProblem = $null
if ($SumsB64) {
    $SumsText = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($SumsB64))
} else {
    $sumsPath = Join-Path $Source $SumsName
    if (Test-Path -LiteralPath $sumsPath -PathType Leaf) {
        $SumsText = [System.IO.File]::ReadAllText($sumsPath, [System.Text.Encoding]::UTF8)
    } else {
        $SumsProblem = ("{0} is not in the source folder {1} (run build-release.cmd first)" -f $SumsName, $Source)
    }
}
$Sums = $null
if ($null -ne $SumsText) {
    try { $Sums = ConvertFrom-Sums $SumsText } catch { $SumsProblem = $_.Exception.Message }
}
if ($SumsProblem -and -not $DryRun) {
    Write-Host ('Swapdesk install cannot start: ' + $SumsProblem)
    exit 1
}

# ---------------------------------------------------------------- self-elevation (skipped in a dry run)
if (-not $DryRun -and -not (Test-Admin)) {
    $log = Join-Path $env:TEMP ('swapdesk-install-{0}.log' -f [guid]::NewGuid().ToString('N'))
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', ('"{0}"' -f $PSCommandPath), '-LogFile', ('"{0}"' -f $log))
    $argList += @('-Source', ('"{0}"' -f $Source))
    $argList += @('-SumsB64', [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($SumsText)))
    if ($EnableSas) { $argList += '-EnableSas' }
    if ($SkipSasPolicy) { $argList += '-SkipSasPolicy' }
    Write-Host 'Swapdesk install needs administrator rights: approve the Windows (UAC) prompt.'
    try {
        $p = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList $argList -Verb RunAs -Wait -PassThru
    } catch {
        Write-Host 'The elevation was cancelled or refused: nothing was changed.'
        exit 1223
    }
    if (Test-Path -LiteralPath $log) {
        Get-Content -LiteralPath $log | ForEach-Object { Write-Host $_ }
        Remove-Item -LiteralPath $log -Force -ErrorAction SilentlyContinue
    }
    exit $p.ExitCode
}

# ---------------------------------------------------------------- helpers
function Step([string]$Text, [scriptblock]$Do) {
    if ($DryRun) { Say ('[dry-run] would: ' + $Text); return }
    Say ('-> ' + $Text)
    & $Do
}

# Runs a native command, throws with its output when the exit code is not 0 (icacls and sc.exe print on both streams).
function Invoke-Native([string]$Exe, [string[]]$Arguments) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $out = & $Exe @Arguments 2>&1 | Out-String
    $code = $LASTEXITCODE
    $ErrorActionPreference = $saved
    if ($code -ne 0) { throw ("{0} {1} failed with exit code {2}: {3}" -f $Exe, ($Arguments -join ' '), $code, $out.Trim()) }
}

function Get-Svc { Get-Service -Name $ServiceName -ErrorAction SilentlyContinue }

# The values of the installed apps entry: name -> @{ Kind; Value }. Pure (no registry, no disk), so a test can run it.
function New-UninstallEntry([string]$Dir, [string]$Version, [int]$SizeKb, [string]$Date, [string]$PublisherName) {
    $uninstallCmd = Join-Path $Dir 'uninstall.cmd'
    $entry = [ordered]@{
        DisplayName          = @{ Kind = 'String'; Value = 'Swapdesk' }
        DisplayVersion       = @{ Kind = 'String'; Value = $Version }
        Publisher            = @{ Kind = 'String'; Value = $PublisherName }
        InstallLocation      = @{ Kind = 'String'; Value = $Dir }
        DisplayIcon          = @{ Kind = 'String'; Value = ('{0},0' -f (Join-Path $Dir 'Swapdesk.exe')) }
        UninstallString      = @{ Kind = 'String'; Value = ('"{0}"' -f $uninstallCmd) }
        QuietUninstallString = @{ Kind = 'String'; Value = ('"{0}" -Quiet' -f $uninstallCmd) }
        InstallDate          = @{ Kind = 'String'; Value = $Date }
        NoModify             = @{ Kind = 'DWord'; Value = 1 }
        NoRepair             = @{ Kind = 'DWord'; Value = 1 }
        EstimatedSize        = @{ Kind = 'DWord'; Value = $SizeKb }
    }
    return $entry
}

function Get-InstallProcesses {
    $found = @()
    foreach ($p in (Get-Process -ErrorAction SilentlyContinue | Where-Object { $ProcessNames -contains $_.ProcessName })) {
        $path = $null
        try { $path = $p.Path } catch { }
        if ($path -and $path.StartsWith($InstallPrefix, [System.StringComparison]::OrdinalIgnoreCase)) { $found += $p }
    }
    return $found
}

# Write-like access bits (the same set as check_no_other_writers in swapdesk-sys-win): write data, append, write EA,
# delete child, write attributes, delete, change permissions, take ownership, generic write/all, maximum allowed.
$WriteMask = 0x2 -bor 0x4 -bor 0x10 -bor 0x40 -bor 0x100 -bor 0x10000 -bor 0x40000 -bor 0x80000 -bor 0x40000000 -bor 0x10000000 -bor 0x2000000

# Returns the problems of a path against InstallPolicy::production (an empty list means it would pass).
function Test-InstallAcl([string]$Path) {
    $problems = @()
    $trusted = @($Sid_System, $Sid_Admins, $Sid_TrustedInstaller)
    $acl = Get-Acl -LiteralPath $Path
    $owner = $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value
    if ($trusted -notcontains $owner) { $problems += ("{0}: owner {1} is not trusted" -f $Path, $owner) }
    foreach ($ace in $acl.Access) {
        if ($ace.AccessControlType -ne 'Allow') { continue }
        if (([int]$ace.PropagationFlags -band 2) -ne 0) { continue }   # InheritOnly: does not apply to the object itself
        $ref = $ace.IdentityReference
        if ($ref -is [System.Security.Principal.SecurityIdentifier]) { $sid = $ref.Value }
        else { $sid = $ref.Translate([System.Security.Principal.SecurityIdentifier]).Value }
        if ((([int]$ace.FileSystemRights -band $WriteMask) -ne 0) -and ($trusted -notcontains $sid)) {
            $problems += ("{0}: {1} ({2}) has write-like access 0x{3:x}" -f $Path, $sid, $ref.Value, [int]$ace.FileSystemRights)
        }
    }
    return $problems
}

function Show-Acl([string]$Path) {
    $acl = Get-Acl -LiteralPath $Path
    Say ("   ACL of {0}" -f $Path)
    Say ("   owner: {0}" -f $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value)
    Say ("   inheritance from the parent: {0}" -f $(if ($acl.AreAccessRulesProtected) { 'off' } else { 'on' }))
    foreach ($ace in $acl.Access) {
        Say ("   {0,-6} {1,-45} {2} [{3}]" -f $ace.AccessControlType, $ace.IdentityReference.Value, $ace.FileSystemRights, $ace.InheritanceFlags)
    }
    Say ("   SDDL: {0}" -f $acl.Sddl)
}

# ---------------------------------------------------------------- main
$startFailed = $false
try {
    Say ('Swapdesk install{0}' -f $(if ($DryRun) { ' (DRY RUN: nothing is changed)' } else { '' }))
    Say ('  install folder : {0}' -f $InstallDir)
    Say ('  service        : {0} ({1})' -f $ServiceName, $ServiceDisplay)

    if (-not [Environment]::Is64BitOperatingSystem) { throw 'Swapdesk is 64-bit only.' }
    if (-not $DryRun -and -not [Environment]::Is64BitProcess) { throw 'Run this from a 64-bit PowerShell (the 32-bit one sees the wrong Program Files).' }

    # ---- source folder (resolved above, before the elevation) and SHA256SUMS
    Say ('  source folder  : {0}' -f $Source)
    if ($SumsProblem) {
        Say ('[dry-run] WARNING: ' + $SumsProblem + '; the real install would stop here')
    } else {
        Say ('  {0}: {1} entries read before the elevation' -f $SumsName, $Sums.Count)
    }
    $missing = @($Required | Where-Object { -not (Test-Path -LiteralPath (Join-Path $Source $_) -PathType Leaf) })
    if ($missing.Count -gt 0) {
        $msg = 'missing in the source folder: ' + ($missing -join ', ') + ' (run build-release.cmd first)'
        if ($DryRun) { Say ('[dry-run] WARNING: ' + $msg + '; the real install would stop here') } else { throw $msg }
    }
    if ($Sums) {
        $noEntry = @(($Required + $Optional) | Where-Object { (Test-Path -LiteralPath (Join-Path $Source $_) -PathType Leaf) -and -not $Sums.ContainsKey($_) })
        $noEntry += @($Required | Where-Object { -not $Sums.ContainsKey($_) })
        $noEntry = @($noEntry | Select-Object -Unique)
        if ($noEntry.Count -gt 0) {
            $msg = ('{0} has no entry for: {1}' -f $SumsName, ($noEntry -join ', '))
            if ($DryRun) { Say ('[dry-run] WARNING: ' + $msg + '; the real install would stop here') } else { throw $msg }
        }
    }

    # ---- DisplayVersion: the VERSION file, accepted only when SHA256SUMS vouches for it
    $Version = $null
    $versionPath = Join-Path $Source $VersionName
    if (-not (Test-Path -LiteralPath $versionPath -PathType Leaf)) {
        $msg = ('{0} is not in the source folder (run build-release.cmd again)' -f $VersionName)
        if ($DryRun) { Say ('[dry-run] WARNING: ' + $msg + '; the real install would stop here') } else { throw $msg }
    } elseif ($Sums -and -not $Sums.ContainsKey($VersionName)) {
        $msg = ('{0} has no entry for {1}' -f $SumsName, $VersionName)
        if ($DryRun) { Say ('[dry-run] WARNING: ' + $msg + '; the real install would stop here') } else { throw $msg }
    } else {
        $versionText = [System.IO.File]::ReadAllText($versionPath, [System.Text.Encoding]::UTF8).Trim()
        $versionHash = (Get-FileHash -LiteralPath $versionPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($Sums -and $Sums[$VersionName] -ne $versionHash) {
            $msg = ('{0} does not match {1}' -f $VersionName, $SumsName)
            if ($DryRun) { Say ('[dry-run] WARNING: ' + $msg + '; the real install would stop here') } else { throw $msg }
        } elseif ($versionText -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$') {
            $msg = ('{0} does not hold a version number' -f $VersionName)
            if ($DryRun) { Say ('[dry-run] WARNING: ' + $msg + '; the real install would stop here') } else { throw $msg }
        } else {
            $Version = $versionText
            Say ('  version        : {0}' -f $Version)
        }
    }

    # ---- Smart App Control warning (read only)
    try {
        $sac = (Get-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy' -Name 'VerifiedAndReputablePolicyState' -ErrorAction Stop).VerifiedAndReputablePolicyState
        if ($sac -eq 1 -or $sac -eq 2) {
            Say ('  WARNING: Smart App Control is on (state {0}). It blocks unsigned programs: the service or Swapdesk.exe may refuse to start (error 4551 or 1260) until a signed build exists.' -f $sac)
        }
    } catch { }

    # ---- 1. stop what runs from the install folder
    $svc = Get-Svc
    if ($svc) {
        Say ('  existing service: {0} (an upgrade)' -f $svc.Status)
        if ($svc.Status -ne 'Stopped') {
            Step ("Stop-Service {0}" -f $ServiceName) {
                Stop-Service -Name $ServiceName -Force -ErrorAction Stop
                (Get-Svc).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(40))
            }
        }
    } else {
        Say '  no existing service (a first install)'
    }
    foreach ($p in (Get-InstallProcesses)) {
        $proc = $p
        Step ("stop process {0} (pid {1}) running from the install folder" -f $proc.ProcessName, $proc.Id) {
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
            Start-Sleep -Milliseconds 300
        }
    }

    # ---- 1b. refuse an install folder that holds files that are not ours (read only)
    # Ours: the names in SHA256SUMS and the files this script installs. The service writes to %ProgramData%, never here.
    if (Test-Path -LiteralPath $InstallDir) {
        $ours = @{}
        foreach ($k in (@($Required) + @($Optional))) { $ours[$k.ToLowerInvariant()] = $true }
        if ($Sums) { foreach ($k in $Sums.Keys) { $ours[$k.ToLowerInvariant()] = $true } }
        $baseLen = $InstallPrefix.Length
        $foreign = @()
        foreach ($item in (Get-ChildItem -LiteralPath $InstallDir -Recurse -Force -File -ErrorAction Stop)) {
            $rel = $item.FullName.Substring($baseLen)
            if (-not $ours.ContainsKey($rel.ToLowerInvariant())) { $foreign += $rel }
        }
        if ($foreign.Count -gt 0) {
            $msg = ('{0} holds files that are not Swapdesk''s (not in {1}): {2}. Move or delete them, or run uninstall -Force.' -f $InstallDir, $SumsName, ($foreign -join ', '))
            if ($DryRun) { Say ('[dry-run] WARNING: ' + $msg + '; the real install would stop here') } else { throw $msg }
        } else {
            Say ('  install folder content check: only our files in {0}' -f $InstallDir)
        }
    } else {
        Say '  install folder content check: the folder does not exist yet'
    }

    # ---- 2. folder and ACL (before the files, so they inherit it)
    $aclCommands = @(
        @('/setowner', "*$Sid_Admins"),
        @('/reset'),
        @('/inheritance:r'),
        @('/grant:r', "*${Sid_System}:(OI)(CI)F", '/grant:r', "*${Sid_Admins}:(OI)(CI)F", '/grant:r', "*${Sid_TrustedInstaller}:(OI)(CI)F", '/grant:r', "*${Sid_Users}:(OI)(CI)RX")
    )
    if (-not (Test-Path -LiteralPath $InstallDir)) {
        Step ("create folder {0}" -f $InstallDir) { New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null }
    }
    foreach ($cmd in $aclCommands) {
        $argv = @($InstallDir) + $cmd + @('/Q')
        $text = 'icacls ' + (($argv | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' ')
        $arguments = $argv
        Step $text { Invoke-Native 'icacls.exe' $arguments }
    }

    # ---- 3. files (each copy is verified against SHA256SUMS, not against the source)
    $copied = @()
    foreach ($name in ($Required + $Optional)) {
        $from = Join-Path $Source $name
        if (-not (Test-Path -LiteralPath $from -PathType Leaf)) {
            if ($Required -contains $name) { continue }   # dry run only: reported above
            Say ("  (optional file {0} is not in the source: skipped)" -f $name)
            continue
        }
        $to = Join-Path $InstallDir $name
        $n = $name; $f = $from; $d = $to
        Step ("copy {0} -> {1}" -f $from, $to) {
            Copy-Item -LiteralPath $f -Destination $d -Force
            $script:copied += $n
        }
    }
    if ($DryRun) {
        Say ('[dry-run] would: verify the SHA-256 of every copied file in {0} against {1}; on a mismatch or a missing entry, delete what was copied and stop' -f $InstallDir, $SumsName)
    } else {
        $verifyFail = @()
        foreach ($name in $copied) {
            $to = Join-Path $InstallDir $name
            $actual = (Get-FileHash -LiteralPath $to -Algorithm SHA256).Hash.ToLowerInvariant()
            if (-not $Sums.ContainsKey($name)) { $verifyFail += ("{0}: no entry in {1}" -f $name, $SumsName) }
            elseif ($Sums[$name] -ne $actual) { $verifyFail += ("{0}: SHA-256 {1} differs from {2} ({3})" -f $name, $actual, $SumsName, $Sums[$name]) }
            else { Say ("   {0}  sha256 {1}  (matches {2})" -f $name, $actual, $SumsName) }
        }
        if ($verifyFail.Count -gt 0) {
            foreach ($name in $copied) { Remove-Item -LiteralPath (Join-Path $InstallDir $name) -Force -ErrorAction SilentlyContinue }
            throw ("hash check failed, the copied files were removed:`n   " + ($verifyFail -join "`n   "))
        }
    }
    # Files copied over an older file keep its old ACL: give every file the folder's inherited ACL and the same owner.
    Step ("icacls ""{0}\*"" /reset /Q   (every file inherits the folder's ACL)" -f $InstallDir) {
        Invoke-Native 'icacls.exe' @((Join-Path $InstallDir '*'), '/reset', '/Q')
    }
    Step ("icacls ""{0}"" /setowner *{1} /T /C /Q" -f $InstallDir, $Sid_Admins) {
        Invoke-Native 'icacls.exe' @($InstallDir, '/setowner', "*$Sid_Admins", '/T', '/C', '/Q')
    }
    if (-not $DryRun) {
        Say '  ACL check against InstallPolicy::production (folder and every file):'
        Show-Acl $InstallDir
        $bad = @(Test-InstallAcl $InstallDir)
        foreach ($f in (Get-ChildItem -LiteralPath $InstallDir -File -Recurse -Force)) { $bad += @(Test-InstallAcl $f.FullName) }
        if ($bad.Count -gt 0) { throw ("the install folder does not match InstallPolicy::production:`n   " + ($bad -join "`n   ")) }
        Say '  ACL check: OK (owner trusted, no write-like access for anyone else)'
    } else {
        Say ('[dry-run] would: verify with Get-Acl that {0} and each file have a trusted owner and no write-like access for other SIDs' -f $InstallDir)
    }

    # ---- 4. service
    $existing = Get-CimInstance -ClassName Win32_Service -Filter ("Name='{0}'" -f $ServiceName) -ErrorAction SilentlyContinue
    $binPath = '"' + $ServiceExe + '"'
    if (-not $existing) {
        Step ("New-Service {0} -BinaryPathName {1} -DisplayName {2} -StartupType Automatic (LocalSystem)" -f $ServiceName, $binPath, $ServiceDisplay) {
            New-Service -Name $ServiceName -BinaryPathName $binPath -DisplayName $ServiceDisplay -Description $ServiceText -StartupType Automatic | Out-Null
        }
    } else {
        if ($existing.StartName -ne 'LocalSystem') { throw ("the existing service runs as {0}, not LocalSystem: uninstall first" -f $existing.StartName) }
        Step ("update the existing service: path {0}, display name, automatic start" -f $binPath) {
            $r = Invoke-CimMethod -InputObject $existing -MethodName Change -Arguments @{ PathName = $binPath; DisplayName = $ServiceDisplay; StartMode = 'Automatic' }
            if ($r.ReturnValue -ne 0) { throw ("Win32_Service.Change returned {0}" -f $r.ReturnValue) }
            Set-Service -Name $ServiceName -Description $ServiceText
        }
    }
    Step ("sc.exe failure {0} reset= 86400 actions= restart/1000/restart/5000/restart/30000" -f $ServiceName) {
        Invoke-Native 'sc.exe' @('failure', $ServiceName, 'reset=', '86400', 'actions=', 'restart/1000/restart/5000/restart/30000')
    }
    Step ("sc.exe failureflag {0} 1   (recovery also runs when the service exits with an error without crashing)" -f $ServiceName) {
        Invoke-Native 'sc.exe' @('failureflag', $ServiceName, '1')
    }

    # ---- 4b. Event Log source
    $sourceExists = $false
    try { $sourceExists = [System.Diagnostics.EventLog]::SourceExists($EventSource) } catch { }
    if ($sourceExists) {
        Say ("  Event Log source {0} already exists" -f $EventSource)
    } else {
        Step ("New-EventLog -LogName {0} -Source {1}" -f $EventLogName, $EventSource) {
            New-EventLog -LogName $EventLogName -Source $EventSource
        }
    }

    # ---- 5. firewall
    Step ("remove old rules of the group {0}" -f $FirewallGroup) {
        $old = @(Get-NetFirewallRule -Group $FirewallGroup -ErrorAction SilentlyContinue)
        if ($old.Count -gt 0) { $old | Remove-NetFirewallRule }
    }
    Step ("New-NetFirewallRule {0}: inbound UDP {1} for {2}, profiles Private and Domain, remote LocalSubnet" -f $RuleSession, ($PortsSession -join ','), $ServiceExe) {
        New-NetFirewallRule -Name $RuleSession -DisplayName ('Swapdesk session and pairing (UDP {0})' -f ($PortsSession -join ', ')) -Group $FirewallGroup `
            -Direction Inbound -Action Allow -Protocol UDP -LocalPort $PortsSession -Program $ServiceExe -Profile Private, Domain -RemoteAddress LocalSubnet -Enabled True | Out-Null
    }
    Step ("New-NetFirewallRule {0}: inbound UDP {1} (mDNS) for {2}, profiles Private and Domain, remote LocalSubnet" -f $RuleMdns, $PortMdns, $ServiceExe) {
        New-NetFirewallRule -Name $RuleMdns -DisplayName ('Swapdesk discovery (mDNS, UDP {0})' -f $PortMdns) -Group $FirewallGroup `
            -Direction Inbound -Action Allow -Protocol UDP -LocalPort $PortMdns -Program $ServiceExe -Profile Private, Domain -RemoteAddress LocalSubnet -Enabled True | Out-Null
    }

    # ---- 6. SoftwareSASGeneration
    if ($SkipSasPolicy) {
        Say '  SoftwareSASGeneration: left alone (-SkipSasPolicy)'
    } elseif (-not $EnableSas) {
        Say '  SoftwareSASGeneration: left alone. Unattended Ctrl+Alt+Del needs -EnableSas.'
    } else {
        Say '  WARNING: -EnableSas lets services simulate Ctrl+Alt+Del (SoftwareSASGeneration = 1). The setting is unproven until the unattended stage.'
        $policy = Get-Item -LiteralPath $SasKey
        $current = $policy.GetValue($SasValue, $null)
        $hasSaved = (Test-Path -LiteralPath $OwnKey) -and ($null -ne (Get-Item -LiteralPath $OwnKey).GetValue('SasSaved', $null))
        if ($null -ne $current -and $policy.GetValueKind($SasValue) -ne [Microsoft.Win32.RegistryValueKind]::DWord) {
            Say ('  WARNING: {0}\{1} is not a DWORD; left alone' -f $SasKey, $SasValue)
        } else {
            if (-not $hasSaved) {
                $had = [int]($null -ne $current)
                $old = 0
                if ($null -ne $current) { $old = [int]$current }
                Step ("save the old {0} (present: {1}, value: {2}) in {3}" -f $SasValue, $had, $old, $OwnKey) {
                    New-Item -Path $OwnKey -Force | Out-Null
                    New-ItemProperty -Path $OwnKey -Name 'SasSaved' -Value 1 -PropertyType DWord -Force | Out-Null
                    New-ItemProperty -Path $OwnKey -Name 'SasHadValue' -Value $had -PropertyType DWord -Force | Out-Null
                    New-ItemProperty -Path $OwnKey -Name 'SasOldValue' -Value $old -PropertyType DWord -Force | Out-Null
                }
            } else {
                Say ('  the old {0} was saved by an earlier install: kept' -f $SasValue)
            }
            Step ("set {0}\{1} = {2} (Services)" -f $SasKey, $SasValue, $SasWanted) {
                New-ItemProperty -Path $SasKey -Name $SasValue -Value $SasWanted -PropertyType DWord -Force | Out-Null
                New-ItemProperty -Path $OwnKey -Name 'SasSetValue' -Value $SasWanted -PropertyType DWord -Force | Out-Null   # marker: this install set it
            }
        }
    }

    # ---- 7. Start menu shortcut (all users)
    $lnk = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) $ShortcutName
    $uiExe = Join-Path $InstallDir 'Swapdesk.exe'
    Step ("create the all-users Start menu shortcut {0} -> {1}" -f $lnk, $uiExe) {
        $shell = New-Object -ComObject WScript.Shell
        $sc = $shell.CreateShortcut($lnk)
        $sc.TargetPath = $uiExe
        $sc.WorkingDirectory = $InstallDir
        $sc.IconLocation = $uiExe + ',0'
        $sc.Description = 'Swapdesk'
        $sc.Save()
    }

    # ---- 8. installed apps entry (Settings > Apps)
    if ($DryRun) {
        $sizeKb = 0
        $showVersion = $(if ($Version) { $Version } else { '<VERSION>' })
    } else {
        $bytes = [long]0
        foreach ($f in (Get-ChildItem -LiteralPath $InstallDir -File -Recurse -Force)) { $bytes += $f.Length }
        $sizeKb = [int][math]::Ceiling($bytes / 1024.0)
        $showVersion = $Version
    }
    $entry = New-UninstallEntry $InstallDir $showVersion $sizeKb ((Get-Date).ToString('yyyyMMdd')) $Publisher
    Step ("write {0}: {1}" -f $UninstallKey, (($entry.Keys | ForEach-Object { '{0}={1}' -f $_, $entry[$_].Value }) -join '; ')) {
        New-Item -Path $UninstallKey -Force | Out-Null
        foreach ($name in $entry.Keys) {
            New-ItemProperty -Path $UninstallKey -Name $name -Value $entry[$name].Value -PropertyType $entry[$name].Kind -Force | Out-Null
        }
    }

    # ---- 9. start the service
    Step ("Start-Service {0}" -f $ServiceName) {
        try {
            Start-Service -Name $ServiceName -ErrorAction Stop
            (Get-Svc).WaitForStatus('Running', [TimeSpan]::FromSeconds(40))
        } catch {
            $script:startFailed = $true
            Say ("   the service did not start: {0}" -f $_.Exception.Message)
            Say '   If Smart App Control or App Control blocked the exe (error 1260 or 4551), see README.md, "Smart App Control".'
            $trace = Join-Path $DataDir 'trace.log'
            if (Test-Path -LiteralPath $trace) {
                Say ("   last lines of {0} (the trace log never holds secrets, CONTRACTS C2.3):" -f $trace)
                Get-Content -LiteralPath $trace -Tail 12 | ForEach-Object { Say ('     ' + $_) }
            }
        }
    }

    # ---- Event Log entry
    $eventText = ('Swapdesk installed from {0} into {1}. Service {2}{3}. SoftwareSASGeneration {4}.' -f $Source, $InstallDir, $ServiceName,
        $(if ($startFailed) { ' did not start' } else { ' started' }), $(if ($EnableSas -and -not $SkipSasPolicy) { 'set by -EnableSas' } else { 'untouched' }))
    Step ("Write-EventLog -LogName {0} -Source {1} -EventId 1000 (install)" -f $EventLogName, $EventSource) {
        try { Write-EventLog -LogName $EventLogName -Source $EventSource -EventId 1000 -EntryType Information -Message $eventText }
        catch { Say ("   WARNING: the Event Log entry was not written: {0}" -f $_.Exception.Message) }
    }

    # ---- report
    Say ''
    if ($DryRun) {
        Say 'Dry run finished: nothing was changed.'
    } else {
        Say 'State now:'
        $s = Get-Svc
        $cim = Get-CimInstance -ClassName Win32_Service -Filter ("Name='{0}'" -f $ServiceName)
        Say ("  service  : {0} / {1} / start {2} / account {3} / pid {4}" -f $s.Name, $s.Status, $cim.StartMode, $cim.StartName, $cim.ProcessId)
        Say ("  binary   : {0}" -f $cim.PathName)
        foreach ($r in (Get-NetFirewallRule -Group $FirewallGroup -ErrorAction SilentlyContinue)) {
            $pf = $r | Get-NetFirewallPortFilter
            $af = $r | Get-NetFirewallAddressFilter
            Say ("  firewall : {0} enabled={1} profile={2} {3} {4} remote={5}" -f $r.Name, $r.Enabled, $r.Profile, $pf.Protocol, ($pf.LocalPort -join ','), ($af.RemoteAddress -join ','))
        }
        $sasNow = (Get-Item -LiteralPath $SasKey).GetValue($SasValue, $null)
        Say ("  {0} = {1}" -f $SasValue, $(if ($null -eq $sasNow) { '(absent)' } else { $sasNow }))
        Say ("  shortcut : {0} ({1})" -f $lnk, $(if (Test-Path -LiteralPath $lnk) { 'present' } else { 'MISSING' }))
        Say ("  apps list: {0} ({1})" -f $UninstallKey, $(if (Test-Path -LiteralPath $UninstallKey) { 'present' } else { 'MISSING' }))
        Start-Sleep -Seconds 3
        $helpers = @(Get-Process -Name 'swapdesk-helper' -ErrorAction SilentlyContinue)
        Say ("  helpers  : {0} swapdesk-helper process(es) running (two expected: system and user role)" -f $helpers.Count)
        Say ''
        if ($startFailed) {
            Say 'Installed, but the service is not running. Fix the cause above, then run install again (or Start-Service SwapdeskService).'
        } else {
            Say 'Swapdesk is installed. Start it from the Start menu: Swapdesk.'
        }
    }
} catch {
    Say ''
    Say ('FAILED: {0}' -f $_.Exception.Message)
    Say 'The install stopped midway. Nothing is rolled back: run install again (it is safe to repeat) or run uninstall.'
    exit 1
}
if ($startFailed) { exit 2 }
exit 0
