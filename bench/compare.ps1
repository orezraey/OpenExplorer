# Speed comparison between Open Explorer and Air Explorer, driven through Windows UI Automation
# the same way for both: it opens each app, types a folder's path into a pane's address bar and
# times how long until the pane shows every item of the folder. See bench\README.md.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File bench\compare.ps1
#
# Windows PowerShell 5.1 (powershell.exe): it has .NET Framework's UIAutomationClient. The apps
# take the screen while it runs; leave the mouse and keyboard alone. Both apps are closed and
# reopened along the way.

param(
    # The Google account connected in both apps, as Open Explorer's address bar names it.
    [string] $Email = 'eliatan1122@gmail.com',
    # The same account's name in Air Explorer's address bar ("Meu Drive [<name>]").
    [string] $AirAccount = 'oreZ raeY',
    [string] $OpenExplorer = (Join-Path $PSScriptRoot '..\target\release\openexplorer.exe'),
    [string] $AirExplorer = 'C:\Program Files\AirExplorer\AirExplorer.exe',
    # Timed runs of each measurement, after one run that isn't counted.
    [int] $Runs = 5,
    # Give up on a run after this long, in seconds.
    [int] $Timeout = 180,
    # Comma-separated (powershell -File passes a list as one word): openexplorer,airexplorer.
    [string] $Apps = 'openexplorer,airexplorer',
    # Only these measurements, comma-separated: system32,winsxs,drive-1000-folders,start,memory...
    [string] $Only = '',
    [string] $Out = (Join-Path $PSScriptRoot ("results\" + (Get-Date -Format 'yyyy-MM-dd_HHmm') + '.json'))
)

$ErrorActionPreference = 'Stop'
# The parameters are typed [string], so the lists get names of their own.
$AppList = @($Apps -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$OnlyList = @($Only -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win {
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, int msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(int flags, int x, int y, int data, IntPtr extra);
    public static int ListCount(IntPtr h) { return (int)SendMessage(h, 0x1004, IntPtr.Zero, IntPtr.Zero); }
    // A left click at screen pixel (x, y).
    public static void Click(int x, int y) { SetCursorPos(x, y); mouse_event(2, 0, 0, 0, IntPtr.Zero); mouse_event(4, 0, 0, 0, IntPtr.Zero); }
}
"@
# UI Automation reports physical pixels; so must the clicks be.
[void][Win]::SetProcessDPIAware()

$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
function Prop($p, $v) { New-Object System.Windows.Automation.PropertyCondition($p, $v) }
function All { New-Object System.Windows.Automation.AndCondition(, [System.Windows.Automation.Condition[]]$args) }

# ------------------------------------------------------------------------------------------------
# Measurements: each opens a folder in the left pane; `Kind` is local or drive.

$Gd = "Open Explorer benchmark"
$Folders = [ordered]@{
    'system32' = @{ Local = 'C:\Windows\System32' }
    'winsxs'   = @{ Local = 'C:\Windows\WinSxS' }
    'drive-5000-files'   = @{ Drive = "$Gd/5000 files" }
    'drive-1000-folders' = @{ Drive = "$Gd/1000 folders" }
}
# Where a pane goes between runs, so each run opens its folder anew.
$Rest = 'C:\Windows\Web'

# Downloads: everything in a Drive folder, selected with Ctrl+A in the right pane and copied with
# the arrow between the panes into the left one, which shows an empty local folder. Timed from
# the arrow until every file is on disk whole and no program still writes it.
$Downloads = [ordered]@{
    'download-large'        = @{ Drive = "$Gd/transfer/large"; Files = 1; Dirs = 0; Bytes = 524288000 }
    # Air Explorer makes about one local folder every 2 s here, so 100 rather than 1,000.
    'download-100-folders'  = @{ Drive = "$Gd/100 folders"; Files = 0; Dirs = 100; Bytes = 0 }
}
$DownloadTo = 'C:\OEBench\dl'

# ------------------------------------------------------------------------------------------------
# What differs between the apps: how to find their window, a pane's address bar, and the number
# of items a pane shows once its folder has finished opening (-1 while it's still opening).

$Adapters = @{
    openexplorer = @{
        Name = 'Open Explorer'; Exe = $OpenExplorer; Process = 'openexplorer'
        DrivePath = { param($p) "$Email/Meu Drive/$p" }
        # Any pane opens any place.
        DrivePane = 0
        # Makes pane `i` a folder view: a pane on the "+" picker opens This PC in a new tab.
        Ready = {
            param($win, $i)
            $pc = $win.FindFirst($Scope::Descendants, (Prop $A::AutomationIdProperty "tp-quick-pc-$i"))
            if ($pc) { $pc.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke(); Start-Sleep -Milliseconds 500 }
        }
        # Puts the keyboard in pane `i`'s list.
        Focus = { param($win, $i) (& $Adapters.openexplorer.Pane $win $i).SetFocus() }
        # The arrow between the panes that copies the right pane's selection to the left one.
        CopyLeft = {
            param($win)
            $b = $win.FindFirst($Scope::Descendants, (Prop $A::AutomationIdProperty 'copy-left'))
            $b.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
        }
        Pane = { param($win, $i) $win.FindFirst($Scope::Descendants, (Prop $A::AutomationIdProperty "pane-$i")) }
        Address = {
            param($win, $i)
            $pane = & $Adapters.openexplorer.Pane $win $i
            $pane.SetFocus()
            [System.Windows.Forms.SendKeys]::SendWait('^l')
            Start-Sleep -Milliseconds 150
            $edit = $null
            foreach ($e in $pane.FindAll($Scope::Descendants, (Prop $A::ControlTypeProperty ([System.Windows.Automation.ControlType]::Edit)))) {
                if ($e.Current.AutomationId -notlike 'search-*') { $edit = $e }
            }
            $edit
        }
        # "1.234 itens" in the pane's status bar: the items it shows. "· carregando…" after it
        # means the app is still checking with the cloud, but every item is already on screen
        # (Air Explorer shows what it kept and checks nothing), so it doesn't hold the count back.
        Count = {
            param($win, $i)
            $pane = & $Adapters.openexplorer.Pane $win $i
            # Right after a start the window is there before its panes.
            if (-not $pane) { return -1 }
            foreach ($t in $pane.FindAll($Scope::Children, (Prop $A::ControlTypeProperty ([System.Windows.Automation.ControlType]::Text)))) {
                if ($t.Current.Name -match '^([\d\.,]+) ite(m|ns)(\s|$)') { return [int]($Matches[1] -replace '[\.,]', '') }
            }
            -1
        }
    }
    airexplorer = @{
        Name = 'Air Explorer'; Exe = $AirExplorer; Process = 'AirExplorer'
        DrivePath = { param($p) "Meu Drive [$AirAccount]/$p" }
        # Each tab belongs to one account: the left pane's is This PC, the right pane's the Google
        # account (set up so before running).
        DrivePane = 1
        Ready = { param($win, $i) }
        # A list view takes no focus through UI Automation: a click on its first row (on an
        # empty list, its middle) does.
        Focus = {
            param($win, $i)
            $r = ((& $Adapters.airexplorer.Lists $win)[$i]).Current.BoundingRectangle
            $y = if ((& $Adapters.airexplorer.Count $win $i) -gt 0) { $r.Top + 52 } else { $r.Top + $r.Height / 2 }
            [Win]::Click([int]($r.Left + 120), [int]$y)
        }
        # The arrows between the panes are unnamed buttons, top to bottom: link, sync, copy to
        # the right, copy to the left.
        CopyLeft = {
            param($win)
            $buttons = $win.FindAll($Scope::Descendants, (Prop $A::ClassNameProperty 'WindowsForms10.BUTTON.app.0.25bb5ff_r6_ad1'))
            $r = ($buttons | Sort-Object { $_.Current.BoundingRectangle.Y } | Select-Object -Last 1).Current.BoundingRectangle
            [Win]::Click([int]($r.X + $r.Width / 2), [int]($r.Y + $r.Height / 2))
        }
        # The pane's file list: a SysListView32, left to right.
        Lists = {
            param($win)
            $lists = $win.FindAll($Scope::Descendants, (Prop $A::ClassNameProperty 'WindowsForms10.SysListView32.app.0.25bb5ff_r6_ad1'))
            @($lists | Where-Object { -not $_.Current.IsOffscreen } | Sort-Object { $_.Current.BoundingRectangle.X })
        }
        # The address box shows only while editing: a click on the bar's free end (the bar is
        # drawn as a ProgressBar, `progress`) opens it.
        Address = {
            param($win, $i)
            $x = ((& $Adapters.airexplorer.Lists $win)[$i]).Current.BoundingRectangle.X
            $near = { param($id) $win.FindAll($Scope::Descendants, (Prop $A::AutomationIdProperty $id)) | Sort-Object { [math]::Abs($_.Current.BoundingRectangle.Right - $x - 400) } | Select-Object -First 1 }
            $bar = & $near 'progress'
            $r = $bar.Current.BoundingRectangle
            [Win]::Click([int]($r.Right - 30), [int]($r.Top + $r.Height / 2))
            Start-Sleep -Milliseconds 200
            & $near 'PART_EditableTextBox'
        }
        Count = {
            param($win, $i)
            $lists = & $Adapters.airexplorer.Lists $win
            if ($lists.Count -le $i) { return -1 }
            [Win]::ListCount([IntPtr]$lists[$i].Current.NativeWindowHandle)
        }
    }
}

# ------------------------------------------------------------------------------------------------

function Get-MainWindow($ad, [int] $wait = 60) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $wait) {
        $proc = Get-Process -Name $ad.Process -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
        if ($proc) { return $A::FromHandle($proc.MainWindowHandle) }
        Start-Sleep -Milliseconds 20
    }
    throw "$($ad.Name): no window after $wait s"
}

function Close-App($ad) {
    foreach ($p in Get-Process -Name $ad.Process -ErrorAction SilentlyContinue) {
        [void]$p.CloseMainWindow()
        if (-not $p.WaitForExit(15000)) { $p.Kill(); [void]$p.WaitForExit(5000) }
    }
    Start-Sleep -Seconds 2
}

# Air Explorer opens a promotion over its window at start (Black Friday, Pro offers); it blocks
# the panes until closed. Closes the app's dialogs over its main window.
function Close-Popups($ad, $win) {
    if ($ad.Process -ne 'AirExplorer') { return }
    $cond = Prop $A::ControlTypeProperty ([System.Windows.Automation.ControlType]::Window)
    foreach ($w in $win.FindAll($Scope::Children, $cond)) {
        $wp = $null
        if ($w.TryGetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern, [ref] $wp)) {
            Write-Host "  closing $($ad.Name)'s window '$($w.Current.Name)'"
            $wp.Close()
            Start-Sleep -Milliseconds 300
        }
    }
}

function Focus-Window($win) {
    $h = [IntPtr]$win.Current.NativeWindowHandle
    [void][Win]::ShowWindow($h, 9)
    [void][Win]::SetForegroundWindow($h)
    Start-Sleep -Milliseconds 200
}

# Types `path` into pane `i`'s address bar, ready for Enter.
function Go-To($ad, $win, [int] $i, [string] $path) {
    # Right after a dialog closes, the address box may not take the focus yet: a few tries.
    for ($try = 1; ; $try++) {
        Close-Popups $ad $win
        Focus-Window $win
        $box = & $ad.Address $win $i
        if (-not $box) { throw "$($ad.Name): no address bar" }
        try { $box.SetFocus(); break } catch { if ($try -ge 5) { throw } }
        Start-Sleep -Milliseconds 500
    }
    $vp = $null
    if ($box.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref] $vp)) {
        $vp.SetValue($path)
    } else {
        [System.Windows.Forms.SendKeys]::SendWait('^a')
        [System.Windows.Forms.Clipboard]::SetText($path)
        [System.Windows.Forms.SendKeys]::SendWait('^v')
    }
    Start-Sleep -Milliseconds 50
}

