# Draws the README's speed chart from results\comparison.csv: a light and a dark SVG for each
# README (English and Portuguese), into docs\bench. Run again after the CSV changes.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File bench\readme-chart.ps1

$ErrorActionPreference = 'Stop'
$rows = Import-Csv (Join-Path $PSScriptRoot 'results\comparison.csv')
$out = Join-Path $PSScriptRoot '..\docs\bench'
New-Item -ItemType Directory -Force $out | Out-Null

# The measurements the chart shows, by group, measurement and place, with their labels. The last
# is the closest result, shown so the chart isn't only the widest gaps.
$picks = @(
    @{ Key = 'The app|Start|This PC'; En = 'Start the app'; Pt = 'Abrir o app'; PlaceEn = 'Windows'; PlacePt = 'Windows' }
    @{ Key = 'Cloud folder, first open|5,000 files|Google Drive'; En = 'Open 5,000 files, first time'; Pt = 'Abrir 5.000 arquivos, 1ª vez' }
    @{ Key = 'Cloud folder, first open|5,000 files|OneDrive'; En = 'Open 5,000 files, first time'; Pt = 'Abrir 5.000 arquivos, 1ª vez' }
    @{ Key = 'Download|500 MiB file|OneDrive'; En = 'Download a 500 MiB file'; Pt = 'Baixar um arquivo de 500 MiB' }
    @{ Key = 'Download|2 GiB file|Google Drive'; En = 'Download a 2 GiB file'; Pt = 'Baixar um arquivo de 2 GiB' }
    @{ Key = 'Download|100 folders|MEGA'; En = 'Download 100 folders'; Pt = 'Baixar 100 pastas' }
    @{ Key = 'Upload|100 files of 512 KiB|Dropbox'; En = 'Upload 100 files of 512 KiB'; Pt = 'Enviar 100 arquivos de 512 KiB' }
    @{ Key = 'Upload|100 MiB file|MEGA'; En = 'Upload a 100 MiB file'; Pt = 'Enviar um arquivo de 100 MiB' }
    @{ Key = 'Download|500 MiB file|Dropbox'; En = 'Download a 500 MiB file'; Pt = 'Baixar um arquivo de 500 MiB'; PlaceEn = 'Dropbox, the closest result'; PlacePt = 'Dropbox, o resultado mais apertado' }
)

$themes = @{
    light = @{ Fg = '#1f2328'; Muted = '#59636e'; Line = '#d1d9e0'; Oe = '#0e7a6e'; Air = '#b4bcc6'; Win = '#0e7a6e' }
    dark  = @{ Fg = '#e6edf3'; Muted = '#9198a1'; Line = '#3d444d'; Oe = '#3fc8b4'; Air = '#4c5561'; Win = '#3fc8b4' }
}
$words = @{
    en = @{ Oe = 'Open Explorer'; Air = 'Air Explorer 6.0 (free)'; Faster = '× faster'; Note = 'Seconds to finish, median of the runs. Shorter is better.'; Comma = $false; Alt = 'Time to finish, Open Explorer against Air Explorer' }
    pt = @{ Oe = 'Open Explorer'; Air = 'Air Explorer 6.0 (grátis)'; Faster = '× mais rápido'; Note = 'Segundos até terminar, mediana das medições. Menor é melhor.'; Comma = $true; Alt = 'Tempo até terminar, Open Explorer contra Air Explorer' }
}

function Esc([string] $s) { [System.Security.SecurityElement]::Escape($s) }
function Num([double] $v, [int] $digits, [bool] $comma) {
    $s = $v.ToString("F$digits", [System.Globalization.CultureInfo]::InvariantCulture)
    if ($comma) { $s = $s.Replace('.', ',') }
    $s
}
function Seconds([double] $v, [bool] $comma) {
    if ($v -lt 1) { return (Num $v 2 $comma) + ' s' }
    if ($v -lt 10) { return (Num $v 1 $comma) + ' s' }
    (Num $v 0 $comma) + ' s'
}

