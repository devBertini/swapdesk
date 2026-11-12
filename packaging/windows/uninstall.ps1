<#
.SYNOPSIS
  Removes Swapdesk from this PC and checks that nothing is left (F-SYS-01).

.DESCRIPTION
  Asks for administrator rights once (UAC), then:
    1. stops the service SwapdeskService and any Swapdesk process running from the install folder;
    2. deletes the service;
    3. removes the firewall rules of the group Swapdesk;
    4. removes the all-users Start menu shortcut and the installed apps entry (HKLM\...\Uninstall\Swapdesk); for every user
       profile on this PC it removes the per-user Run value Swapdesk ("open at login": a hive that is not loaded is
       loaded, changed and unloaded) and the user's logs, %LOCALAPPDATA%\Swapdesk\logs (the folder Swapdesk\ goes
       when it is empty);
    5. restores SoftwareSASGeneration to the value saved by install.ps1 (or removes it when it was absent), only when
       the install recorded that it set it (install -EnableSas) and unless someone changed it since (then it is left
       and reported); removes HKLM\SOFTWARE\Swapdesk;
    6. deletes the files install.ps1 put in %ProgramFiles%\Swapdesk (this script and uninstall.cmd included: run from
       that folder, the elevated copy first copies itself to an administrator-only folder under %WINDIR%\Temp and
       runs from there), then the folder if it is empty;
    7. deletes the data folder %ProgramData%\Swapdesk (identity, trust store, settings, audit and trace logs) unless
       -KeepData is given; the updater's folder update inside it is removed whole. Only the names CONTRACTS C5.2 lists are deleted (with their .1, .tmp and .damaged-* copies),
       then the folder if it is empty. A folder that is a reparse point is never entered.
  Then it writes one Application event (id 1001), removes the Event Log source Swapdesk, and queries the service, the rules, the folders, the shortcut, the registry key and the processes, and exits
  with 1 when something is left.

.PARAMETER KeepData
  Keeps %ProgramData%\Swapdesk (this PC stays paired with its devices). Default: the data is removed.

.PARAMETER Force
  Deletes the install folder and the data folder with everything in them, also files this script does not know.

.PARAMETER DryRun
  Prints every action without doing any of it. Needs no administrator rights and does not elevate.

.PARAMETER Quiet
  Accepted for the installed apps entry's QuietUninstallString. There is no prompt to hide; it changes nothing.

.PARAMETER Relocated
  Internal: this is the copy that the elevated script started from %WINDIR%\Temp so the install folder can be deleted.

.PARAMETER LogFile
  Internal: the elevated copy of this script writes what it prints here so the first window can show it.

.NOTES
  Exit codes: 0 clean, 1 failed or something left, 1223 the UAC prompt was cancelled.
  Never reads, prints or copies a secret (the data files are deleted, never opened). No network access.
