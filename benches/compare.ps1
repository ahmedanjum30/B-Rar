<#
.SYNOPSIS
  Times `brar x` against WinRAR and bsdtar (Windows' built-in tar.exe) on archives built from
  a corpus directory, and checks every tool extracts the same file count and byte total.
.EXAMPLE
  ./benches/compare.ps1 -Corpus C:\some\folder -Runs 3 -Formats zip,rar
#>
param(
    [Parameter(Mandatory)] [string] $Corpus,
    [string] $Work = (Join-Path $PSScriptRoot '..\bench-data'),
    [string] $WinRarDir = 'C:\Program Files\WinRAR',
    [string] $Brar = (Join-Path $PSScriptRoot '..\target\release\brar.exe'),
    [int] $Runs = 2,
    [string[]] $Formats = @('zip', 'rar', '7z', 'tar', 'tar.gz', 'tar.xz', 'tar.zst')
)
# Native tools report failure via exit codes (see Check); stderr warnings are not fatal.
$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force $Work | Out-Null
$Work = (Resolve-Path $Work).Path
$Corpus = (Resolve-Path $Corpus).Path
$parent = Split-Path $Corpus -Parent
$leaf = Split-Path $Corpus -Leaf
$rar = Join-Path $WinRarDir 'Rar.exe'
$unrar = Join-Path $WinRarDir 'UnRAR.exe'
$winrar = Join-Path $WinRarDir 'WinRAR.exe'
$threads = [Environment]::ProcessorCount

function Check([string] $what) { if ($LASTEXITCODE -ne 0) { throw "$what failed ($LASTEXITCODE)" } }

function Build-Archive([string] $format) {
    $out = Join-Path $Work "corpus.$format"
    if (Test-Path $out) { return $out }
    Write-Host "building $out ..."
    Push-Location $parent
    try {
        switch ($format) {
            'rar'     { & $rar a -m3 "-mt$threads" -r -idq $out $leaf; Check 'rar' }
            'zip'     { $p = Start-Process $winrar -ArgumentList @('a', '-afzip', '-m3', '-r', '-ibck', "`"$out`"", "`"$leaf`"") -Wait -PassThru
                        if ($p.ExitCode -ne 0) { throw 'WinRAR zip failed' } }
            '7z'      { tar --format 7zip --options 7zip:compression=lzma2 -cf $out $leaf; Check '7z' }
            'tar'     { tar -cf $out $leaf; Check 'tar' }
            'tar.gz'  { tar -czf $out $leaf; Check 'tar.gz' }
            'tar.xz'  { tar -cJf $out --options "xz:threads=$threads" $leaf; Check 'tar.xz' }
            'tar.zst' { tar --zstd -cf $out $leaf; Check 'tar.zst' }
        }
    } finally { Pop-Location }
    $out
}

function Time-Run([string] $dest, [scriptblock] $cmd) {
    if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
    New-Item -ItemType Directory $dest | Out-Null
    $t = Measure-Command { & $cmd | Out-Null }
    $t.TotalSeconds
}

function Tree-Stats([string] $dir) {
    $files = Get-ChildItem -LiteralPath $dir -Recurse -File -Force
    '{0} files / {1:N0} bytes' -f $files.Count, ($files | Measure-Object Length -Sum).Sum
}

$tools = [ordered]@{
    brar   = { param($a, $d) & $Brar x $a -o $d; Check 'brar' }
    WinRAR = { param($a, $d)
               if ($a.EndsWith('.rar')) { & $unrar x -y -idq $a "$d\"; Check 'unrar' }
               else { $p = Start-Process $winrar -ArgumentList @('x', '-y', '-ibck', "`"$a`"", "`"$d\`"") -Wait -PassThru
                      if ($p.ExitCode -ne 0) { throw "WinRAR failed ($($p.ExitCode))" } } }
    bsdtar = { param($a, $d) tar -xf $a -C $d; Check 'bsdtar' }
}

$results = foreach ($format in $Formats) {
    $archive = Build-Archive $format
    $best = @{}
    for ($i = 0; $i -lt $Runs; $i++) {
        foreach ($name in $tools.Keys) {
            $dest = Join-Path $Work "out-$name"
            $t = Time-Run $dest { & $tools[$name] $archive $dest }
            if (-not $best.ContainsKey($name) -or $t -lt $best[$name]) { $best[$name] = $t }
        }
    }
    # Compare the last extraction of each tool against brar's.
    $ref = Tree-Stats (Join-Path $Work 'out-brar')
    $mismatch = @($tools.Keys | Where-Object { $_ -ne 'brar' -and (Tree-Stats (Join-Path $Work "out-$_")) -ne $ref })
    $fastestOther = ($tools.Keys | Where-Object { $_ -ne 'brar' } | ForEach-Object { $best[$_] } | Measure-Object -Minimum).Minimum
    [pscustomobject]@{
        Format     = $format
        'MB'       = [math]::Round((Get-Item $archive).Length / 1MB)
        'brar s'   = [math]::Round($best['brar'], 2)
        'WinRAR s' = [math]::Round($best['WinRAR'], 2)
        'bsdtar s' = [math]::Round($best['bsdtar'], 2)
        'vs best'  = '{0:N1}x' -f ($fastestOther / $best['brar'])
        Output     = if ($mismatch) { "DIFFERS: $($mismatch -join ',') (brar: $ref)" } else { "identical ($ref)" }
    }
}
$results | Format-Table -AutoSize | Out-String -Width 250
