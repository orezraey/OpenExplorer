# Prints how many items each of a running app's Win32 list views holds (LVM_GETITEMCOUNT), read
# only. Air Explorer's file lists are standard SysListView32 controls, so this is how the speed
# comparison sees a folder finish opening there.
#
#   powershell -NoProfile -File bench\list-count.ps1 -Process AirExplorer

param([Parameter(Mandatory)] [string] $Process)

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Lv {
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, int msg, IntPtr w, IntPtr l);
    public static int Count(IntPtr h) { return (int)SendMessage(h, 0x1004, IntPtr.Zero, IntPtr.Zero); }
}
"@

$A = [System.Windows.Automation.AutomationElement]
$proc = Get-Process -Name $Process -ErrorAction Stop | Select-Object -First 1
$cond = New-Object System.Windows.Automation.AndCondition(
    (New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $proc.Id)),
    (New-Object System.Windows.Automation.PropertyCondition($A::ClassNameProperty, 'WindowsForms10.SysListView32.app.0.25bb5ff_r6_ad1')))
foreach ($w in $A::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children, (New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $proc.Id)))) {
    foreach ($list in $w.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond)) {
        $r = $list.Current.BoundingRectangle
        "list at x=$([int]$r.X) y=$([int]$r.Y): $([Lv]::Count([IntPtr]$list.Current.NativeWindowHandle)) items"
    }
}