#>
[CmdletBinding()]
param(
    [switch]$KeepData,
    [switch]$Force,
    [switch]$DryRun,
    [switch]$Quiet,
    [switch]$Relocated,
    [string]$LogFile
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------- constants (keep equal to install.ps1)
$ServiceName   = 'SwapdeskService'
$FirewallGroup = 'Swapdesk'
$RuleNames     = @('Swapdesk-UDP-Session-Pairing', 'Swapdesk-UDP-mDNS')
$ShortcutName  = 'Swapdesk.lnk'
$SasKey        = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
$SasValue      = 'SoftwareSASGeneration'
$OwnRoot       = 'HKLM:\SOFTWARE\Swapdesk'
$OwnKey        = 'HKLM:\SOFTWARE\Swapdesk\Installer'
$InstalledFiles = @('Swapdesk.exe', 'swapdesk-service.exe', 'swapdesk-helper.exe', 'LICENSE-MIT', 'LICENSE-APACHE', 'uninstall.cmd', 'uninstall.ps1')
$UninstallKey  = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Swapdesk'
$ProfileList   = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList'
$RunKeyPath    = 'Software\Microsoft\Windows\CurrentVersion\Run'   # under each user's hive
$RunValue      = 'Swapdesk'
$TempHive      = 'SwapdeskUninstall'
# CONTRACTS C5.2: identity.bin, trust.bin, settings.bin, audit-sessions.log, audit-network.log, trace.log
# (rotation keeps <name>.1; a file that does not decode is set aside as <name>.damaged-<ms>; writes go through <name>.tmp).
$DataFiles     = @('identity.bin', 'trust.bin', 'settings.bin', 'audit-sessions.log', 'audit-network.log', 'trace.log', 'accounts.bin')
# ACCOUNTS.md 10.6: each account's folder accounts\<SID> holds these.
$AccountFiles  = @('identity.bin', 'trust.bin', 'settings.bin', 'account.bin', 'peers.bin', 'audit-sessions.log', 'audit-network.log')
$ProcessNames  = @('Swapdesk', 'swapdesk-helper', 'swapdesk-service')
$EventSource   = 'Swapdesk'
$EventLogName  = 'Application'

$PfBase = $env:ProgramW6432
if (-not $PfBase) { $PfBase = $env:ProgramFiles }
$InstallDir = Join-Path $PfBase 'Swapdesk'
$DataDir    = Join-Path $env:ProgramData 'Swapdesk'
$Shortcut   = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) $ShortcutName
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