# Waits until pane `i` shows `expected` items (any settled count when 0), in seconds.
# With -Fresh it finds the window again each time: during a start the first window handle the
# process reports may not be the one that ends up holding the panes.
function Wait-Count($ad, $win, [int] $i, [int] $expected, [Diagnostics.Stopwatch] $sw, [switch] $Fresh) {
    $last = -2; $since = 0.0
    while ($sw.Elapsed.TotalSeconds -lt $Timeout) {
        if ($Fresh) { try { $win = Get-MainWindow $ad 1 } catch { Start-Sleep -Milliseconds 15; continue } }
        $n = try { & $ad.Count $win $i } catch { -1 }
        $now = $sw.Elapsed.TotalSeconds
        if ($expected -gt 0) {
            if ($n -eq $expected) { return $now }
        } elseif ($n -ge 0) {
            # Learning a folder's count: the same for two seconds.
            if ($n -ne $last) { $last = $n; $since = $now } elseif ($now - $since -ge 2) { return @($since, $n) }
        }
        Start-Sleep -Milliseconds 15
    }
    throw "$($ad.Name): pane $i didn't reach $expected items in $Timeout s (last $n)"
}

# Opens `path` in pane `i`: the time runs from the Enter that opens it (which an app may handle
# before the key's message returns, folder read included) until the pane shows every item.
function Open-Folder($ad, $win, [int] $i, [string] $path, [int] $expected) {
    Go-To $ad $win $i $path
    $sw = [Diagnostics.Stopwatch]::StartNew()
    [System.Windows.Forms.SendKeys]::SendWait('{ENTER}')
    Wait-Count $ad $win $i $expected $sw
}

