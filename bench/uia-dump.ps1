# Writes the UI Automation tree of a running app's windows to a text file, read only: it
# clicks and types nothing. Used to find how the speed comparison reaches each app's address
# bar, file list and status bar.
#
#   powershell -NoProfile -File bench\uia-dump.ps1 -Process openexplorer -Out uia-openexplorer.txt
#   powershell -NoProfile -File bench\uia-dump.ps1 -Process AirExplorer -Out uia-airexplorer.txt
#
# Windows PowerShell 5.1 (powershell.exe), which has .NET Framework's UIAutomationClient.

param(
    [Parameter(Mandatory)] [string] $Process,
    [Parameter(Mandatory)] [string] $Out,
    [int] $Depth = 14,
    # Every element, as Inspect.exe's raw view shows them; WinForms and WPF hosts hide much from
    # the default control view.
    [switch] $Raw
)

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$walker = if ($Raw) { [System.Windows.Automation.TreeWalker]::RawViewWalker } else { [System.Windows.Automation.TreeWalker]::ControlViewWalker }
$lines = New-Object System.Collections.Generic.List[string]

function Write-Node($el, $level) {
    if ($level -gt $Depth) { return }
    try {
        $c = $el.Current
        $kind = $c.ControlType.ProgrammaticName -replace '^ControlType\.', ''
        $value = ''
        $p = $null
        if ($el.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref] $p)) { $value = " value='$($p.Current.Value)'" }
        $off = if ($c.IsOffscreen) { ' offscreen' } else { '' }
        $lines.Add(('  ' * $level) + "$kind name='$($c.Name)' id='$($c.AutomationId)' class='$($c.ClassName)'$value$off")
    } catch { $lines.Add(('  ' * $level) + "(gone)"); return }
    $child = $walker.GetFirstChild($el)
    $n = 0
    while ($child -and $n -lt 60) {
        Write-Node $child ($level + 1)
        $child = $walker.GetNextSibling($child)
        $n++
    }
    if ($child) { $lines.Add(('  ' * ($level + 1)) + "...") }
}

$procs = Get-Process -Name $Process -ErrorAction SilentlyContinue
if (-not $procs) { throw "$Process isn't running" }
$root = [System.Windows.Automation.AutomationElement]::RootElement
foreach ($proc in $procs) {
    $cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ProcessIdProperty, $proc.Id)
    foreach ($w in $root.FindAll([System.Windows.Automation.TreeScope]::Children, $cond)) {
        $lines.Add("=== window '$($w.Current.Name)' pid $($proc.Id)")
        Write-Node $w 0
    }
}
[System.IO.File]::WriteAllLines($Out, $lines)
"$($lines.Count) lines -> $Out"