$width = 880; $labelW = 290; $barX = 300; $barW = 400; $rowH = 62; $top = 52
foreach ($lang in 'en', 'pt') {
    $w = $words[$lang]
    foreach ($theme in 'light', 'dark') {
        $c = $themes[$theme]
        $height = $top + $picks.Count * $rowH + 40
        $svg = New-Object System.Text.StringBuilder
        [void]$svg.Append("<svg xmlns=`"http://www.w3.org/2000/svg`" width=`"$width`" height=`"$height`" viewBox=`"0 0 $width $height`" role=`"img`" aria-label=`"$(Esc $w.Alt)`">")
        [void]$svg.Append("<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}.l{font-size:15px;fill:$($c.Fg)}.p{font-size:12px;fill:$($c.Muted)}.v{font-size:12px;fill:$($c.Muted);font-variant-numeric:tabular-nums}.x{font-size:15px;font-weight:600;fill:$($c.Win);font-variant-numeric:tabular-nums}</style>")
        # Legend.
        [void]$svg.Append("<rect x=`"0`" y=`"10`" width=`"14`" height=`"10`" rx=`"2`" fill=`"$($c.Oe)`"/><text class=`"p`" x=`"20`" y=`"19`">$(Esc $w.Oe)</text>")
        [void]$svg.Append("<rect x=`"140`" y=`"10`" width=`"14`" height=`"10`" rx=`"2`" fill=`"$($c.Air)`"/><text class=`"p`" x=`"160`" y=`"19`">$(Esc $w.Air)</text>")
        $y = $top
        foreach ($p in $picks) {
            $r = $rows | Where-Object { "$($_.group)|$($_.measurement)|$($_.place)" -eq $p.Key } | Select-Object -First 1
            if (-not $r) { throw "no row $($p.Key) in comparison.csv" }
            $oe = [double]::Parse($r.open_explorer, [System.Globalization.CultureInfo]::InvariantCulture)
            $air = [double]::Parse($r.air_explorer, [System.Globalization.CultureInfo]::InvariantCulture)
            $max = [Math]::Max($oe, $air)
            $label = if ($lang -eq 'en') { $p.En } else { $p.Pt }
            $place = if ($lang -eq 'en' -and $p.PlaceEn) { $p.PlaceEn } elseif ($lang -eq 'pt' -and $p.PlacePt) { $p.PlacePt } else { $r.place }
            [void]$svg.Append("<line x1=`"0`" x2=`"$width`" y1=`"$($y - 8)`" y2=`"$($y - 8)`" stroke=`"$($c.Line)`" stroke-width=`"1`"/>")
            [void]$svg.Append("<text class=`"l`" x=`"0`" y=`"$($y + 16)`">$(Esc $label)</text><text class=`"p`" x=`"0`" y=`"$($y + 34)`">$(Esc $place)</text>")
            $i = 0
            foreach ($v in $oe, $air) {
                $bw = [Math]::Max(2, [Math]::Round($v / $max * $barW, 1))
                $by = $y + 4 + $i * 20
                $fill = if ($i -eq 0) { $c.Oe } else { $c.Air }
                [void]$svg.Append("<rect x=`"$barX`" y=`"$by`" width=`"$bw`" height=`"14`" rx=`"2`" fill=`"$fill`"/>")
                [void]$svg.Append("<text class=`"v`" x=`"$($barX + $bw + 6)`" y=`"$($by + 11)`">$(Seconds $v $w.Comma)</text>")
                $i++
            }
            $x = $air / $oe
            $times = if ($x -ge 10) { Num $x 0 $w.Comma } else { Num $x 1 $w.Comma }
            [void]$svg.Append("<text class=`"x`" x=`"$width`" y=`"$($y + 24)`" text-anchor=`"end`">$times$(Esc $w.Faster)</text>")
            $y += $rowH
        }
        [void]$svg.Append("<text class=`"p`" x=`"0`" y=`"$($y + 14)`">$(Esc $w.Note)</text>")
        [void]$svg.Append('</svg>')
        $file = Join-Path $out "speed-$lang-$theme.svg"
        [System.IO.File]::WriteAllText($file, $svg.ToString(), (New-Object System.Text.UTF8Encoding $false))
        Write-Host "-> $file"
    }
}
