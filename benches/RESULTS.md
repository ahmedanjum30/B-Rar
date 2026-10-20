# Benchmark results

Extraction benchmarks of B-Rar against other archivers. (Runs before the rename were made with the same code under the old name "brar".) Newest first. Every run checked that
B-Rar's output is identical to WinRAR's (names, sizes, modification times, SHA-256 of every
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
| B-Rar | 0.1.0, release build (fat LTO), Rust 1.99, windows-gnu |

---

## 2026-10-20: single core, 1.1 GB RAR5, WinRAR vs UnRAR vs B-Rar

**Archive:** `italian-repack.rar` (1.1 GB, RAR5 `-m5 -md32m`, 79 files, 1.22 GiB unpacked).

**Method:** every tool pinned to one CPU core with process affinity (`start /affinity 1`, mask
verified as 1 inside the run, inherited by the tool and any threads it starts), plus each tool's
own single-thread switch (`-mt1` for WinRAR/UnRAR, `-t 1` for B-Rar). Order WinRAR → UnRAR →
B-Rar, 20 s apart, extracting to E:.

| # | Tool | Start → end | Time | Speed |
|---|---|---|---|---|
| 1 | WinRAR.exe `-mt1` | 22:13:19 → 22:13:32 | 13.13 s | 95 MiB/s |
| 2 | UnRAR.exe `-mt1` | 22:14:00 → 22:14:12 | 12.12 s | 103 MiB/s |
| 3 | **B-Rar** `-t 1` | 22:14:41 → 22:14:53 | **12.12 s** | **103 MiB/s** |

**On one core B-Rar ties UnRAR and is 8% faster than WinRAR.exe.** All exited 0; output
identical to WinRAR's (0 SHA-256 mismatches, 79 files).

Reading: all three run the same unRAR decoder, so on a single core the decoder sets the pace and
B-Rar adds no overhead of its own. Its 1.7–2.4× wins on many cores come entirely from decoding
several files at once. (WinRAR.exe is a little slower than its console tool, likely GUI overhead.)

---

## 2026-10-20: 1.04 GB 7z (LZMA2, solid), single runs, 20 s apart

**Archive:** `Italian-GameDrive.7z` from Downloads (C:), made by the user with 7-Zip. 1.04 GB,
LZMA2:25 (32 MB dictionary), solid, 1 block. Unpacks to 1.22 GiB in 79 files.

**Method:** one run per tool, 7-Zip first, then B-Rar 20 s later, each extracting to its own
folder on E:.

| # | Tool | Command | Start → end | Time | Speed |
|---|---|---|---|---|---|
| 1 | **7-Zip 26.04** | `7z x <7z> -oE:\bench-7zip -y -mmt=on` | 22:09:21 → 22:09:25 | **4.11 s** | **305 MiB/s** |
| 2 | B-Rar | `B-Rar x <7z> -o E:\bench-B-Rar -y` | 22:09:51 → 22:09:58 | 6.33 s | 198 MiB/s |

**7-Zip is 1.54× faster than B-Rar on its own format.** Both exited 0; output identical (0
SHA-256 mismatches across 79 files).

Why (measured with `t`, test mode): both tools keep about 6 cores busy (7-Zip: 23.1 CPU-s in
3.94 s; B-Rar: 36.4 CPU-s in 6.25 s), so parallelism is equal. The gap is per-core decoder speed:
B-Rar's 7z path uses the pure-Rust `lzma-rust2` LZMA decoder, which needs ~1.6× the CPU time of
7-Zip's hand-optimized decoder. Because the archive is solid with one block, B-Rar's per-file
parallelism (which wins on RAR) can't help here.

**Next step:** decode 7z's LZMA/LZMA2 with a faster engine: the 7-Zip LZMA SDK (public domain C,
the decoder 7-Zip itself uses) or liblzma (already linked for .xz). Then re-run this test.

---

## 2026-10-20: 28.6 GB RAR5, single runs, 20 s apart

**Archive:** `2347080-GameDrive.Org.rar` from Downloads (C:). 28.6 GB, RAR5, `-m5 -md=32m`, not
solid. Unpacks to 39.0 GiB in 2,611 files and 64 folders.

**Method:** one run per tool, extracting to its own folder on E: (`E:\bench-<tool>`), in the
order WinRAR → UnRAR → B-Rar with a 20 s pause between runs. Times cover the extraction command
only. Script: `bench-one.ps1` (session scratchpad; reproduced below).