# Files, folders and bytes under `dir`, and whether every file can be opened alone (no program
# still writes it).
function Tree-Of([string] $dir) {
    $files = @(Get-ChildItem -LiteralPath $dir -Recurse -File -Force -ErrorAction SilentlyContinue)
    $dirs = @(Get-ChildItem -LiteralPath $dir -Recurse -Directory -Force -ErrorAction SilentlyContinue)
    $bytes = ($files | Measure-Object Length -Sum).Sum
    $free = $true
    foreach ($f in $files) {
        try { $s = [System.IO.File]::Open($f.FullName, 'Open', 'Read', 'None'); $s.Close() } catch { $free = $false; break }
    }
    @{ files = $files.Count; dirs = $dirs.Count; bytes = [long]$bytes; free = $free }
}

function Download($ad, $win, $d) {
    if (Test-Path $DownloadTo) { Remove-Item -LiteralPath $DownloadTo -Recurse -Force }
    New-Item -ItemType Directory -Force $DownloadTo | Out-Null
    & $ad.Ready $win 1
    Open-Folder $ad $win 0 $DownloadTo 0 | Out-Null
    Open-Folder $ad $win 1 (& $ad.DrivePath $d.Drive) 0 | Out-Null
    Focus-Window $win
    & $ad.Focus $win 1
    Start-Sleep -Milliseconds 200
    [System.Windows.Forms.SendKeys]::SendWait('^a')
    Start-Sleep -Milliseconds 300
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $ad.CopyLeft $win
    $pasted = $false
    while ($sw.Elapsed.TotalSeconds -lt $Timeout) {
        Close-Popups $ad $win
        $t = Tree-Of $DownloadTo
        if ($t.files -gt 0 -or $t.dirs -gt 0) { $pasted = $true }
        # Nothing arrived after a few seconds: the copy didn't start. The run starts over.
        if (-not $pasted -and $sw.Elapsed.TotalSeconds -gt 8) {
            Write-Host "    (the copy didn't start; trying again)"
            return Download $ad $win $d
        }
        if ($t.files -eq $d.Files -and $t.dirs -eq $d.Dirs -and $t.bytes -eq $d.Bytes -and $t.free) { return $sw.Elapsed.TotalSeconds }
        Start-Sleep -Milliseconds 50
    }
    $t = Tree-Of $DownloadTo
    throw "$($ad.Name): download of $($d.Drive) not done in $Timeout s ($($t.files) files, $($t.dirs) folders, $($t.bytes) bytes)"
}

