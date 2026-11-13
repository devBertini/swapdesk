# Firewall rules of a Swapdesk DEVELOPMENT instance (swapdesk-service --console --instance N), so another machine of
# the local network can reach it while the installed Swapdesk keeps its own rules and ports untouched.
#
#   dev-firewall.cmd [-Exe <path of the development swapdesk-service.exe>] [-Instance <1..99>] [-Remove] [-DryRun]
#
# What it changes (one UAC click; nothing else on the machine):
#   - removes the rules of the firewall group "Swapdesk development";
#   - unless -Remove: adds two inbound rules in that group for the program -Exe only, profiles Private and Domain,
#     remote addresses LocalSubnet: UDP 47820 + 10 N and 47821 + 10 N (session and pairing of instance N), and
#     UDP 5353 (mDNS).
# Default -Exe: %USERPROFILE%\sd-dev\swapdesk-service.exe (a copy of the built service: a running instance then never
# blocks a build, and the rule names one fixed path). The rules do nothing while that program is not running.
# -DryRun prints the actions and changes nothing (no UAC).
# What the elevated part did is written to %TEMP%\swapdesk-dev-firewall.log and printed here.
param(
    [string]$Exe = (Join-Path $env:USERPROFILE 'sd-dev\swapdesk-service.exe'),
    [ValidateRange(1, 99)][int]$Instance = 1,
    [switch]$Remove,
    [switch]$DryRun,
    [switch]$Elevated
)

$ErrorActionPreference = 'Stop'
$Group = 'Swapdesk development'
$RuleSession = 'Swapdesk-Dev-UDP-Session-Pairing'
$RuleMdns = 'Swapdesk-Dev-UDP-mDNS'
$Ports = @((47820 + 10 * $Instance), (47821 + 10 * $Instance))
$Log = Join-Path $env:TEMP 'swapdesk-dev-firewall.log'

function Test-Admin {
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    return ([System.Security.Principal.WindowsPrincipal]$id).IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Show-State {
    $rules = @(Get-NetFirewallRule -Group $Group -ErrorAction SilentlyContinue)
    if ($rules.Count -eq 0) { 'State now: no rule in the group "{0}"' -f $Group; return }
    foreach ($r in $rules) {
        $port = $r | Get-NetFirewallPortFilter
        $app = $r | Get-NetFirewallApplicationFilter
        $addr = $r | Get-NetFirewallAddressFilter
        'State now: {0} enabled={1} profile={2} {3} {4} remote={5} program={6}' -f $r.Name, $r.Enabled, $r.Profile, $port.Protocol, ($port.LocalPort -join ','), ($addr.RemoteAddress -join ','), $app.Program
    }
}

$plan = @('remove the rules of the group "{0}"' -f $Group)
if (-not $Remove) {
    $plan += 'New-NetFirewallRule {0}: inbound UDP {1} for {2}, profiles Private and Domain, remote LocalSubnet' -f $RuleSession, ($Ports -join ','), $Exe
    $plan += 'New-NetFirewallRule {0}: inbound UDP 5353 (mDNS) for {1}, profiles Private and Domain, remote LocalSubnet' -f $RuleMdns, $Exe
}

if ($DryRun) {
    $plan | ForEach-Object { '-> (dry run) ' + $_ }
    Show-State
    exit 0
}

if (-not (Test-Admin)) {
    if ($Elevated) { Write-Error 'the elevated copy has no administrator rights'; exit 1 }
    Remove-Item -LiteralPath $Log -ErrorAction SilentlyContinue
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', ('"{0}"' -f $PSCommandPath), '-Exe', ('"{0}"' -f $Exe), '-Instance', $Instance, '-Elevated')
    if ($Remove) { $argList += '-Remove' }
    try {
        $p = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList $argList -Verb RunAs -Wait -PassThru -WindowStyle Hidden
    } catch {
        'The administrator prompt was declined: nothing was changed.'
        exit 2
    }
    if (Test-Path -LiteralPath $Log) { Get-Content -LiteralPath $Log }
    exit $p.ExitCode
}

# Elevated from here on.
try {
    $out = @()
    $old = @(Get-NetFirewallRule -Group $Group -ErrorAction SilentlyContinue)
    if ($old.Count -gt 0) { $old | Remove-NetFirewallRule }
    $out += '-> ' + $plan[0] + (' ({0} removed)' -f $old.Count)
    if (-not $Remove) {
        New-NetFirewallRule -Name $RuleSession -DisplayName ('Swapdesk development: session and pairing (UDP {0})' -f ($Ports -join ', ')) -Group $Group `
            -Direction Inbound -Action Allow -Protocol UDP -LocalPort $Ports -Program $Exe -Profile Private, Domain -RemoteAddress LocalSubnet | Out-Null
        $out += '-> ' + $plan[1]
        New-NetFirewallRule -Name $RuleMdns -DisplayName 'Swapdesk development: discovery (mDNS, UDP 5353)' -Group $Group `
            -Direction Inbound -Action Allow -Protocol UDP -LocalPort 5353 -Program $Exe -Profile Private, Domain -RemoteAddress LocalSubnet | Out-Null
        $out += '-> ' + $plan[2]
    }
    $out += Show-State
    $out | Set-Content -LiteralPath $Log -Encoding utf8
    exit 0
} catch {
    ('FAILED: ' + $_.Exception.Message) | Set-Content -LiteralPath $Log -Encoding utf8
    exit 1
}
