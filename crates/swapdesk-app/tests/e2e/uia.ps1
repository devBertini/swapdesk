# UI Automation helper of tests/e2e_ui.rs. Windows PowerShell 5.1, ASCII only.
#
# A line protocol on stdin/stdout. Every string that crosses it is hex of its UTF-8 bytes ("-" is "none"), so names with
# accents survive any console code page. Replies: "OK <hex of the text>" or "ERR <hex of the message>".
#
#   windows <pid>                                              top-level windows of the process
#   tree <pid>                                                 every window and every descendant
#   find <pid> <winRegex|-> <exact|contains|regex> <name> <type|->
#   invoke <pid> <winRegex|-> <exact|contains|regex> <name> <type|->     Invoke pattern of the first enabled match
#   close <pid>                                                WindowPattern.Close of every top-level window
#   shot <pid> <path prefix>                                   one PNG per window: <prefix>-<n>.png
#   quit
#
# Only the UI Automation Invoke and Window patterns are used on the two Swapdesk processes: no mouse and no keyboard.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class SdUia {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT r);
}
'@
[void][SdUia]::SetProcessDPIAware()

$AE = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]

function Enc([string]$s) {
    $bytes = [Text.Encoding]::UTF8.GetBytes($s)
    $sb = New-Object System.Text.StringBuilder
    foreach ($b in $bytes) { [void]$sb.Append($b.ToString('x2')) }
    $sb.ToString()
}

function Dec([string]$h) {
    if ($h -eq '-' -or $h.Length -eq 0) { return $null }
    $b = New-Object byte[] ($h.Length / 2)
    for ($i = 0; $i -lt $b.Length; $i++) { $b[$i] = [Convert]::ToByte($h.Substring(2 * $i, 2), 16) }
    [Text.Encoding]::UTF8.GetString($b)
}

function Get-Windows([int]$procId) {
    $cond = New-Object System.Windows.Automation.PropertyCondition($AE::ProcessIdProperty, $procId)
    $col = $AE::RootElement.FindAll($Scope::Children, $cond)
    $out = @()
    foreach ($w in $col) { $out += $w }
    , $out
}

function Walk($root) {
    $root.FindAll($Scope::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
}

function Name-Matches([string]$name, [string]$mode, [string]$pat) {
    switch ($mode) {
        'exact' { return ($name -ceq $pat) }
        'contains' { return $name.Contains($pat) }
        'regex' { return ($name -match $pat) }
        default { return $false }
    }
}

function Type-Of($c) { $c.ControlType.ProgrammaticName -replace '^ControlType\.', '' }

function Find-El([int]$procId, $winPat, [string]$mode, [string]$pat, $type) {
    foreach ($w in (Get-Windows $procId)) {
        try { $wn = $w.Current.Name } catch { continue }
        if ($null -ne $winPat -and -not ($wn -match $winPat)) { continue }
        foreach ($e in (Walk $w)) {
            try { $c = $e.Current } catch { continue }
            if ($null -ne $type -and (Type-Of $c) -ne $type) { continue }
            if (Name-Matches $c.Name $mode $pat) {
                return @{ el = $e; win = $wn; name = $c.Name; type = (Type-Of $c); enabled = $c.IsEnabled }
            }
        }
    }
    return $null
}

function Describe-Window($w) {
    $c = $w.Current
    $r = $c.BoundingRectangle
    "{0} | {1} | class {2} | hwnd {3} | {4},{5} {6}x{7} | offscreen {8}" -f (Type-Of $c), $c.Name, $c.ClassName, $c.NativeWindowHandle, [int]$r.X, [int]$r.Y, [int]$r.Width, [int]$r.Height, $c.IsOffscreen
}

function Colors-Of($bmp) {
    $seen = New-Object 'System.Collections.Generic.HashSet[int]'
    $sum = 0.0
    $n = 0
    $steps = 40
    for ($i = 0; $i -lt $steps; $i++) {
        for ($j = 0; $j -lt $steps; $j++) {
            $x = [int](($bmp.Width - 1) * $i / ($steps - 1))
            $y = [int](($bmp.Height - 1) * $j / ($steps - 1))
            $p = $bmp.GetPixel($x, $y)
            [void]$seen.Add(($p.R -shr 3) * 1024 + ($p.G -shr 3) * 32 + ($p.B -shr 3))
            $sum += 0.299 * $p.R + 0.587 * $p.G + 0.114 * $p.B
            $n++
        }
    }
    @{ distinct = $seen.Count; luma = [math]::Round($sum / $n, 1) }
}

function Shot-Window($w, [string]$file) {
    $hwnd = [IntPtr]$w.Current.NativeWindowHandle
    $r = New-Object SdUia+RECT
    [void][SdUia]::GetWindowRect($hwnd, [ref]$r)
    $width = $r.Right - $r.Left
    $height = $r.Bottom - $r.Top
    if ($width -le 0 -or $height -le 0) { return "skipped (empty rectangle)" }
    $bmp = New-Object System.Drawing.Bitmap $width, $height
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    $ok = [SdUia]::PrintWindow($hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    $mode = 'PrintWindow'
    $stats = Colors-Of $bmp
    if (-not $ok -or $stats.distinct -le 1) {
        # A window of the flip model can come out blank: copy the screen under its rectangle instead.
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size $width, $height))
        $g.Dispose()
        $mode = 'CopyFromScreen'
        $stats = Colors-Of $bmp
    }
    $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    "{0} | {1} | {2}x{3} | distinct colours {4} | mean luma {5}" -f $file, $mode, $width, $height, $stats.distinct, $stats.luma
}