function Memory-Of($ad) {
    $root = Get-Process -Name $ad.Process | Select-Object -First 1
    $all = @(Get-CimInstance Win32_Process)
    $ids = New-Object System.Collections.Generic.HashSet[int]
    [void]$ids.Add($root.Id)
    do {
        $grew = $false
        foreach ($p in $all) { if ($ids.Contains([int]$p.ParentProcessId) -and $ids.Add([int]$p.ProcessId)) { $grew = $true } }
    } while ($grew)
    $ws = 0; $priv = 0
    foreach ($id in $ids) {
        $p = Get-Process -Id $id -ErrorAction SilentlyContinue
        if ($p) { $ws += $p.WorkingSet64; $priv += $p.PrivateMemorySize64 }
    }
    @{ processes = $ids.Count; working_set_mb = [math]::Round($ws / 1MB, 1); private_mb = [math]::Round($priv / 1MB, 1) }
}

function Median($xs) {
    $s = @($xs | Sort-Object)
    if ($s.Count -eq 0) { return $null }
    if ($s.Count % 2) { $s[[int][math]::Floor($s.Count / 2)] } else { ($s[$s.Count / 2 - 1] + $s[$s.Count / 2]) / 2 }
}

function Want($name) { $OnlyList.Count -eq 0 -or $OnlyList -contains $name }

# ------------------------------------------------------------------------------------------------

$results = [ordered]@{
    date = (Get-Date).ToString('s')
    machine = @{
        os = (Get-CimInstance Win32_OperatingSystem).Caption
        build = [Environment]::OSVersion.Version.ToString()
        cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
        ram_gb = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
    }
    runs = $Runs
    apps = [ordered]@{}
}