# ---------------------------------------------------------------- self-elevation (skipped in a dry run)
if (-not $DryRun -and -not (Test-Admin)) {
    $log = Join-Path $env:TEMP ('swapdesk-uninstall-{0}.log' -f [guid]::NewGuid().ToString('N'))
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', ('"{0}"' -f $PSCommandPath), '-LogFile', ('"{0}"' -f $log))
    if ($KeepData) { $argList += '-KeepData' }
    if ($Force) { $argList += '-Force' }
    if ($Quiet) { $argList += '-Quiet' }
    Write-Host 'Swapdesk uninstall needs administrator rights: approve the Windows (UAC) prompt.'
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

# ---------------------------------------------------------------- relocation (the install folder cannot delete the script that runs in it)
# Elevated, started from the install folder: copy this script to a folder only SYSTEM and Administrators can use, run
# that copy to do the work, then remove the folder. The copy lives under %WINDIR%\Temp, never a user's Temp.
if (-not $DryRun -and -not $Relocated -and (($PSScriptRoot.TrimEnd('\') + '\').StartsWith($InstallPrefix, [System.StringComparison]::OrdinalIgnoreCase))) {
    $work = Join-Path $env:windir ('Temp\swapdesk-uninstall-{0}' -f [guid]::NewGuid().ToString('N'))
    $code = 1
    try {
        New-Item -ItemType Directory -Path $work -Force | Out-Null
        & icacls.exe $work '/inheritance:r' '/grant:r' '*S-1-5-18:(OI)(CI)F' '/grant:r' '*S-1-5-32-544:(OI)(CI)F' '/Q' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw ('icacls failed on {0} with exit code {1}' -f $work, $LASTEXITCODE) }
        $copy = Join-Path $work 'uninstall.ps1'
        Copy-Item -LiteralPath $PSCommandPath -Destination $copy -Force
        $childArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $copy, '-Relocated')
        if ($KeepData) { $childArgs += '-KeepData' }
        if ($Force) { $childArgs += '-Force' }
        if ($Quiet) { $childArgs += '-Quiet' }
        if ($LogFile) { $childArgs += @('-LogFile', $LogFile) }
        Set-Location -LiteralPath $env:windir
        & (Join-Path $PSHOME 'powershell.exe') @childArgs
        $code = $LASTEXITCODE
    } catch {
        Say ('FAILED: {0}' -f $_.Exception.Message)
        $code = 1
    } finally {
        Set-Location -LiteralPath $env:windir
        if ($work -like '*\Temp\swapdesk-uninstall-*') { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
    }
    exit $code
}

# ---------------------------------------------------------------- helpers
function Step([string]$Text, [scriptblock]$Do) {
    if ($DryRun) { Say ('[dry-run] would: ' + $Text); return }
    Say ('-> ' + $Text)
    & $Do
}

function Invoke-Native([string]$Exe, [string[]]$Arguments) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $out = & $Exe @Arguments 2>&1 | Out-String
    $code = $LASTEXITCODE
    $ErrorActionPreference = $saved
    if ($code -ne 0) { throw ("{0} {1} failed with exit code {2}: {3}" -f $Exe, ($Arguments -join ' '), $code, $out.Trim()) }
}

function Get-Svc { Get-Service -Name $ServiceName -ErrorAction SilentlyContinue }

function Test-ReparsePoint([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    return ($null -ne $item) -and (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)
}

function Get-RunningFromInstallDir {
    $found = @()
    foreach ($p in (Get-Process -ErrorAction SilentlyContinue | Where-Object { $ProcessNames -contains $_.ProcessName })) {
        $path = $null
        try { $path = $p.Path } catch { }
        # UPDATES.md 6.2: an update applier runs a copy of the service from the data folder's update\applier.
        $applierPrefix = (Join-Path $DataDir 'update\applier') + '\'
        if ($path -and ($path.StartsWith($InstallPrefix, [System.StringComparison]::OrdinalIgnoreCase) -or $path.StartsWith($applierPrefix, [System.StringComparison]::OrdinalIgnoreCase))) { $found += $p }
    }
    return $found
}

# Deletes the known files of a folder (no recursion, never through a reparse point), then the folder if it is empty.
# With -Everything the folder goes with all it holds. Returns the list of names that stayed.
function Remove-KnownFolder([string]$Dir, [string[]]$Names, [bool]$Everything) {
    if (-not (Test-Path -LiteralPath $Dir)) { return @() }
    if (Test-ReparsePoint $Dir) {
        Say ("   WARNING: {0} is a reparse point (a link): not entered, not deleted. Remove it by hand after checking where it points." -f $Dir)
        return @($Dir)
    }
    if ($Everything) {
        Remove-Item -LiteralPath $Dir -Recurse -Force
        return @()
    }
    foreach ($file in (Get-ChildItem -LiteralPath $Dir -Force -File)) {
        $known = $false
        foreach ($n in $Names) {
            if ($file.Name -eq $n -or $file.Name -eq ($n + '.1') -or $file.Name -eq ($n + '.tmp') -or $file.Name.StartsWith($n + '.damaged-', [System.StringComparison]::Ordinal)) { $known = $true }
        }
        if ($known) {
            Remove-Item -LiteralPath $file.FullName -Force
            Say ("   deleted {0}" -f $file.FullName)
        }
    }
    $left = @(Get-ChildItem -LiteralPath $Dir -Force | ForEach-Object { $_.Name })
    if ($left.Count -eq 0) {
        Remove-Item -LiteralPath $Dir -Force
        Say ("   removed the empty folder {0}" -f $Dir)
    }
    return $left
}

# The real accounts on this PC (local, domain, Microsoft, Entra): SID and profile folder. System and service profiles are skipped.
function Get-UserProfiles {
    $found = @()
    foreach ($k in (Get-ChildItem -LiteralPath $ProfileList -ErrorAction SilentlyContinue)) {
        $sid = $k.PSChildName
        if ($sid -notmatch '^S-1-(5-21|12-1)-[0-9-]+$') { continue }
        $path = $k.GetValue('ProfileImagePath', $null)
        if ($path) { $found += [pscustomobject]@{ Sid = $sid; Path = [Environment]::ExpandEnvironmentVariables([string]$path) } }
    }
    return $found
}

# Removes the value Swapdesk of one user's Run key. A hive that is loaded (the user is signed in) is used as it is;
# otherwise NTUSER.DAT is loaded under HKU\SwapdeskUninstall and unloaded again. Returns $null when done, or a text.
function Remove-UserRunValue($UserProfile) {
    $loaded = Test-Path -LiteralPath ('Registry::HKEY_USERS\' + $UserProfile.Sid)
    $root = 'Registry::HKEY_USERS\' + $UserProfile.Sid
    $mounted = $false
    try {
        if (-not $loaded) {
            $dat = Join-Path $UserProfile.Path 'NTUSER.DAT'
            if (-not (Test-Path -LiteralPath $dat -PathType Leaf)) { return $null }
            Invoke-Native 'reg.exe' @('load', ('HKU\' + $TempHive), $dat)
            $mounted = $true
            $root = 'Registry::HKEY_USERS\' + $TempHive
        }
        $key = $root + '\' + $RunKeyPath
        if ((Test-Path -LiteralPath $key) -and ($null -ne (Get-Item -LiteralPath $key).GetValue($RunValue, $null))) {
            Remove-ItemProperty -LiteralPath $key -Name $RunValue -ErrorAction Stop
            Say ("   removed the Run value {0} of {1}" -f $RunValue, $UserProfile.Sid)
        }
        return $null
    } catch {
        return ("the Run value of {0} could not be removed: {1}" -f $UserProfile.Sid, $_.Exception.Message)
    } finally {
        if ($mounted) {
            [System.GC]::Collect()
            [System.GC]::WaitForPendingFinalizers()
            try { Invoke-Native 'reg.exe' @('unload', ('HKU\' + $TempHive)) } catch { Say ("   WARNING: {0}" -f $_.Exception.Message) }
        }
    }
}

# ---------------------------------------------------------------- main
$leftovers = @()
try {
    Say ('Swapdesk uninstall{0}' -f $(if ($DryRun) { ' (DRY RUN: nothing is changed)' } else { '' }))
    Say ('  install folder : {0}' -f $InstallDir)
    Say ('  data folder    : {0} ({1})' -f $DataDir, $(if ($KeepData) { 'kept: -KeepData' } else { 'removed' }))

    # ---- 1. stop
    $svc = Get-Svc
    if ($svc -and $svc.Status -ne 'Stopped') {
        Step ("Stop-Service {0}" -f $ServiceName) {
            Stop-Service -Name $ServiceName -Force -ErrorAction Stop
            (Get-Svc).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(40))
        }
    } elseif ($svc) {
        Say '  the service is already stopped'
    } else {
        Say '  no service named SwapdeskService'
    }
    foreach ($p in (Get-RunningFromInstallDir)) {
        $proc = $p
        Step ("stop process {0} (pid {1}) running from the install folder" -f $proc.ProcessName, $proc.Id) {
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
            Start-Sleep -Milliseconds 300
        }
    }

    # ---- 2. delete the service
    if ($svc) {
        Step ("sc.exe delete {0}" -f $ServiceName) {
            Invoke-Native 'sc.exe' @('delete', $ServiceName)
            # The SCM removes it when the last handle closes; wait for that.
            $until = (Get-Date).AddSeconds(20)
            while ((Get-Svc) -and (Get-Date) -lt $until) { Start-Sleep -Milliseconds 500 }
        }
    }

    # ---- 3. firewall
    $rules = @(Get-NetFirewallRule -Group $FirewallGroup -ErrorAction SilentlyContinue)
    foreach ($n in $RuleNames) {
        if (-not ($rules | Where-Object { $_.Name -eq $n })) {
            $r = Get-NetFirewallRule -Name $n -ErrorAction SilentlyContinue
            if ($r) { $rules += $r }
        }
    }
    if ($rules.Count -gt 0) {
        $names = ($rules | ForEach-Object { $_.Name }) -join ', '
        $toRemove = $rules
        Step ("Remove-NetFirewallRule {0}" -f $names) { $toRemove | Remove-NetFirewallRule }
    } else {
        Say '  no Swapdesk firewall rules'
    }

    # ---- 4. shortcut
    if (Test-Path -LiteralPath $Shortcut) {
        Step ("delete the shortcut {0}" -f $Shortcut) { Remove-Item -LiteralPath $Shortcut -Force }
    } else {
        Say '  no Start menu shortcut'
    }

    # ---- 4b. installed apps entry
    if (Test-Path -LiteralPath $UninstallKey) {
        Step ("remove the installed apps entry {0}" -f $UninstallKey) { Remove-Item -LiteralPath $UninstallKey -Recurse -Force }
    } else {
        Say '  no installed apps entry'
    }

    # ---- 4c. each user's Run value ("open at login") and logs
    foreach ($u in (Get-UserProfiles)) {
        $user = $u
        $logsDir = Join-Path $user.Path 'AppData\Local\Swapdesk\logs'
        $logsParent = Join-Path $user.Path 'AppData\Local\Swapdesk'
        if ($DryRun) {
            Say ('[dry-run] would: remove the Run value {0} of {1} and the folder {2} (then {3} if empty)' -f $RunValue, $user.Sid, $logsDir, $logsParent)
            continue
        }
        $problem = Remove-UserRunValue $user
        if ($problem) { $leftovers += $problem; Say ('   WARNING: ' + $problem) }
        if (Test-Path -LiteralPath $logsDir) {
            Say ("-> delete the user's logs {0}" -f $logsDir)
            foreach ($l in @(Remove-KnownFolder $logsDir @() $true)) { $leftovers += ("{0}: not deleted" -f $l) }
        }
        if (Test-Path -LiteralPath $logsParent) { [void](Remove-KnownFolder $logsParent @() $false) }
    }

    # ---- 5. SoftwareSASGeneration
    if (Test-Path -LiteralPath $OwnKey) {
        $own = Get-Item -LiteralPath $OwnKey
        $saved = $own.GetValue('SasSaved', $null)
        $marker = $own.GetValue('SasSetValue', $null)   # written by install -EnableSas only, after it set the value
        if ($null -ne $saved -and $null -ne $marker) {
            $had = [int]$own.GetValue('SasHadValue', 0)
            $old = [int]$own.GetValue('SasOldValue', 0)
            $ours = [int]$own.GetValue('SasSetValue', 1)
            $now = (Get-Item -LiteralPath $SasKey).GetValue($SasValue, $null)
            if ($null -ne $now -and [int]$now -ne $ours) {
                Say ("  {0} is {1} now, not the {2} install.ps1 set: left alone" -f $SasValue, $now, $ours)
            } elseif ($had -eq 1) {
                Step ("restore {0}\{1} = {2} (the value before the install)" -f $SasKey, $SasValue, $old) {
                    New-ItemProperty -Path $SasKey -Name $SasValue -Value $old -PropertyType DWord -Force | Out-Null
                }
            } else {
                Step ("remove {0}\{1} (it did not exist before the install)" -f $SasKey, $SasValue) {
                    Remove-ItemProperty -Path $SasKey -Name $SasValue -ErrorAction SilentlyContinue
                }
            }
        } else {
            Say '  the install did not set SoftwareSASGeneration (no -EnableSas marker): left alone'
        }
        Step ("remove the registry key {0}" -f $OwnRoot) { Remove-Item -LiteralPath $OwnRoot -Recurse -Force }
    } else {
        Say ('  no saved SoftwareSASGeneration value ({0} absent): policy left alone' -f $OwnKey)
    }

    # ---- 5b. Event Log: one event, then the source goes
    $sourceExists = $false
    try { $sourceExists = [System.Diagnostics.EventLog]::SourceExists($EventSource) } catch { }
    if ($sourceExists) {
        Step ("Write-EventLog -LogName {0} -Source {1} -EventId 1001 (uninstall)" -f $EventLogName, $EventSource) {
            try { Write-EventLog -LogName $EventLogName -Source $EventSource -EventId 1001 -EntryType Information -Message ('Swapdesk uninstalled from {0}.{1}' -f $InstallDir, $(if ($KeepData) { ' Data kept.' } else { ' Data removed.' })) }
            catch { Say ("   WARNING: the Event Log entry was not written: {0}" -f $_.Exception.Message) }
        }
        Step ("Remove-EventLog -Source {0}" -f $EventSource) { Remove-EventLog -Source $EventSource }
    } else {
        Say ('  no Event Log source {0}' -f $EventSource)
    }

    # ---- 6. install folder
    if (Test-Path -LiteralPath $InstallDir) {
        $installDirLeft = @()
        if ($DryRun) {
            Say ('[dry-run] would: delete {0} from {1}{2}, then the folder if empty' -f ($InstalledFiles -join ', '), $InstallDir, $(if ($Force) { ' (-Force: the whole folder)' } else { '' }))
        } else {
            Say ("-> delete the installed files in {0}" -f $InstallDir)
            $installDirLeft = @(Remove-KnownFolder $InstallDir $InstalledFiles ([bool]$Force))
            foreach ($l in $installDirLeft) { $leftovers += ("{0}: {1}" -f $InstallDir, $l) }
        }
    } else {
        Say '  no install folder'
    }

    # ---- 7. data
    if ($KeepData) {
        Say ('  data kept: {0}' -f $DataDir)
    } elseif (Test-Path -LiteralPath $DataDir) {
        $accountsDir = Join-Path $DataDir 'accounts'
        if ($DryRun) {
            Say ('[dry-run] would: delete {0} (with .1, .tmp, .damaged-* copies) from each folder {1}\<SID>, then each folder and {1} if empty' -f ($AccountFiles -join ', '), $accountsDir)
            Say ('[dry-run] would: delete the update folder {0}\update (the updater''s state, journal, staging, rollback and applier)' -f $DataDir)
            Say ('[dry-run] would: delete {0} (with .1, .tmp, .damaged-* copies) from {1}{2}, then the folder if empty' -f ($DataFiles -join ', '), $DataDir, $(if ($Force) { ' (-Force: the whole folder)' } else { '' }))
        } else {
            # ACCOUNTS.md 10.6: the accounts first, each by the names it holds; a link is never entered.
            if (-not $Force -and (Test-Path -LiteralPath $accountsDir)) {
                if (Test-ReparsePoint $accountsDir) {
                    $leftovers += @(Remove-KnownFolder $accountsDir @() $false)
                } else {
                    foreach ($acct in (Get-ChildItem -LiteralPath $accountsDir -Force -Directory | Where-Object { $_.Name -match '^S-1-[0-9-]+$' })) {
                        Say ("-> delete the account files in {0}" -f $acct.FullName)
                        foreach ($l in @(Remove-KnownFolder $acct.FullName $AccountFiles $false)) { $leftovers += ("{0}: {1}" -f $acct.FullName, $l) }
                    }
                    foreach ($l in @(Remove-KnownFolder $accountsDir @() $false)) { $leftovers += ("{0}: {1}" -f $accountsDir, $l) }
                }
            }
            # UPDATES.md 6.3: the folder update holds only the updater's own files (its state, the journal, a staged or
            # rolled-back version, the applier and its log): removed whole, a link never entered.
            $updateDir = Join-Path $DataDir 'update'
            if (Test-Path -LiteralPath $updateDir) {
                Say ("-> delete the update folder {0}" -f $updateDir)
                foreach ($l in @(Remove-KnownFolder $updateDir @() $true)) { $leftovers += ("{0}: {1}" -f $updateDir, $l) }
            }
            Say ("-> delete the data files in {0}" -f $DataDir)
            $dataLeft = @(Remove-KnownFolder $DataDir $DataFiles ([bool]$Force))
            foreach ($l in $dataLeft) { $leftovers += ("{0}: {1}" -f $DataDir, $l) }
        }
    } else {
        Say '  no data folder'
    }

    # ---- checks
    Say ''
    if ($DryRun) {
        Say 'Dry run finished: nothing was changed.'
    } else {
        Say 'Check that nothing is left:'
        $problems = @()
        if (Get-Svc) { $problems += 'the service SwapdeskService still exists (marked for deletion until every handle to it closes: close services.msc, then check again)' }
        $savedEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        & sc.exe query $ServiceName *> $null
        $scCode = $LASTEXITCODE            # 1060 = "the specified service does not exist as an installed service"
        $ErrorActionPreference = $savedEap
        if ($scCode -ne 1060) { $problems += ("sc.exe query {0} exited with {1}, not 1060 (service does not exist)" -f $ServiceName, $scCode) }
        if (@(Get-NetFirewallRule -Group $FirewallGroup -ErrorAction SilentlyContinue).Count -gt 0) { $problems += 'firewall rules of the group Swapdesk remain' }
        foreach ($n in $RuleNames) { if (Get-NetFirewallRule -Name $n -ErrorAction SilentlyContinue) { $problems += ("firewall rule {0} remains" -f $n) } }
        if (Test-Path -LiteralPath $Shortcut) { $problems += ("shortcut {0} remains" -f $Shortcut) }
        if (Test-Path -LiteralPath $OwnRoot) { $problems += ("registry key {0} remains" -f $OwnRoot) }
        if (Test-Path -LiteralPath $UninstallKey) { $problems += ("installed apps entry {0} remains" -f $UninstallKey) }
        foreach ($u in (Get-UserProfiles)) {
            if (Test-Path -LiteralPath (Join-Path $u.Path 'AppData\Local\Swapdesk\logs')) { $problems += ("the logs of {0} remain" -f $u.Sid) }
        }
        $srcLeft = $false
        try { $srcLeft = [System.Diagnostics.EventLog]::SourceExists($EventSource) } catch { }
        if ($srcLeft) { $problems += ("Event Log source {0} remains" -f $EventSource) }
        if (Test-Path -LiteralPath $InstallDir) { $problems += ("folder {0} remains" -f $InstallDir) }
        if (-not $KeepData -and (Test-Path -LiteralPath $DataDir)) { $problems += ("folder {0} remains" -f $DataDir) }
        $still = @(Get-RunningFromInstallDir)
        if ($still.Count -gt 0) { $problems += 'a Swapdesk process still runs from the install folder' }
        foreach ($l in $leftovers) { $problems += ("left: {0}" -f $l) }

        Say ("  service query        : {0} (sc.exe query exit code {1}; 1060 = does not exist)" -f $(if (Get-Svc) { 'STILL THERE' } else { 'not found (good)' }), $scCode)
        Say ("  firewall rule query  : {0} rule(s) of the group {1}" -f @(Get-NetFirewallRule -Group $FirewallGroup -ErrorAction SilentlyContinue).Count, $FirewallGroup)
        Say ("  install folder       : {0}" -f $(if (Test-Path -LiteralPath $InstallDir) { 'STILL THERE' } else { 'gone' }))
        Say ("  data folder          : {0}" -f $(if ($KeepData) { 'kept on request' } elseif (Test-Path -LiteralPath $DataDir) { 'STILL THERE' } else { 'gone' }))
        Say ("  shortcut             : {0}" -f $(if (Test-Path -LiteralPath $Shortcut) { 'STILL THERE' } else { 'gone' }))
        Say ("  registry key         : {0}" -f $(if (Test-Path -LiteralPath $OwnRoot) { 'STILL THERE' } else { 'gone' }))
        Say ("  installed apps entry : {0}" -f $(if (Test-Path -LiteralPath $UninstallKey) { 'STILL THERE' } else { 'gone' }))
        $sasNow = (Get-Item -LiteralPath $SasKey).GetValue($SasValue, $null)
        Say ("  {0} : {1}" -f $SasValue, $(if ($null -eq $sasNow) { '(absent)' } else { $sasNow }))
        Say ''
        if ($problems.Count -gt 0) {
            Say 'NOT CLEAN:'
            foreach ($pr in $problems) { Say ('  - ' + $pr) }
            Say 'Run uninstall again, or with -Force to delete unknown files in the two folders.'
            exit 1
        }
        Say 'Swapdesk is removed and nothing is left.'
    }
} catch {
    Say ''
    Say ('FAILED: {0}' -f $_.Exception.Message)
    Say 'Run uninstall again (it is safe to repeat).'
    exit 1
}
exit 0
