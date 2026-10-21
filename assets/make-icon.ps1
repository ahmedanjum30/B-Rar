# Renders B-Rar's icons from the artwork in logo-source.png (1254 px, transparent corners):
# assets\brar.ico, assets\brar-256.png, assets\brar-full-512.png and the MSIX logos in
# assets\msix. Re-run after changing the artwork.
#
# Two crops of the artwork:
#   full  the whole tile with the "B-Rar" wordmark: 150 px and up, where the text is readable.
#   mark  just the clamp-B on the navy tile: everything smaller (taskbar, title bar, Explorer).
# "Plated" MSIX logos sit on a plate in the user's accent color, so they're drawn edge to edge
# on navy; "unplated" ones and the .ico keep the rounded tile with transparent corners.
Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = 'Stop'

$navy = [Drawing.Color]::FromArgb(255, 2, 16, 33)   # the tile's own background color
$fullBox = [Drawing.Rectangle]::new(46, 47, 1160, 1160)
$markBox = [Drawing.Rectangle]::new(238, 115, 780, 780)

function Get-Pixels([Drawing.Bitmap] $b) {
    $data = $b.LockBits([Drawing.Rectangle]::new(0, 0, $b.Width, $b.Height), 'ReadWrite', 'Format32bppArgb')
    $bytes = New-Object byte[] ($data.Stride * $b.Height)
    [Runtime.InteropServices.Marshal]::Copy([IntPtr]$data.Scan0, [byte[]]$bytes, [int]0, [int]$bytes.Length)
    @{ Data = $data; Bytes = $bytes }
}
function Set-Pixels([Drawing.Bitmap] $b, $data, [byte[]] $bytes) {
    [Runtime.InteropServices.Marshal]::Copy([byte[]]$bytes, [int]0, [IntPtr]$data.Scan0, [int]$bytes.Length)
    $b.UnlockBits($data)
}