foreach ($key in $AppList) {
    $ad = $Adapters[$key]
    $exe = Get-Item $ad.Exe
    $version = $exe.VersionInfo.ProductVersion
    if (-not $version -and $key -eq 'openexplorer') {
        # A local build has no version resource: the workspace's version and the commit.
        $cargo = Get-Content (Join-Path $PSScriptRoot '..\Cargo.toml') | Select-String '^version = "(.+)"' | Select-Object -First 1
        $version = "$($cargo.Matches[0].Groups[1].Value) ($(git -C $PSScriptRoot rev-parse --short HEAD))"
    }
    Write-Host "== $($ad.Name) $version"
    $r = [ordered]@{ version = $version; exe_mb = [math]::Round($exe.Length / 1MB, 1) }

    Close-App $ad
    Start-Process $ad.Exe | Out-Null
    $win = Get-MainWindow $ad
    Start-Sleep -Seconds 5
    $win = Get-MainWindow $ad

    # Learn each folder's count (and warm the way there), then time it.
    foreach ($name in $Folders.Keys) {
        if (-not (Want $name)) { continue }
        $f = $Folders[$name]
        # A Drive folder opens in the app's Drive pane, between runs going back to the benchmark's
        # top folder there.
        if ($f.Local) { $path = $f.Local; $pane = 0; $back = $Rest }
        else { $path = & $ad.DrivePath $f.Drive; $pane = $ad.DrivePane; $back = & $ad.DrivePath $Gd }
        Open-Folder $ad $win $pane $back 0 | Out-Null
        $learned = Open-Folder $ad $win $pane $path 0
        $expected = $learned[1]
        $times = @()
        for ($k = 0; $k -lt $Runs; $k++) {
            Open-Folder $ad $win $pane $back 0 | Out-Null
            $times += [math]::Round((Open-Folder $ad $win $pane $path $expected), 3)
        }
        Write-Host ("  {0,-20} {1,6} items  median {2:N3} s  ({3})" -f $name, $expected, (Median $times), ($times -join ', '))
        $r[$name] = @{ items = $expected; seconds = $times; median = (Median $times) }
    }

    foreach ($name in $Downloads.Keys) {
        if (-not (Want $name)) { continue }
        $d = $Downloads[$name]
        $times = @()
        for ($k = 0; $k -le $Runs; $k++) {
            $t = [math]::Round((Download $ad $win $d), 3)
            Write-Host ("    {0} run {1}: {2} s" -f $name, $k, $t)
            if ($k -gt 0) { $times += $t }
        }
        Write-Host ("  {0,-20} median {1:N3} s  ({2})" -f $name, (Median $times), ($times -join ', '))
        $r[$name] = @{ seconds = $times; median = (Median $times); files = $d.Files; folders = $d.Dirs; bytes = $d.Bytes }
    }

    # Start: from launch until the left pane shows its first folder whole (Air Explorer reopens
    # the last one, C:\Windows\Web; Open Explorer the desktop): the moment its count reached
    # the number it then kept for two seconds.
    if (Want 'start') {
        Open-Folder $ad $win 0 $Rest 0 | Out-Null
        $times = @()
        for ($k = 0; $k -le $Runs; $k++) {
            Close-App $ad
            $sw = [Diagnostics.Stopwatch]::StartNew()
            Start-Process $ad.Exe | Out-Null
            $win = Get-MainWindow $ad
            $t = (Wait-Count $ad $win 0 0 $sw -Fresh)[0]
            $win = Get-MainWindow $ad
            if ($k -gt 0) { $times += [math]::Round($t, 3) }
        }
        Write-Host ("  {0,-20} median {1:N3} s  ({2})" -f 'start', (Median $times), ($times -join ', '))
        $r['start'] = @{ seconds = $times; median = (Median $times) }
    }

    # Memory, 10 s after the last start, idle, then after opening the folder of 1000 folders.
    if (Want 'memory') {
        # Without Air Explorer's promotion, which is a web page of its own.
        Close-Popups $ad $win
        Start-Sleep -Seconds 10
        $idle = Memory-Of $ad
        Open-Folder $ad $win $ad.DrivePane (& $ad.DrivePath "$Gd/1000 folders") 0 | Out-Null
        Start-Sleep -Seconds 3
        $busy = Memory-Of $ad
        Write-Host ("  memory idle {0} MB private, {1} MB working set; with 1000 folders {2} / {3} MB ({4} processes)" -f $idle.private_mb, $idle.working_set_mb, $busy.private_mb, $busy.working_set_mb, $busy.processes)
        $r['memory_idle'] = $idle
        $r["memory_1000_folders"] = $busy
    }

    $results.apps[$key] = $r
}

New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
$results | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 $Out
Write-Host "results -> $Out"
