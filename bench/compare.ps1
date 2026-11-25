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
    # The cloud accounts are the ones connected in Open Explorer (its settings folder says which);
    # Air Explorer must have the same ones, one per cloud, each open in a tab of its right pane.
    # The Google account, when Open Explorer has more than one.
    [string] $Email = '',
    [string] $OpenExplorer = (Join-Path $PSScriptRoot '..\target\release\openexplorer.exe'),
    [string] $AirExplorer = 'C:\Program Files\AirExplorer\AirExplorer.exe',
    # Timed runs of each measurement, after one run that isn't counted.
    [int] $Runs = 5,
    # Give up on a run after this long, in seconds.
    [int] $Timeout = 180,
    # Comma-separated (powershell -File passes a list as one word): openexplorer,airexplorer.
    [string] $Apps = 'openexplorer,airexplorer',
    # Only these measurements, comma-separated: system32,winsxs,drive-1000-folders,onedrive-5000-files,start,memory...
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
    public static void DoubleClick(int x, int y) { Click(x, y); Click(x, y); }

    // A row of another process's list view (Air Explorer's is virtual: its rows aren't in the UI
    // Automation tree), read through a buffer in that process.
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("kernel32.dll")] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll")] static extern IntPtr VirtualAllocEx(IntPtr p, IntPtr at, IntPtr size, uint type, uint protect);
    [DllImport("kernel32.dll")] static extern bool VirtualFreeEx(IntPtr p, IntPtr at, IntPtr size, uint type);
    [DllImport("kernel32.dll")] static extern bool WriteProcessMemory(IntPtr p, IntPtr at, byte[] buf, IntPtr size, out IntPtr done);
    [DllImport("kernel32.dll")] static extern bool ReadProcessMemory(IntPtr p, IntPtr at, byte[] buf, IntPtr size, out IntPtr done);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    struct POINT { public int X, Y; }
    // The screen point at the middle of the first row of list h whose first column is name,
    // or null. LVM_GETITEMTEXTW and LVM_GETITEMRECT (64-bit LVITEMW: pszText at 24, cchTextMax
    // at 32).
    public static int[] RowPoint(IntPtr h, string name) {
        uint pid; GetWindowThreadProcessId(h, out pid);
        IntPtr proc = OpenProcess(0x38, false, pid);
        IntPtr mem = VirtualAllocEx(proc, IntPtr.Zero, (IntPtr)4096, 0x3000, 4);
        try {
            int n = ListCount(h);
            IntPtr done;
            for (int i = 0; i < n; i++) {
                byte[] item = new byte[88];
                BitConverter.GetBytes((long)mem + 512).CopyTo(item, 24);
                BitConverter.GetBytes(260).CopyTo(item, 32);
                WriteProcessMemory(proc, mem, item, (IntPtr)88, out done);
                int len = (int)SendMessage(h, 0x1073, (IntPtr)i, mem);
                byte[] text = new byte[len * 2];
                ReadProcessMemory(proc, mem + 512, text, (IntPtr)text.Length, out done);
                if (System.Text.Encoding.Unicode.GetString(text) != name) continue;
                byte[] rect = new byte[16];
                WriteProcessMemory(proc, mem, rect, (IntPtr)16, out done);
                SendMessage(h, 0x100E, (IntPtr)i, mem);
                ReadProcessMemory(proc, mem, rect, (IntPtr)16, out done);
                POINT p; p.X = BitConverter.ToInt32(rect, 0) + 40; p.Y = (BitConverter.ToInt32(rect, 4) + BitConverter.ToInt32(rect, 12)) / 2;
                ClientToScreen(h, ref p);
                return new int[] { p.X, p.Y };
            }
            return null;
        } finally { VirtualFreeEx(proc, mem, IntPtr.Zero, 0x8000); CloseHandle(proc); }
    }
}
"@
# UI Automation reports physical pixels; so must the clicks be.
[void][Win]::SetProcessDPIAware()

