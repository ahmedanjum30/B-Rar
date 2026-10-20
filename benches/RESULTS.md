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

## 2026-10-21: RAR, all cores vs one core: WinRAR, UnRAR, 7-Zip, B-Rar

**Archive:** `italian-repack.rar` (1.1 GB, RAR5 `-m5 -md32m`, not solid, 79 files, 1.22 GiB).
**Method:** single runs, **5 s pauses** (the user's new default), B-Rar last in each group; all
cores (00:52–00:53), then one core with process affinity (00:53–00:55).

| Tool | All cores | One core |
|---|---|---|
| WinRAR.exe | 4.08 s | 13.13 s |
| UnRAR.exe | 3.57 s | 12.08 s |
| 7-Zip 26.04 | 7.60 s | **8.37 s** |
| **B-Rar** | **2.04 s** | 12.28 s |

All eight outputs identical (0 SHA-256 mismatches).

- **All cores: B-Rar fastest**, 1.75× faster than UnRAR, 2.0× faster than WinRAR, 3.7× faster
  than 7-Zip.
- **One core: 7-Zip fastest**, 1.47× faster than B-Rar. B-Rar ties UnRAR, since both run the
  official unRAR decoder.
- Correction to earlier notes: 7-Zip's own RAR5 decoder is *faster per core* than unRAR's (8.37 s
  vs 12.08 s). 7-Zip is slow on all cores only because it decodes RAR on one thread (7.60 s vs
  8.37 s), whereas unRAR multithreads RAR5 internally (WinRAR 13.13 s → 4.08 s) and B-Rar adds
  file-level parallelism on top.
- To win on one core too, B-Rar needs a faster RAR decoder than unRAR's (e.g. 7-Zip's Rar5Decoder,
  licensed LGPL with the unRAR restriction).

---

## 2026-10-21: ZIP, all cores vs one core: WinRAR, 7-Zip, B-Rar

**Archive:** `Italian.zip` (1.1 GB Deflate, 79 files, 1.22 GiB). **Method:** single runs, 10 s
pauses, B-Rar last in each group; all cores first (00:48–00:49), then one core with process
affinity (00:49–00:50). B-Rar includes the libdeflate fast path (406d4c7).

| Tool | All cores | One core |
|---|---|---|
| WinRAR.exe | 7.18 s | 8.11 s |
| 7-Zip 26.04 | 7.32 s | 8.01 s |
| **B-Rar** | **2.37 s** | **4.12 s** |
| B-Rar advantage | **3.0× / 3.1×** | **1.97× / 1.94×** |

(Windows tar for reference: one core 4.37 s median on 2026-10-21; all cores 3.82 s on 2026-10-20.)

All six outputs identical (0 SHA-256 mismatches). WinRAR and 7-Zip gain only ~12% from 16
threads because they extract ZIP entries one at a time; B-Rar decodes entries in parallel, so the
gap widens from ~2× on one core to ~3× on all cores.

---

## 2026-10-21: single core, ZIP, with libdeflate: B-Rar beats tar

Same archive (`Italian.zip`, 1.1 GB Deflate, 79 files), both pinned to one core, 10 s pauses,
3 alternating pairs, median.

| Tool | Median | Runs |
|---|---|---|
| **B-Rar** | **3.81 s** | 3.81, 3.84, 3.72 |
| Windows tar | 4.37 s | 4.56, 4.37, 4.30 |

**B-Rar is now 1.15× faster than tar on one core** (was 10% slower), winning all three pairs.
Output identical (0 SHA-256 mismatches); names, sizes and mtimes match WinRAR's output.

What changed: a fast path for plain Deflate and stored entries. A standalone single-core test on
the 545 MB entry found three costs in the old path (2.36 s total):

