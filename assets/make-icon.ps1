# Renders brar's icon: assets\brar.ico (16-256 px, PNG-compressed entries), assets\brar-256.png
# and the MSIX logos in assets\msix. Re-run after changing the design.
Add-Type -AssemblyName System.Drawing

function Render([int] $s) {
    $bmp = New-Object Drawing.Bitmap $s, $s
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'; $g.TextRenderingHint = 'AntiAliasGridFit'; $g.InterpolationMode = 'HighQualityBicubic'
    $pad = [Math]::Max(0.5, $s * 0.04); $r = $s * 0.22; $w = $s - 2 * $pad
    $path = New-Object Drawing.Drawing2D.GraphicsPath
    $path.AddArc($pad, $pad, 2 * $r, 2 * $r, 180, 90); $path.AddArc($pad + $w - 2 * $r, $pad, 2 * $r, 2 * $r, 270, 90)
    $path.AddArc($pad + $w - 2 * $r, $pad + $w - 2 * $r, 2 * $r, 2 * $r, 0, 90); $path.AddArc($pad, $pad + $w - 2 * $r, 2 * $r, 2 * $r, 90, 90)
    $path.CloseFigure()
    $rect = New-Object Drawing.RectangleF $pad, $pad, $w, $w
    $brush = New-Object Drawing.Drawing2D.LinearGradientBrush $rect, ([Drawing.Color]::FromArgb(255, 37, 99, 235)), ([Drawing.Color]::FromArgb(255, 14, 165, 233)), 45.0
    $g.FillPath($brush, $path)
    $font = New-Object Drawing.Font 'Segoe UI', ([float]($s * 0.62)), ([Drawing.FontStyle]::Bold), ([Drawing.GraphicsUnit]::Pixel)
    $fmt = New-Object Drawing.StringFormat; $fmt.Alignment = 'Center'; $fmt.LineAlignment = 'Center'
    $g.DrawString('B', $font, [Drawing.Brushes]::White, (New-Object Drawing.RectangleF 0, ($s * 0.02), $s, $s), $fmt)
    $g.Dispose()
    $ms = New-Object IO.MemoryStream; $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    , $ms.ToArray()
}

# Windows .ico
$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$pngs = foreach ($s in $sizes) { Render $s }
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
[IO.File]::WriteAllBytes((Join-Path $PSScriptRoot 'brar-256.png'), $pngs[-1])

# MSIX logos. "targetsize" variants are used for the taskbar, Settings and file lists;
# "altform-unplated" ones are drawn without a colored plate behind them.
$msix = Join-Path $PSScriptRoot 'msix'
New-Item -ItemType Directory -Force $msix | Out-Null
$logos = @{ 'Square44x44Logo.png' = 44; 'Square150x150Logo.png' = 150; 'StoreLogo.png' = 50 }
foreach ($t in 16, 24, 32, 48, 256) {
    $logos["Square44x44Logo.targetsize-$t.png"] = $t
    $logos["Square44x44Logo.targetsize-${t}_altform-unplated.png"] = $t
}
foreach ($name in $logos.Keys) { [IO.File]::WriteAllBytes((Join-Path $msix $name), (Render $logos[$name])) }
