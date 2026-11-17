# Regenerates the app icon (assets\explorer-app.ico, assets\explorer-app.png) and the MSIX logos
# (packaging\msix\Assets) from one vector drawing: a flat Fluent-style folder.
#
#   .\packaging\make-assets.ps1
Add-Type -AssemblyName System.Drawing
$root = Split-Path $PSScriptRoot -Parent

function RoundRect([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90); $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90); $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure(); $p
}

# Draws the folder into a square of side $s at ($ox, $oy) on a 16-unit grid: a flat two-tone
# Fluent folder (back with tab, lighter front), no gradients or badges so it stays crisp at 16 px.
function DrawFolder($g, [float]$s, [float]$ox, [float]$oy) {
    $u = $s / 16.0
    $back = New-Object System.Drawing.Drawing2D.GraphicsPath
    $back.FillMode = 'Winding'
    $back.AddPath((RoundRect ($ox + 1*$u) ($oy + 2.5*$u) (6*$u) (3*$u) (1*$u)), $false)
    $back.AddPath((RoundRect ($ox + 1*$u) ($oy + 3.5*$u) (14*$u) (10*$u) (1.2*$u)), $false)
    $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 226, 158, 24))), $back)
    $front = RoundRect ($ox + 1*$u) ($oy + 5.5*$u) (14*$u) (8*$u) (1.2*$u)
    $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 255, 200, 61))), $front)
}

# A transparent PNG of size $w x $h with the folder drawn at $scale of the shorter side, centered.
function Render([int]$w, [int]$h, [float]$scale) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'; $g.PixelOffsetMode = 'HighQuality'
    $g.Clear([System.Drawing.Color]::Transparent)
    $s = [Math]::Min($w, $h) * $scale
    DrawFolder $g $s (($w - $s) / 2) (($h - $s) / 2)
    $g.Dispose()
    $bmp
}

function SavePng($bmp, $path) {
    New-Item -ItemType Directory -Force (Split-Path $path) | Out-Null
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
}

# App icon: multi-size .ico with PNG frames.
$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$frames = foreach ($s in $sizes) {
    $bmp = Render $s $s 1.0
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    , $ms.ToArray()
}
$out = New-Object System.IO.MemoryStream; $wr = New-Object System.IO.BinaryWriter $out
$wr.Write([uint16]0); $wr.Write([uint16]1); $wr.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]; $d = $frames[$i]
    $wr.Write([byte]($s % 256)); $wr.Write([byte]($s % 256)); $wr.Write([byte]0); $wr.Write([byte]0)
    $wr.Write([uint16]1); $wr.Write([uint16]32); $wr.Write([uint32]$d.Length); $wr.Write([uint32]$offset)
    $offset += $d.Length
}
foreach ($d in $frames) { $wr.Write($d) }
[System.IO.File]::WriteAllBytes("$root\assets\explorer-app.ico", $out.ToArray())
SavePng (Render 256 256 1.0) "$root\assets\explorer-app.png"

# MSIX logos (names and sizes as Windows expects them).
$a = "$root\packaging\msix\Assets"
SavePng (Render 150 150 0.66) "$a\Square150x150Logo.scale-100.png"
SavePng (Render 300 300 0.66) "$a\Square150x150Logo.scale-200.png"
SavePng (Render 44 44 0.9) "$a\Square44x44Logo.scale-100.png"
SavePng (Render 88 88 0.9) "$a\Square44x44Logo.scale-200.png"
foreach ($t in 16, 24, 32, 48, 256) {
    SavePng (Render $t $t 1.0) "$a\Square44x44Logo.targetsize-$t.png"
    SavePng (Render $t $t 1.0) "$a\Square44x44Logo.targetsize-${t}_altform-unplated.png"
}
SavePng (Render 50 50 0.9) "$a\StoreLogo.scale-100.png"
SavePng (Render 100 100 0.9) "$a\StoreLogo.scale-200.png"
Write-Host "Assets written"