$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
function Prop($p, $v) { New-Object System.Windows.Automation.PropertyCondition($p, $v) }
function All { New-Object System.Windows.Automation.AndCondition(, [System.Windows.Automation.Condition[]]$args) }

# ------------------------------------------------------------------------------------------------
# The clouds, and the accounts Open Explorer has connected in each (from its settings folder).

$Config = Join-Path $env:APPDATA 'OpenExplorer'
function Accounts-In([string] $file) {
    $path = Join-Path $Config $file
    if (Test-Path $path) { @(Get-Content -Raw -Encoding UTF8 $path | ConvertFrom-Json) } else { @() }
}
$Google = Accounts-In 'accounts.json' | Where-Object { -not $Email -or $_.email -eq $Email } | Select-Object -First 1
$OneDrive = Accounts-In 'onedrive-accounts.json' | Select-Object -First 1
$Dropbox = Accounts-In 'dropbox-accounts.json' | Select-Object -First 1
$Mega = Accounts-In 'mega-accounts.json' | Select-Object -First 1

# Each cloud's test data (made by the examples in bench\README.md) sits in "Open Explorer
# benchmark". `Oe` is where that folder's parent is in Open Explorer's address bar; `Air` is how
# Air Explorer's address bar starts for the account ("Dropbox [Ana]"), whose own folder it shows
# at the top.
$Gd = "Open Explorer benchmark"
$Clouds = [ordered]@{}
if ($Google) { $Clouds['drive'] = @{ Oe = "$($Google.email)/Meu Drive"; Air = 'Meu Drive [' } }
if ($OneDrive) { $Clouds['onedrive'] = @{ Oe = "onedrive://$($OneDrive.email)/Meus arquivos"; Air = 'OneDrive [' } }
if ($Dropbox) { $Clouds['dropbox'] = @{ Oe = "dropbox://$($Dropbox.email)"; Air = 'Dropbox [' } }
if ($Mega) { $Clouds['mega'] = @{ Oe = "mega://$($Mega.email)/Drive na nuvem"; Air = 'Mega [' } }

# ------------------------------------------------------------------------------------------------
# Measurements: each opens a folder, a local one in the left pane or a cloud's in the app's
# cloud pane.