# The artwork with its faint haze removed: a soft glow around the tile and a stray streak
# along the top edge, nearly transparent pixels that smudge small sizes.
$source = [Drawing.Bitmap]::new((Join-Path $PSScriptRoot 'logo-source.png'))
$art = [Drawing.Bitmap]::new($source.Width, $source.Height, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [Drawing.Graphics]::FromImage($art); $g.DrawImage($source, 0, 0, $source.Width, $source.Height); $g.Dispose(); $source.Dispose()
$p = Get-Pixels $art; $bytes = $p.Bytes
for ($i = 3; $i -lt $bytes.Length; $i += 4) { if ($bytes[$i] -lt 24) { $bytes[$i] = 0 } }
Set-Pixels $art $p.Data $bytes

function Crop([Drawing.Rectangle] $box) {
    $b = [Drawing.Bitmap]::new($box.Width, $box.Height, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($b)
    $g.DrawImage($art, [Drawing.Rectangle]::new(0, 0, $box.Width, $box.Height), $box, 'Pixel'); $g.Dispose()
    $b
}

# Downscales in halving steps: one big bicubic step from 780 px to 16 px aliases badly.
function Scale([Drawing.Bitmap] $b, [int] $s) {
    $cur = $b
    while ($cur.Width -gt 2 * $s) {
        $half = [Drawing.Bitmap]::new([int]($cur.Width / 2), [int]($cur.Height / 2), [Drawing.Imaging.PixelFormat]::Format32bppArgb)
        $g = [Drawing.Graphics]::FromImage($half); $g.InterpolationMode = 'HighQualityBicubic'; $g.PixelOffsetMode = 'HighQuality'
        $g.DrawImage($cur, 0, 0, $half.Width, $half.Height); $g.Dispose()
        if ($cur -ne $b) { $cur.Dispose() }
        $cur = $half
    }
    $out = [Drawing.Bitmap]::new($s, $s, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($out); $g.InterpolationMode = 'HighQualityBicubic'; $g.PixelOffsetMode = 'HighQuality'
    $g.DrawImage($cur, 0, 0, $s, $s); $g.Dispose()
    if ($cur -ne $b) { $cur.Dispose() }
    $out
}

# Rounded tile: the image's alpha times an antialiased rounded-square mask.
function Round([Drawing.Bitmap] $b) {
    $s = $b.Width; $r = $s * 0.22; $pad = [Math]::Max(0.5, $s * 0.02); $w = $s - 2 * $pad
    $mask = [Drawing.Bitmap]::new($s, $s, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($mask); $g.SmoothingMode = 'AntiAlias'
    $path = New-Object Drawing.Drawing2D.GraphicsPath
    $path.AddArc($pad, $pad, 2 * $r, 2 * $r, 180, 90); $path.AddArc($pad + $w - 2 * $r, $pad, 2 * $r, 2 * $r, 270, 90)
    $path.AddArc($pad + $w - 2 * $r, $pad + $w - 2 * $r, 2 * $r, 2 * $r, 0, 90); $path.AddArc($pad, $pad + $w - 2 * $r, 2 * $r, 2 * $r, 90, 90)
    $path.CloseFigure(); $g.FillPath([Drawing.Brushes]::White, $path); $g.Dispose()
    $p1 = Get-Pixels $b; $p2 = Get-Pixels $mask; $px = $p1.Bytes; $m = $p2.Bytes
    for ($i = 3; $i -lt $px.Length; $i += 4) { $px[$i] = [byte]([int]$px[$i] * $m[$i] / 255) }
    Set-Pixels $mask $p2.Data $m; $mask.Dispose()
    Set-Pixels $b $p1.Data $px
    $b
}

# Edge to edge on navy, for plated logos.
function Bleed([Drawing.Bitmap] $b) {
    $out = [Drawing.Bitmap]::new($b.Width, $b.Height, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($out); $g.Clear($navy); $g.DrawImage($b, 0, 0, $b.Width, $b.Height); $g.Dispose()
    $b.Dispose(); $out
}

$full = Crop $fullBox
$mark = Crop $markBox
function Icon([int] $s, [switch] $Plated) {
    $b = if ($s -ge 150) { Scale $full $s } else { Scale $mark $s }
    # The full crop already has the artwork's own rounded corners; the mark needs them.
    if ($Plated) { $b = Bleed $b } elseif ($s -lt 150) { $b = Round $b }
    $ms = New-Object IO.MemoryStream; $b.Save($ms, [Drawing.Imaging.ImageFormat]::Png); $b.Dispose()
    , $ms.ToArray()
}

# Windows .ico: PNG-compressed entries, 16 to 256 px.
$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$pngs = foreach ($s in $sizes) { Icon $s }
$out = New-Object IO.MemoryStream; $bw = New-Object IO.BinaryWriter $out
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]; $d = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$d); $bw.Write([byte]$d); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([uint16]1); $bw.Write([uint16]32); $bw.Write([uint32]$pngs[$i].Length); $bw.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $bw.Write($p) }
[IO.File]::WriteAllBytes((Join-Path $PSScriptRoot 'brar.ico'), $out.ToArray())
# The window icon and the in-app logo are shown small: the mark, not the wordmark.
$m = Round (Scale $mark 256); $m.Save((Join-Path $PSScriptRoot 'brar-256.png')); $m.Dispose()
# For the website, README and store listing.
$f = Scale $full 512; $f.Save((Join-Path $PSScriptRoot 'brar-full-512.png')); $f.Dispose()

# MSIX logos. "targetsize" variants are used for the taskbar, Settings and file lists;
# "altform-unplated" ones are drawn without a plate behind them.
$msix = Join-Path $PSScriptRoot 'msix'
New-Item -ItemType Directory -Force $msix | Out-Null
$plated = @{ 'Square44x44Logo.png' = 44; 'Square150x150Logo.png' = 150; 'StoreLogo.png' = 50 }
$unplated = @{}
foreach ($t in 16, 24, 32, 48, 256) {
    $plated["Square44x44Logo.targetsize-$t.png"] = $t
    $unplated["Square44x44Logo.targetsize-${t}_altform-unplated.png"] = $t
}
foreach ($name in $plated.Keys) { [IO.File]::WriteAllBytes((Join-Path $msix $name), (Icon $plated[$name] -Plated)) }
foreach ($name in $unplated.Keys) { [IO.File]::WriteAllBytes((Join-Path $msix $name), (Icon $unplated[$name])) }