function Handle([string]$line) {
    $p = $line.Split(' ')
    $procId = [int]$p[1]
    switch ($p[0]) {
        'windows' {
            $lines = foreach ($w in (Get-Windows $procId)) { Describe-Window $w }
            return ($lines -join "`n")
        }
        'tree' {
            $lines = New-Object System.Collections.Generic.List[string]
            foreach ($w in (Get-Windows $procId)) {
                $lines.Add((Describe-Window $w))
                foreach ($e in (Walk $w)) {
                    try {
                        $c = $e.Current
                        $r = $c.BoundingRectangle
                        $rect = ''
                        if (-not [double]::IsInfinity($r.X)) { $rect = (" @{0},{1} {2}x{3}" -f [int]$r.X, [int]$r.Y, [int]$r.Width, [int]$r.Height) }
                        $val = ''
                        try {
                            $vp = $e.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
                            $val = ' value="' + $vp.Current.Value + '"'
                        } catch { }
                        $flags = ''
                        if (-not $c.IsEnabled) { $flags += ' disabled' }
                        try {
                            $tg = $e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
                            $flags += ' toggle=' + $tg.Current.ToggleState
                        } catch { }
                        $pats = ($e.GetSupportedPatterns() | ForEach-Object { $_.ProgrammaticName -replace 'Identifiers\.Pattern$', '' }) -join ','
                        if ($pats) { $flags += " [$pats]" }
                        $lines.Add(("  {0} `"{1}`"{2}{3}{4}" -f (Type-Of $c), $c.Name, $val, $flags, $rect))
                    } catch { $lines.Add("  (element gone: $_)") }
                }
            }
            return ($lines -join "`n")
        }
        'find' {
            $m = Find-El $procId (Dec $p[2]) $p[3] (Dec $p[4]) (Dec $p[5])
            if ($null -eq $m) { return 'NONE' }
            return ("FOUND`t{0}`t{1}`t{2}`t{3}" -f $m.win, $m.type, $m.enabled, $m.name)
        }
        'invoke' {
            $m = Find-El $procId (Dec $p[2]) $p[3] (Dec $p[4]) (Dec $p[5])
            if ($null -eq $m) { return 'NONE' }
            if (-not $m.enabled) { return ("DISABLED`t{0}" -f $m.name) }
            $how = 'Invoke'
            try {
                $ip = $m.el.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
                $ip.Invoke()
            } catch {
                # AccessKit exposes a selected control through SelectionItem and a toggled one through Toggle.
                try {
                    $sp = $m.el.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
                    $sp.Select()
                    $how = 'SelectionItem.Select'
                } catch {
                    $tp = $m.el.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
                    $tp.Toggle()
                    $how = 'Toggle'
                }
            }
            return ("INVOKED`t{0}`t{1}`t{2}`t{3}" -f $m.win, $m.type, $m.name, $how)
        }
        'close' {
            $n = 0
            foreach ($w in (Get-Windows $procId)) {
                try {
                    $wp = $w.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern)
                    $wp.Close()
                    $n++
                } catch { }
            }
            return "closed $n"
        }
        'shot' {
            $prefix = Dec $p[2]
            $i = 0
            $lines = foreach ($w in (Get-Windows $procId)) {
                $i++
                try { Shot-Window $w ("{0}-{1}.png" -f $prefix, $i) } catch { "shot failed: $_" }
            }
            return ($lines -join "`n")
        }
        default { throw "unknown command $($p[0])" }
    }
}

[Console]::Out.WriteLine('ready')
[Console]::Out.Flush()
while ($true) {
    $line = [Console]::In.ReadLine()
    if ($null -eq $line -or $line -eq 'quit') { break }
    try {
        $reply = 'OK ' + (Enc ([string](Handle $line)))
    } catch {
        $reply = 'ERR ' + (Enc ([string]($_ | Out-String)))
    }
    [Console]::Out.WriteLine($reply)
    [Console]::Out.Flush()
}
