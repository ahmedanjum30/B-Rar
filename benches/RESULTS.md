# Benchmark results

Extraction benchmarks of brar against other archivers. Newest first. Every run checked that
brar's output is identical to WinRAR's (names, sizes, modification times, SHA-256 of every
file) unless noted otherwise.

## Test machine

| | |
|---|---|
| OS | Windows 11 Pro (build 26300) |
| CPU | 16 logical cores |
| RAM | 32 GB |
| Drive C: (source, `Downloads`) | SK Hynix HFS512GDE9X084N, NVMe SSD |
| Drive E: (target) | WD Blue SN570 1 TB, NVMe SSD, NTFS |
| Tools | WinRAR 7.10 (WinRAR.exe, UnRAR.exe 7.01), 7-Zip 26.04, bsdtar 3.8.8 |
| brar | 0.1.0, release build (fat LTO), Rust 1.99, windows-gnu |

---

## 2026-10-20: 28.6 GB RAR5, single runs, 20 s apart

**Archive:** `2347080-GameDrive.Org.rar` from Downloads (C:). 28.6 GB, RAR5, `-m5 -md=32m`, not
solid. Unpacks to 39.0 GiB in 2,611 files and 64 folders.

**Method:** one run per tool, extracting to its own folder on E: (`E:\bench-<tool>`), in the
order WinRAR → UnRAR → brar with a 20 s pause between runs. Times cover the extraction command
only. Script: `bench-one.ps1` (session scratchpad; reproduced below).

| # | Tool | Command | Start → end | Time | Speed |
|---|---|---|---|---|---|
| 1 | WinRAR.exe | `WinRAR x -y -ibck <rar> E:\bench-winrar\` | 21:19:27 → 21:22:23 | 175.3 s | 228 MiB/s |
| 2 | UnRAR.exe | `UnRAR x -y -idq <rar> E:\bench-unrar\` | 21:22:54 → 21:25:51 | 176.4 s | 226 MiB/s |
| 3 | **brar** | `brar x <rar> -o E:\bench-brar -y` | 21:26:24 → 21:27:39 | **74.5 s** | **536 MiB/s** |

**brar: 2.35× faster than WinRAR, 2.37× faster than UnRAR.** All exited 0. Output verified
identical: 0 differences in names, sizes and mtimes; 0 SHA-256 mismatches across all 2,611
files.

Notes:
- WinRAR and UnRAR use the same decoder and decode one file at a time, hence near-identical
  times. brar runs the same unRAR decoder on several files in parallel (the archive is not solid).
- brar ran last (as requested), so it may have gained a little from the archive being partly in
  the OS file cache (32 GB RAM vs 28.6 GB archive). Likely small, since every run writes 39 GB,
  which evicts most of the archive. To rule it out, re-run with brar first.

---

## 2026-10-20: 1.1 GB RAR5, median of 4 runs

**Archive:** `italian-repack.rar` (1.1 GB, RAR5 `-m5 -md32m`, 79 files, 1.2 GiB unpacked, not
solid; two files of 545 MB and 519 MB make up 85% of the data). It is a re-pack of
`Italian-GameDrive.Org.rar` (see the note on damage below) with the same settings.

**Method:** 4 rounds, alternating tool order each round, fresh output folder per run, median
of the middle two times. Output on E:, archive on E:.

| Rank | Tool | Median | vs brar |
|---|---|---|---|
| 1 | **brar** | **2.37 s** | — |
| 2 | UnRAR.exe | 3.61 s | 1.5× slower |
| 3 | WinRAR.exe | 4.05 s | 1.7× slower |
| 4 | 7-Zip 26.04 | 7.67 s | 3.2× slower |
| 5 | bsdtar (Windows `tar.exe`) | 14.87 s | 6.3× slower |

Earlier runs on the original `Italian-GameDrive.Org.rar` (same contents, before it got damaged)
gave the same picture: brar 2.46 s, UnRAR 4.12 s, WinRAR 5.06 s, 7-Zip 8.10 s (medians), with
identical output from brar, WinRAR and 7-Zip.

Gains here are capped by the two big files: one file can't be split across cores, so the
floor is the time to decode the 545 MB file.

### History of brar on this archive

| Version | Time | Change |
|---|---|---|
| First working build | 3.45 s | unRAR decoding sequentially; small files written on a background pool |
| + parallel decoding | 2.21–2.46 s | several unRAR handles decode different files at once (non-solid archives) |

---

## Not benchmarked yet

ZIP, 7z and tar.* have not been benchmarked against 7-Zip yet. 7-Zip is likely strongest on its
own 7z format. `benches/compare.ps1` builds archives from a corpus folder and compares brar,
WinRAR and bsdtar.

## Incident: archive damaged on disk

`Italian-GameDrive.Org.rar` tested clean several times on 2026-10-20, then later the same day
failed its checksum in brar, WinRAR and 7-Zip alike (`Italian\ita_cod26_shared-00000.xsub`),
with its size and modification time unchanged. Possible hardware cause (SSD or RAM); re-download
it and run `chkdsk` before reusing it as a benchmark file.

## Script used for the 28.6 GB run

```powershell
param([ValidateSet('winrar', 'unrar', 'brar')] [string] $Tool, [int] $WaitSeconds = 0)
$archive = "$env:USERPROFILE\Downloads\2347080-GameDrive.Org.rar"
$dest = "E:\bench-$Tool"
if ($WaitSeconds -gt 0) { Start-Sleep -Seconds $WaitSeconds }
if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
New-Item -ItemType Directory $dest | Out-Null
$start = Get-Date
switch ($Tool) {
    'winrar' { Start-Process 'C:\Program Files\WinRAR\WinRAR.exe' -ArgumentList @('x', '-y', '-ibck', "`"$archive`"", "`"$dest\`"") -Wait }
    'unrar'  { & 'C:\Program Files\WinRAR\UnRAR.exe' x -y -idq $archive "$dest\" | Out-Null }
    'brar'   { & 'E:\WORK\BRar\target\release\brar.exe' x $archive -o $dest -y }
}
'{0}: {1:N1} s' -f $Tool, ((Get-Date) - $start).TotalSeconds
```