| Variant (545 MB entry, one core) | Time | MB/s |
|---|---|---|
| zip crate reader over a memory map (old B-Rar) | 2.36 s | 231 |
| zip crate reader over a heap copy | 1.86 s | 293 |
| zlib-rs directly | 1.70 s | 320 |
| **libdeflate, one call, heap input** (new B-Rar) | **1.33 s** | **409** |

So: page faults on the memory-mapped archive 0.5 s, the zip crate's reader layers 0.16 s, and
zlib-rs vs libdeflate 0.37 s. The fast path reads each entry with positioned reads, decodes it in
one libdeflate 1.26 call, checks the CRC, and writes in 1 MiB blocks; a 2 GiB budget caps memory.
Encrypted entries and other methods (Deflate64, bzip2, LZMA, zstd) still use the zip crate.

All cores, same archive, single run: **1.94 s** (was 2.73 s; tar 3.82 s, WinRAR 7.09 s, 7-Zip
7.26 s in the earlier session).

---

## 2026-10-20: single core, ZIP, after the write fix: tar vs B-Rar

Same archive (`Italian.zip`), both pinned to one core, 10 s pauses, 3 alternating pairs, median.

| Tool | Median | Runs |
|---|---|---|
| Windows tar | **5.12 s** | 4.69, 5.12, 5.28 |
| B-Rar | 5.44 s | 4.83, 5.44, 5.71 |

**B-Rar is now 6% behind tar on one core (was 39%).** Output identical. (Both ran slower than
in the earlier session; the machine was slower overall, so compare the ratio, not the times.)

Re-check at 23:11–23:13 (same code as pushed in 280fe97, same method):

| Tool | Median | Runs |
|---|---|---|
| Windows tar | **5.12 s** | 5.26, 5.12, 4.45 |
| B-Rar | 5.47 s | 5.47, 4.85, 5.68 |

B-Rar 7% behind, consistent with the first series. B-Rar won one pair (4.85 vs 5.12 s); runs vary by
about ±10% on this machine, so pairs alone aren't conclusive.

Re-check at 23:26–23:28 on a quieter machine (the user closed other tasks; same code and method):

| Tool | Median | Runs |
|---|---|---|
| Windows tar | **4.17 s** | 4.28, 4.14, 4.17 |
| B-Rar | 4.58 s | 4.58, 4.72, 4.55 |

**B-Rar 10% behind tar**, and tar won all three pairs. With less background noise the runs are
tight (±2%), so this is the most reliable single-core figure so far: the decode-speed gap
(Chromium zlib vs zlib-rs) is real. Output identical.

What changed: profiling showed B-Rar made **144,604 write calls** for 1.22 GB (8.9 KB each,
exactly what the Deflate reader hands back per read), costing 1.45 s. B-Rar now fills its 1 MiB
buffer before each write: **1,313 calls, 0.48 s.**

Single-core breakdown (decode-only = `tar -xOf > NUL` / `B-Rar t`): tar 3.51 s decode + ~0.6 s
writing; B-Rar 3.9–4.1 s decode + ~0.5 s writing. The remaining gap is decoding: tar's
Chromium zlib inflates faster per core than zlib-rs. **Next step:** libdeflate (441 MB/s vs
zlib-rs 316–354 MB/s in the standalone test).

---

## 2026-10-20: single core, 1.1 GB ZIP (Deflate)

