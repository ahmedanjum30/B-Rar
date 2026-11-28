# Compares B-Rar builds with each other (not with other archivers; that's compare.ps1) on the
# archives in bench-data: each archive, every build in rotating order, -Runs times each. Per
# run: the time, B-Rar's own peak memory (private bytes), the rise in the system's memory in
# use, and the files and bytes written (which must match between builds).
#
#   powershell -ExecutionPolicy Bypass -File benches\builds.ps1 `
#       -Builds @{ '1.5.3' = 'C:\builds\1.5.3\B-Rar.exe'; now = 'target\release\B-Rar.exe' }
#
# Build an older version from its tag in a separate folder, with its own target directory:
#   git worktree add --detach ..\brar-v1.5.3 v1.5.3; cd ..\brar-v1.5.3
#   $env:CARGO_TARGET_DIR = '..\brar-v1.5.3-target'; cargo build --release -p brar-cli
#
# The archives: bench-data's ZIP, RAR, 7z and tar benchmarks, bench-data\zip\big-entry.zip (one
# 1.15 GB Deflate entry) and small-files.zip (30,000 files), plus any -Extra archives (a big
# real one, say). Missing ones are skipped. Output goes to -Out, deleted before each run; put it
# on another drive than bench-data to time reading and writing on separate drives. Turn
# Defender's real-time protection off, or log that it's on (it slows unpackaged builds).
param(
    [Parameter(Mandatory)] [System.Collections.IDictionary] $Builds,
    [int] $Runs = 3,
    [string] $Out = 'C:\brar-bench-out',
    [string[]] $Extra = @(),
    [string] $Csv
)
$bd = (Resolve-Path (Join-Path $PSScriptRoot '..\bench-data')).Path
$archives = @(
    "$bd\italian.zip", "$bd\single.zip", "$bd\corpus.zip",
    "$bd\zip\big-entry.zip", "$bd\zip\small-files.zip",
    "$bd\italian-repack.rar", "$bd\solid\game-solid.rar", "$bd\7z\mt.7z", "$bd\tars\c.tar"
) + $Extra
foreach ($b in @($Builds.Keys)) {
    if (-not (Test-Path $Builds[$b])) { throw "build '$b' not found: $($Builds[$b])" }
}
try { "Defender real-time protection: " + (Get-MpComputerStatus).RealTimeProtectionEnabled } catch {}

function InUse {
    $os = Get-CimInstance Win32_OperatingSystem
    [long](($os.TotalVisibleMemorySize - $os.FreePhysicalMemory) * 1KB)
}
Remove-Item Env:BRAR_DISK, Env:BRAR_BUDGET_MB -ErrorAction SilentlyContinue
$names = @($Builds.Keys)
$rows = @()
foreach ($archive in $archives) {
    if (-not (Test-Path $archive)) { "skipped (not found): $archive"; continue }
    for ($r = 0; $r -lt $Runs; $r++) {
        # Rotate the order each round, so no build always goes first.
        $k = $r % $names.Count
        $order = @($names[$k..($names.Count - 1)]) + @(if ($k -gt 0) { $names[0..($k - 1)] })
        foreach ($b in $order) {
            Remove-Item $Out -Recurse -Force -ErrorAction SilentlyContinue
            Start-Sleep 3
            [long]$base = InUse
            $sw = [Diagnostics.Stopwatch]::StartNew()
            $p = Start-Process $Builds[$b] -ArgumentList 'x', "`"$archive`"", '-o', "`"$Out`"", '-y' -PassThru -WindowStyle Hidden
            [long]$priv = 0; [long]$use = 0
            while (-not $p.HasExited) {
                try { $p.Refresh(); $priv = [Math]::Max($priv, [long]$p.PrivateMemorySize64) } catch {}
                $use = [Math]::Max($use, (InUse))
                Start-Sleep -Milliseconds 100
            }
            $sw.Stop(); $p.WaitForExit()
            $files = @(Get-ChildItem $Out -Recurse -File -Force -ErrorAction SilentlyContinue)
            $rows += [pscustomobject]@{
                Archive = Split-Path $archive -Leaf; Build = $b; Run = $r + 1; Exit = $p.ExitCode
                Seconds = [Math]::Round($sw.Elapsed.TotalSeconds, 2)
                OwnGB = [Math]::Round($priv / 1GB, 2); InUseGB = [Math]::Round(($use - $base) / 1GB, 2)
                Files = $files.Count; Bytes = ($files | Measure-Object Length -Sum).Sum
            }
        }
    }
}
Remove-Item $Out -Recurse -Force -ErrorAction SilentlyContinue
if ($Csv) { $rows | Export-Csv -NoTypeInformation -Path $Csv }

"===== per archive and build: best and median time, own memory peak, rise in memory in use"
foreach ($g in ($rows | Group-Object Archive)) {
    $outputs = @($g.Group | Select-Object Files, Bytes -Unique)
    $same = if ($outputs.Count -eq 1) { 'same output' } else { 'OUTPUT DIFFERS: ' + (($outputs | ForEach-Object { "$($_.Files) files / $($_.Bytes) B" }) -join ' vs ') }
    $fails = @($g.Group | Where-Object Exit -ne 0)
    "-- $($g.Name)  ($same$(if ($fails) { ', FAILED RUNS: ' + $fails.Count }))"
    foreach ($bg in ($g.Group | Group-Object Build)) {
        $t = @($bg.Group.Seconds | Sort-Object)
        "   {0,-8} best {1,7:N2} s  median {2,7:N2} s   own {3,5:N2} GB   in use +{4,5:N2} GB   runs: {5}" -f $bg.Name, $t[0], $t[[int][Math]::Floor($t.Count / 2)], ($bg.Group.OwnGB | Measure-Object -Maximum).Maximum, ($bg.Group.InUseGB | Measure-Object -Maximum).Maximum, ($t -join ', ')
    }
}