| # | Tool | Command | Start → end | Time | Speed |
|---|---|---|---|---|---|
| 1 | WinRAR.exe | `WinRAR x -y -ibck <rar> E:\bench-winrar\` | 21:19:27 → 21:22:23 | 175.3 s | 228 MiB/s |
| 2 | UnRAR.exe | `UnRAR x -y -idq <rar> E:\bench-unrar\` | 21:22:54 → 21:25:51 | 176.4 s | 226 MiB/s |
| 3 | **B-Rar** | `B-Rar x <rar> -o E:\bench-B-Rar -y` | 21:26:24 → 21:27:39 | **74.5 s** | **536 MiB/s** |

**B-Rar: 2.35× faster than WinRAR, 2.37× faster than UnRAR.** All exited 0. Output verified
identical: 0 differences in names, sizes and mtimes; 0 SHA-256 mismatches across all 2,611
files.

Notes:
- WinRAR and UnRAR use the same decoder and decode one file at a time, hence near-identical
  times. B-Rar runs the same unRAR decoder on several files in parallel (the archive is not solid).
- B-Rar ran last (as requested), so it may have gained a little from the archive being partly in
  the OS file cache (32 GB RAM vs 28.6 GB archive). Likely small, since every run writes 39 GB,
  which evicts most of the archive. To rule it out, re-run with B-Rar first.

---

## 2026-10-20: 1.1 GB RAR5, median of 4 runs

**Archive:** `italian-repack.rar` (1.1 GB, RAR5 `-m5 -md32m`, 79 files, 1.2 GiB unpacked, not
solid; two files of 545 MB and 519 MB make up 85% of the data). It is a re-pack of
`Italian-GameDrive.Org.rar` (see the note on damage below) with the same settings.

**Method:** 4 rounds, alternating tool order each round, fresh output folder per run, median
of the middle two times. Output on E:, archive on E:.

| Rank | Tool | Median | vs B-Rar |
|---|---|---|---|
| 1 | **B-Rar** | **2.37 s** | — |
| 2 | UnRAR.exe | 3.61 s | 1.5× slower |
| 3 | WinRAR.exe | 4.05 s | 1.7× slower |
| 4 | 7-Zip 26.04 | 7.67 s | 3.2× slower |
| 5 | bsdtar (Windows `tar.exe`) | 14.87 s | 6.3× slower |

Earlier runs on the original `Italian-GameDrive.Org.rar` (same contents, before it got damaged)
gave the same picture: B-Rar 2.46 s, UnRAR 4.12 s, WinRAR 5.06 s, 7-Zip 8.10 s (medians), with
identical output from B-Rar, WinRAR and 7-Zip.

Gains here are capped by the two big files: one file can't be split across cores, so the
floor is the time to decode the 545 MB file.

### History of B-Rar on this archive

| Version | Time | Change |
|---|---|---|
| First working build | 3.45 s | unRAR decoding sequentially; small files written on a background pool |
| + parallel decoding | 2.21–2.46 s | several unRAR handles decode different files at once (non-solid archives) |

---

## Not benchmarked yet

ZIP and tar.* have not been benchmarked against 7-Zip yet. `benches/compare.ps1` builds archives from a corpus folder and compares B-Rar,
WinRAR and bsdtar.

## Incident: archive damaged on disk

`Italian-GameDrive.Org.rar` tested clean several times on 2026-10-20, then later the same day
failed its checksum in B-Rar, WinRAR and 7-Zip alike (`Italian\ita_cod26_shared-00000.xsub`),
with its size and modification time unchanged. Possible hardware cause (SSD or RAM); re-download
it and run `chkdsk` before reusing it as a benchmark file.

## Script used for the 28.6 GB run

(Shown with today's names; the run itself used `brar.exe`, the same code before the rename.)

```powershell
param([ValidateSet('winrar', 'unrar', 'B-Rar')] [string] $Tool, [int] $WaitSeconds = 0)
$archive = "$env:USERPROFILE\Downloads\2347080-GameDrive.Org.rar"
$dest = "E:\bench-$Tool"
if ($WaitSeconds -gt 0) { Start-Sleep -Seconds $WaitSeconds }
if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
New-Item -ItemType Directory $dest | Out-Null
$start = Get-Date
switch ($Tool) {
    'winrar' { Start-Process 'C:\Program Files\WinRAR\WinRAR.exe' -ArgumentList @('x', '-y', '-ibck', "`"$archive`"", "`"$dest\`"") -Wait }
    'unrar'  { & 'C:\Program Files\WinRAR\UnRAR.exe' x -y -idq $archive "$dest\" | Out-Null }
    'B-Rar'   { & 'E:\WORK\BRar\target\release\B-Rar.exe' x $archive -o $dest -y }
}
'{0}: {1:N1} s' -f $Tool, ((Get-Date) - $start).TotalSeconds
```