**Archive:** `Italian.zip` (see the ZIP section below). **Method:** each tool pinned to one core
with process affinity (mask verified as 1), plus its own single-thread switch where it has one
(`-mt1` WinRAR, `-mmt=1` 7-Zip, `-t 1` B-Rar; tar is single-threaded). Order WinRAR → 7-Zip → tar
→ B-Rar; 20 s pauses, except 10 s before B-Rar (the user's new default for future runs). B-Rar
includes the ZIP fixes from commit 6817553.

| # | Tool | Start → end | Time | Speed |
|---|---|---|---|---|
| 1 | WinRAR.exe `-mt1` | 22:41:56 → 22:42:04 | 8.14 s | 154 MiB/s |
| 2 | 7-Zip `-mmt=1` | 22:42:34 → 22:42:43 | 8.17 s | 153 MiB/s |
| 3 | **Windows tar** | 22:43:11 → 22:43:15 | **4.08 s** | **307 MiB/s** |
| 4 | B-Rar `-t 1` | 22:43:35 → 22:43:41 | 5.69 s | 220 MiB/s |

On one core: **B-Rar is 1.43× faster than WinRAR and 7-Zip, but 1.39× slower than tar.** All exited
0; output identical (0 SHA-256 mismatches).

Reading: with one core there's no parallelism, so this is pure per-core Deflate speed plus file
writing. tar's Chromium zlib decodes faster per core than zlib-rs (~316 MB/s on its own in the
standalone test). libdeflate (441 MB/s in that test) would likely close most of this gap, and it
would also make the multi-core result faster still.

---

## 2026-10-20: 1.1 GB ZIP (Deflate), single runs, 20 s apart

**Archive:** `Italian.zip` from Downloads (C:), made by the user. 1.1 GB, Deflate (79 entries,
plus 3 stored folders), unpacks to 1.22 GiB in 79 files. Poorly compressible game data (~90%
ratio); two files of 545 MB and 519 MB are 85% of the data.

**Method:** one run per tool, WinRAR → 7-Zip → Windows tar → B-Rar, 20 s apart, extracting to
`E:\bench-zip-<tool>`. (UnRAR can't open ZIP, so Windows' built-in `tar.exe` took its place.)

| # | Tool | Start → end | Time | Speed |
|---|---|---|---|---|
| 1 | WinRAR.exe | 22:22:45 → 22:22:52 | 7.09 s | 177 MiB/s |
| 2 | 7-Zip 26.04 (`-mmt=on`) | 22:23:19 → 22:23:27 | 7.26 s | 173 MiB/s |
| 3 | **Windows tar** (bsdtar 3.8.8, Chromium zlib) | 22:23:53 → 22:23:57 | **3.82 s** | **328 MiB/s** |
| 4 | B-Rar | 22:24:24 → 22:24:28 | 4.71 s | 266 MiB/s |

**B-Rar: 1.5× faster than WinRAR and 7-Zip, but 1.23× slower than Windows tar.** All exited 0;
output identical across all four (0 SHA-256 mismatches).

### After two fixes (same day): B-Rar 2.73 s

| Tool | Time | vs B-Rar |
|---|---|---|
| **B-Rar (fixed)** | **2.73 s** | — |
| Windows tar | 3.82 s | 1.40× slower |
| WinRAR.exe | 7.09 s | 2.60× slower |
| 7-Zip 26.04 | 7.26 s | 2.66× slower |

(Single B-Rar run at 22:38:58, same archive and target drive; output identical to WinRAR's.)

Investigation: a standalone test showed Deflate itself was fine. B-Rar's path decodes the 545 MB
entry at 316 MB/s (zlib-rs alone 340–354 MB/s, libdeflate 441 MB/s, CRC32 18.5 GB/s). Two bugs
cost the time instead:
1. **Scheduling:** the rayon parallel iterator handed the two largest entries (sorted next to each
   other) to one thread, so they decoded back to back. Now each entry is its own FIFO job, started
   biggest first.
2. **Opening the archive:** the zip crate's default `ArchiveOffset::Detect` scans the file for
   the central directory, 0.59 s on this 1.1 GB archive. B-Rar now tries the standard layout
   (`Known(0)`) first and only falls back to the scan.

Test mode went 3.44 s → 1.93–2.10 s; extraction 4.71 s → 2.73 s.

**Possible next step:** libdeflate for the per-entry decode (~1.3× faster than zlib-rs above).

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

tar.* has not been benchmarked yet. `benches/compare.ps1` builds archives from a corpus folder and compares B-Rar,
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