$Folders = [ordered]@{
    'system32' = @{ Local = 'C:\Windows\System32' }
    'winsxs'   = @{ Local = 'C:\Windows\WinSxS' }
}
foreach ($c in $Clouds.Keys) {
    $Folders["$c-5000-files"] = @{ Cloud = $c; Path = "$Gd/5000 files" }
    $Folders["$c-1000-folders"] = @{ Cloud = $c; Path = "$Gd/1000 folders" }
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
# Each download goes to a folder of its own under this one, never emptied while an app may
# still write there (a download still running would fail with "file not found"); they go at
# the end, with the apps closed.
$DownloadRoot = 'C:\OEBench'
$DownloadTo = $null

# ------------------------------------------------------------------------------------------------
# What differs between the apps: how to find their window, a pane's address bar, and the number
# of items a pane shows once its folder has finished opening (-1 while it's still opening).

$Adapters = @{
    openexplorer = @{
        Name = 'Open Explorer'; Exe = $OpenExplorer; Process = 'openexplorer'; Title = '^Open Explorer'
        CloudPath = { param($win, $c, $p) "$($Clouds[$c].Oe)/$p" }
        # Deletes the folder listings it keeps on disk between runs (Google Drive's and each
        # cloud's), with the app closed. A process started from a packaged app (the Claude app)
        # may see its own copy of the folder, so both copies go.
        Forget = {
            $roots = @("$env:LOCALAPPDATA\OpenExplorer")
            $roots += Get-ChildItem "$env:LOCALAPPDATA\Packages\*\LocalCache\Local\OpenExplorer" -Directory -ErrorAction SilentlyContinue | ForEach-Object FullName
            foreach ($root in $roots) {
                foreach ($d in 'cache', 'onedrive', 'dropbox', 'mega', 'b2') {
                    if (Test-Path "$root\$d") { Remove-Item -LiteralPath "$root\$d" -Recurse -Force }
                }
            }
        }
        # Any pane opens any place.
        CloudPane = 0
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
        # The folder pane `i` shows: the last step of its breadcrumb (`crumb-N`).
        Where = {
            param($win, $i)
            $pane = & $Adapters.openexplorer.Pane $win $i
            if (-not $pane) { return $null }
            $crumbs = $pane.FindAll($Scope::Descendants, (Prop $A::ControlTypeProperty ([System.Windows.Automation.ControlType]::Hyperlink))) | Where-Object { $_.Current.AutomationId -like 'crumb-*' }
            ($crumbs | Sort-Object { [int]($_.Current.AutomationId -replace '\D', '') } | Select-Object -Last 1).Current.Name
        }
        # The screen point of the row `name` in pane `i`'s list.
        Row = {
            param($win, $i, $name)
            $cond = All (Prop $A::ControlTypeProperty ([System.Windows.Automation.ControlType]::DataItem)) (Prop $A::NameProperty $name)
            $row = (& $Adapters.openexplorer.Pane $win $i).FindFirst($Scope::Descendants, $cond)
            if ($row) { $r = $row.Current.BoundingRectangle; @([int]($r.X + 40), [int]($r.Y + $r.Height / 2)) }
        }
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
                if ($t.Current.Name -match '^([\d\.,]+) ite(m|ns)(\s|$)') {
                    $n = [int]($Matches[1] -replace '[\.,]', '')
                    # "0 itens · carregando…" is a folder still on its way, not an empty one.
                    if ($n -eq 0 -and $t.Current.Name -match 'carregando') { return -1 }
                    return $n
                }
            }
            -1
        }
    }
    airexplorer = @{
        Name = 'Air Explorer'; Exe = $AirExplorer; Process = 'AirExplorer'; Title = '^Air Explorer v'
        # Each tab belongs to one account: the left pane's is This PC, and the right pane has a
        # tab for each cloud account (opened before running). This brings the cloud's tab
        # forward and starts the path with the account's name as its address bar shows it.
        CloudPath = {
            param($win, $c, $p)
            "$(& $Adapters.airexplorer.Tab $win $Clouds[$c].Air)/$p"
        }
        CloudPane = 1
        # It keeps no folder listings on disk (only its settings, in %APPDATA%\AirExplorer).
        Forget = { }
        # The right pane's tabs are a Windows tab control, and only the tab in front is in the
        # UI Automation tree: each tab in turn comes forward (TCM_SETCURFOCUS, which selects as
        # a click does) until one's address bar starts with `prefix`; its whole name is returned.
        Tab = {
            param($win, $prefix)
            $cond = Prop $A::ClassNameProperty 'WindowsForms10.SysTabControl32.app.0.25bb5ff_r6_ad1'
            $strip = $win.FindAll($Scope::Descendants, $cond) | Sort-Object { $_.Current.BoundingRectangle.X } | Select-Object -Last 1
            $h = [IntPtr]$strip.Current.NativeWindowHandle
            $text = Prop $A::ControlTypeProperty ([System.Windows.Automation.ControlType]::Text)
            $any = [System.Windows.Automation.Condition]::TrueCondition
            # The account's name in the tab in front, if its address starts with `prefix`.
            $front = {
                $page = $strip.FindFirst($Scope::Children, $any)
                $root = $page.FindAll($Scope::Descendants, $text) | Where-Object { $_.Current.Name.StartsWith($prefix) } | Select-Object -First 1
                if ($root) { $root.Current.Name }
            }
            # Right after a start a tab may still be connecting, with no address yet: a few rounds.
            for ($round = 0; $round -lt 30; $round++) {
            $name = & $front
            if ($name) { return $name }
            # The last tab is the "+" that adds one.
            $n = [int][Win]::SendMessage($h, 0x1304, [IntPtr]::Zero, [IntPtr]::Zero) - 1
            $cur = [int][Win]::SendMessage($h, 0x130B, [IntPtr]::Zero, [IntPtr]::Zero)
            for ($k = 0; $k -lt $n; $k++) {
                if ($k -eq $cur) { continue }
                $was = $strip.FindFirst($Scope::Children, $any).Current.NativeWindowHandle
                [void][Win]::SendMessage($h, 0x1330, [IntPtr]$k, [IntPtr]::Zero)
                # Until the tree shows the new tab's page.
                for ($try = 0; $try -lt 50 -and $strip.FindFirst($Scope::Children, $any).Current.NativeWindowHandle -eq $was; $try++) { Start-Sleep -Milliseconds 50 }
                Start-Sleep -Milliseconds 200
                $name = & $front
                if ($name) { return $name }
                $cur = $k
            }
            Start-Sleep -Seconds 1
            }
            throw "Air Explorer: no tab in the right pane whose address starts with '$prefix'"
        }
        Ready = { param($win, $i) }
        # Its address bar stays on the old folder until the new one is open: nothing to wait for.
        Where = { param($win, $i) $null }
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
        Row = {
            param($win, $i, $name)
            [Win]::RowPoint([IntPtr]((& $Adapters.airexplorer.Lists $win)[$i]).Current.NativeWindowHandle, $name)
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

# The app's main window: of its process's top-level windows, the one whose title matches the
# adapter's `Title` (Air Explorer's promotion can be the window Windows calls main).
function Get-MainWindow($ad, [int] $wait = 60) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $wait) {
        foreach ($proc in Get-Process -Name $ad.Process -ErrorAction SilentlyContinue) {
            $mine = $A::RootElement.FindAll($Scope::Children, (Prop $A::ProcessIdProperty $proc.Id))
            $win = $mine | Where-Object { $_.Current.Name -match $ad.Title } | Select-Object -First 1
            if ($win) { return $win }
        }
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
    # Over the main window, or a window of its own.
    $others = $A::RootElement.FindAll($Scope::Children, (Prop $A::ProcessIdProperty $win.Current.ProcessId)) | Where-Object { $_.Current.Name -notmatch $ad.Title }
    foreach ($w in @($win.FindAll($Scope::Children, $cond)) + @($others)) {
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

# Waits until pane `i` shows `expected` items (when 0, any count of at least `least` that holds
# for two seconds), in seconds.
# With -Fresh it finds the window again each time: during a start the first window handle the
# process reports may not be the one that ends up holding the panes.
function Wait-Count($ad, $win, [int] $i, [int] $expected, [Diagnostics.Stopwatch] $sw, [switch] $Fresh, [int] $least = 0) {
    $last = -2; $since = 0.0
    while ($sw.Elapsed.TotalSeconds -lt $Timeout) {
        if ($Fresh) { try { $win = Get-MainWindow $ad 1 } catch { Start-Sleep -Milliseconds 15; continue } }
        $n = try { & $ad.Count $win $i } catch { -1 }
        $now = $sw.Elapsed.TotalSeconds
        if ($expected -gt 0) {
            if ($n -eq $expected) { return $now }
        } elseif ($n -ge $least) {
            # Learning a folder's count: the same for two seconds.
            if ($n -ne $last) { $last = $n; $since = $now } elseif ($now - $since -ge 2) { return @($since, $n) }
        }
        Start-Sleep -Milliseconds 15
    }
    throw "$($ad.Name): pane $i didn't reach $expected items in $Timeout s (last $n)"
}

# Opens `path` in pane `i`: the time runs from the Enter that opens it (which an app may handle
# before the key's message returns, folder read included) until the pane shows every item.
function Open-Folder($ad, $win, [int] $i, [string] $path, [int] $expected, [int] $least = 0) {
    Go-To $ad $win $i $path
    $sw = [Diagnostics.Stopwatch]::StartNew()
    [System.Windows.Forms.SendKeys]::SendWait('{ENTER}')
    # Learning a count: not the old folder's, while the new one is still being found.
    if ($expected -eq 0) {
        $leaf = ($path -split '[\\/]' | Where-Object { $_ } | Select-Object -Last 1)
        while ($sw.Elapsed.TotalSeconds -lt $Timeout) {
            $at = & $ad.Where $win $i
            if ($null -eq $at -or $at -eq $leaf) { break }
            Start-Sleep -Milliseconds 15
        }
    }
    Wait-Count $ad $win $i $expected $sw -least $least
}

# Opens the folder `name`, a row of pane `i`'s list, with a double click: the time runs from the
# click until the pane shows `expected` items.
function Open-Row($ad, $win, [int] $i, [string] $name, [int] $expected, [int] $least = 0) {
    Close-Popups $ad $win
    Focus-Window $win
    # The list may still be filling in, or the pane being redrawn: a few seconds' tries.
    $at = $null
    for ($try = 0; $try -lt 100 -and -not $at; $try++) {
        $at = try { & $ad.Row $win $i $name } catch { $null }
        if (-not $at) { Start-Sleep -Milliseconds 100 }
    }
    if (-not $at) { throw "$($ad.Name): no row '$name' in pane $i" }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    [Win]::DoubleClick($at[0], $at[1])
    Wait-Count $ad $win $i $expected $sw -least $least
}

# Starts the app anew, with no folder listings kept from before, and waits until its window
# shows its first folder.
function Start-Uncached($ad) {
    Close-App $ad
    & $ad.Forget
    Start-App $ad
}

# Starts the app and waits until its window shows its first folder: its panes are there.
function Start-App($ad) {
    Start-Process $ad.Exe | Out-Null
    Wait-Count $ad (Get-MainWindow $ad) 0 0 ([Diagnostics.Stopwatch]::StartNew()) -Fresh | Out-Null
    $win = Get-MainWindow $ad
    Close-Popups $ad $win
    $win
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
    $script:DownloadTo = Join-Path $DownloadRoot ('dl-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Force $DownloadTo | Out-Null
    & $ad.Ready $win 1
    Open-Folder $ad $win 0 $DownloadTo 0 | Out-Null
    # The folder above through the address bar, the folder itself by a double click (Air
    # Explorer's address bar stalls on a folder it hasn't listed yet).
    $up = Split-Path $d.Drive -Parent
    Open-Folder $ad $win 1 (& $ad.CloudPath $win 'drive' ($up -replace '\\', '/')) 0 -least 1 | Out-Null
    Open-Row $ad $win 1 (Split-Path $d.Drive -Leaf) 0 -least 1 | Out-Null
    Focus-Window $win
    & $ad.Focus $win 1
    Start-Sleep -Milliseconds 200
    [System.Windows.Forms.SendKeys]::SendWait('^a')
    Start-Sleep -Milliseconds 300
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $ad.CopyLeft $win
    $pasted = $false
    $first = $null
    while ($sw.Elapsed.TotalSeconds -lt $Timeout) {
        Close-Popups $ad $win
        $t = Tree-Of $DownloadTo
        if ($t.files -gt 0 -or $t.dirs -gt 0) { $pasted = $true }
        # Nothing arrived for a long while: the copy didn't start. Stopped here rather than
        # started over: deleting the folder under a copy that was only slow to begin breaks it.
        if (-not $pasted -and $sw.Elapsed.TotalSeconds -gt 30) {
            throw "$($ad.Name): nothing arrived in $DownloadTo 30 s after the copy of $($d.Drive)"
        }
        if ($pasted -and -not $first) { $first = $sw.Elapsed.TotalSeconds; Write-Host ("      first item on disk after {0:N2} s" -f $first) }
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
    # The other app closed: its window over this one's would take the clicks, and its keys.
    foreach ($other in $Adapters.Keys) { if ($other -ne $key) { Close-App $Adapters[$other] } }
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
    $win = Start-App $ad

    # Learn each folder's count (and warm the way there), then time it: opened before in this
    # session, and (for a cloud's) opened for the first time since the app started, with no
    # listing of it kept from before (`-uncached`).
    foreach ($name in $Folders.Keys) {
        $f = $Folders[$name]
        $warm = Want $name
        $cold = $f.Cloud -and (Want "$name-uncached")
        if (-not ($warm -or $cold)) { continue }
        # A cloud's folder opens in the app's cloud pane, between runs going back to the
        # benchmark's top folder there.
        if ($f.Local) { $path = $f.Local; $pane = 0; $back = $Rest }
        else { $back = & $ad.CloudPath $win $f.Cloud $Gd; $path = & $ad.CloudPath $win $f.Cloud $f.Path; $pane = $ad.CloudPane }
        Open-Folder $ad $win $pane $back 0 | Out-Null
        # The first time in, by a double click: through its address bar, Air Explorer takes far
        # longer for a cloud folder it hasn't listed yet.
        $learned = if ($f.Local) { Open-Folder $ad $win $pane $path 0 -least 1 } else { Open-Row $ad $win $pane (Split-Path $f.Path -Leaf) 0 -least 1 }
        $expected = $learned[1]
        if ($warm) {
            $times = @()
            for ($k = 0; $k -lt $Runs; $k++) {
                Open-Folder $ad $win $pane $back 0 | Out-Null
                $times += [math]::Round((Open-Folder $ad $win $pane $path $expected), 3)
            }
            Write-Host ("  {0,-20} {1,6} items  median {2:N3} s  ({3})" -f $name, $expected, (Median $times), ($times -join ', '))
            $r[$name] = @{ items = $expected; seconds = $times; median = (Median $times) }
        }
        if ($cold) {
            $times = @()
            for ($k = 0; $k -le $Runs; $k++) {
                # Left in the top folder, so the next start (Air Explorer reopens each tab's last
                # folder) doesn't load the measured one before it's asked for.
                Open-Folder $ad $win $pane (& $ad.CloudPath $win $f.Cloud $Gd) 0 -least 1 | Out-Null
                $win = Start-Uncached $ad
                # The way there (the account and the top folder) is the same in every run; then a
                # double click opens the folder, as a person would. (Air Explorer takes far longer
                # through its address bar: about 40 s for `5000 files` on OneDrive.)
                Open-Folder $ad $win $pane (& $ad.CloudPath $win $f.Cloud $Gd) 0 -least 1 | Out-Null
                $t = [math]::Round((Open-Row $ad $win $pane (Split-Path $f.Path -Leaf) $expected), 3)
                Write-Host ("    {0}-uncached run {1}: {2} s" -f $name, $k, $t)
                if ($k -gt 0) { $times += $t }
            }
            Write-Host ("  {0,-20} {1,6} items  median {2:N3} s  ({3})" -f "$name-uncached", $expected, (Median $times), ($times -join ', '))
            $r["$name-uncached"] = @{ items = $expected; seconds = $times; median = (Median $times) }
        }
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
        Open-Folder $ad $win $ad.CloudPane (& $ad.CloudPath $win 'drive' "$Gd/1000 folders") 0 | Out-Null
        Start-Sleep -Seconds 3
        $busy = Memory-Of $ad
        Write-Host ("  memory idle {0} MB private, {1} MB working set; with 1000 folders {2} / {3} MB ({4} processes)" -f $idle.private_mb, $idle.working_set_mb, $busy.private_mb, $busy.working_set_mb, $busy.processes)
        $r['memory_idle'] = $idle
        $r["memory_1000_folders"] = $busy
    }

    $results.apps[$key] = $r
}

foreach ($key in $AppList) { Close-App $Adapters[$key] }
Get-ChildItem -LiteralPath $DownloadRoot -Directory -Filter 'dl-*' -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
$results | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 $Out
Write-Host "results -> $Out"
