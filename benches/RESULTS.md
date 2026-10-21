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

## 2026-10-21 (22:11): RAR5 engine (big files, solid archives) and a 28.6 GB real archive

After the decoder rewrite and the two-phase engine (`rar5/parallel.rs`). All cores, one run
each (28.6 GB) or best of 2 (the rest), 5 s apart, competitors first. "prev" is the build
before the engine (big files and solid archives went to unRAR).

| Archive | B-Rar new | B-Rar prev | UnRAR | WinRAR | 7-Zip |
|---|---|---|---|---|---|
| TheOrder1886, 28.6 GB RAR5, 16,736 files, 33.3 GB out | **45.4 s** | 50.4 s | 104.5 s | 100.7 s | |
| one 545 MB file | **0.87 s** | 1.46 s | 1.53 s | 2.01 s | 3.14 s |
| 1.1 GB, 2 big + 77 files | **1.64 s** | 1.69 s | 3.36 s | 4.03 s | 7.55 s |
| SOLID 1 GB game data | **1.80 s** | 3.13 s | 3.36 s | 4.04 s | 7.23 s |
| SOLID 48k source files | 7.35 s | **7.26 s** | 27.55 s | 29.24 s | 24.68 s |

The 28.6 GB output was compared byte for byte with UnRAR's: identical. On it, every file went
through B-Rar's decoder (none is big enough for the engine: 33 GB over 16 threads), so the 10%
gain over "prev" is the decoder rewrite.

Total CPU cycles per output byte (all threads; inflated by SMT on this 8-core / 16-thread
Ryzen 7 5800H for every multithreaded tool):

| Archive | B-Rar new | B-Rar prev | UnRAR | 7-Zip (1 thread) |
|---|---|---|---|---|
| one 545 MB file | 18.2 | 26.2 | 30.7 | 17.9 |
| 1.1 GB, 2 big + 77 | 19.6 | 28.9 | 31.0 | 18.4 |
| SOLID 1 GB game | 18.1 | 26.1 | 29.5 | 17.7 |
| SOLID 48k files | 98.6 | 76.4 | 86.1 | 74.2 |

Engine phases (BRAR_STATS) before -> after chunking across files and reusing token buffers:
solid 48k files 48,023 -> 67 chunks, phase A 21.0 -> 2.6 and phase B 9.7 -> 3.9 cycles/byte;
one 545 MB file phase A 16.6 -> 10.9, phase B 6.0 -> 3.9.

---

## 2026-10-21 (19:59): CPU work per byte, every tool and format

Total CPU cycles of the whole extraction (all threads, `QueryProcessCycleTime`) divided by
the bytes written: how much work each tool does, independent of how many cores it uses. All
cores, one run each, 3 s apart. "B-Rar (own)" is a test build that sends every RAR5 file to
B-Rar's decoder (not committed); "B-Rar" on RAR is the released per-file choice (big files to
unRAR). `italian.zip` was rebuilt with 7-Zip `-mx5` from the same files (the original ZIP was
deleted). Machine at about 3.2 GHz.

| Archive | B-Rar | B-Rar (own) | WinRAR | UnRAR | 7-Zip | Windows tar |
|---|---|---|---|---|---|---|
| ZIP 1.2 GB | 8.51 (1.77 s) | | 15.20 (6.39 s) | | 14.31 (5.94 s) | **8.14** (3.37 s) |
| ZIP 47k files | **59.32** (6.30 s) | | 81.64 (27.72 s) | | 80.86 (26.65 s) | 50.40 ✗ (16.55 s) |
| RAR5 1.1 GB | 26.73 (1.72 s) | **16.47** (2.97 s) | 29.80 (3.71 s) | 29.46 (3.45 s) | 18.32 (7.57 s) | |
| RAR5 one 545 MB file | 24.65 (1.53 s) | **15.57** (2.80 s) | 29.49 (1.78 s) | 28.48 (1.58 s) | 17.64 (3.18 s) | |
| RAR5 48k files | **65.43** (6.77 s) | 67.95 (7.11 s) | 85.28 (27.76 s) | 83.33 (26.94 s) | 72.43 (23.97 s) | |
| 7z 1.0 GB | 85.98 (5.92 s) | | 62.28 (5.52 s) | | **58.69** (4.10 s) | 106.77 (44.09 s) |
| tar.gz 47k files | 58.13 (5.40 s) | | 72.17 (23.75 s) | | | **50.87** (16.71 s) |
| tar.xz 47k files | 74.61 (5.52 s) | | 85.50 (28.11 s) | | | **67.21** (22.01 s) |
| tar.bz2 47k files | 123.87 (16.39 s) | | 124.39 (40.61 s) | | | **100.75** (32.85 s) |

Cycles per output byte (lower is less work), wall time in brackets. ✗ = tar stopped at a
file with a non-ASCII name.

Decode only (B-Rar's RAR5 decoder, `rar5_bench`, best of 3, one thread):

| Archive | MB/s | cycles/output byte | cycles/packed byte |
|---|---|---|---|
| one 545 MB file (81% ratio) | 249 | 12.20 | 15.05 |
| italian-repack.rar (79 files) | 247 | 12.32 | 13.99 |
| source-m5.rar (48k text files) | 606 | 4.95 | 35.57 |
| mixed.rar | 489 | 6.21 | |

For comparison, libdeflate decodes the same 545 MB file (as Deflate) at 441 MB/s, about 7.3
cycles/byte: roughly 1.7x faster per byte than B-Rar's RAR5 decoder.

What it shows:
- B-Rar's RAR5 decoder does the least work of any tool: 45% less than UnRAR, 10-12% less than
  7-Zip. The released build is fastest on the clock only because unRAR spends ~60% more cycles
  to use many cores on big files.
- Many small files cost about 1.3-1.4 million cycles per file, almost all of it Windows
  creating, writing and closing files; decoding is a few cycles per byte of that.
- B-Rar does 4-14% more work than single-threaded Windows tar on ZIP and tar formats: the cost
  of parallel file creation (directory locks, waiting) and buffering.
- 7z: B-Rar does 47% more work than 7-Zip (the lzma-rust2 decoder). bzip2: 23% more than tar
  (B-Rar's bzip2 crate is slower than tar's libbz2).

---

## 2026-10-21 (04:16): full matrix, every format, all cores vs one core

After the RAR per-file fix (commit 9768fa7). One run per cell, 5 s apart, competitors first and
B-Rar last. "One core" = the tool started with `start /affinity 1` (plus `-mt1` / `-mmt=1` /
`-t 1`). Every run's output was counted (files and bytes) to catch partial extractions. Idle PC.

| Archive | Mode | B-Rar | WinRAR | UnRAR | 7-Zip | Windows tar | Winner |
|---|---|---|---|---|---|---|---|
| ZIP 1.2 GB (79 files) | all cores | **3.03 s** | 7.09 s | — | 8.08 s | 4.05 s | B-Rar, 1.3× over tar |
| | one core | **3.69 s** | 6.75 s | — | 7.74 s | 4.05 s | B-Rar, 1.1× over tar |
| RAR5 1.1 GB (79 files) | all cores | **2.01 s** | 4.04 s | 4.05 s | 8.09 s | 15.13 s | B-Rar, 2.0× over WinRAR |
| | one core | **6.41 s** | 11.86 s | 11.33 s | 7.75 s | 15.27 s | B-Rar, 1.2× over 7-Zip |
| 7z 1.04 GB (LZMA2, 79 files) | all cores | 7.04 s | 5.05 s | — | **5.04 s** | 44.39 s | 7-Zip, 1.4× over B-Rar |
| | one core | 32.66 s | 22.09 s | — | **21.68 s** | 45.30 s | 7-Zip, 1.5× over B-Rar |
| ZIP, 47k small files | all cores | **7.04 s** | 26.26 s | — | 27.22 s | 17.13 s ✗ | B-Rar, 3.7× over WinRAR |
| | one core | **13.82 s** | 26.94 s | — | 27.13 s | 17.05 s ✗ | B-Rar, 1.9× over WinRAR |
| RAR5, 48k small files | all cores | **7.08 s** | 28.24 s | 27.20 s | 24.22 s | 26.26 s ✗ | B-Rar, 3.4× over 7-Zip |
| | one core | **15.63 s** | 27.74 s | 26.30 s | 23.75 s | 27.13 s ✗ | B-Rar, 1.5× over 7-Zip |
| tar.gz, 47k small files | all cores | **6.06 s** | 24.22 s | — | n/a † | 17.12 s | B-Rar, 2.8× over tar |
| | one core | **12.73 s** | 24.39 s | — | n/a † | 17.38 s | B-Rar, 1.4× over tar |

- ✗ Windows tar failed: it stopped on one file with a non-ASCII name and wrote 47,013 of 47,014
  (ZIP) / 48,350 of 48,351 (RAR) files. Its time is for an incomplete extraction.
- † 7-Zip only removes the gzip layer in one pass (writes `c.tar`, 3.8-4.0 s); unpacking the
  files needs a second run, so it isn't comparable.
- B-Rar's ZIP time was 2.37 s in the run below; 3.03 s here is run-to-run noise on one sample.
- The "many small files are slow" result from earlier today (about 55 s for 48k files) was taken
  while a game was running. On an idle PC B-Rar is the fastest by 3-4×.
- Where B-Rar loses: 7z (LZMA decoding, lzma-rust2), and bzip2 (single-threaded, not in this
  table: 15.6 s for a 1 GB tar.bz2 in the end-to-end run).

---

## 2026-10-21: why 7-Zip is faster on RAR (one core), and a many-files bug

Decode-only (`t`), one core (affinity 1), CPU seconds:

| Archive | 7-Zip | UnRAR | B-Rar (before fix) | B-Rar (fixed) |
|---|---|---|---|---|
| `italian-repack.rar` (1.1 GB game data, 79 files, barely compressible) | **7.28** | 10.73 | 11.28 | ≈ UnRAR |
| `source-m5.rar` (144 MB; 1,038 MB of source code, 57,224 entries, 7.2× compressible) | **2.45** | 6.81 | 39.78 | 7.17 |

Wall time on `source-m5.rar`: one core 7-Zip 2.85 s, UnRAR 8.57 s, B-Rar 43.44 s → **7.86 s**; all
cores 7-Zip 2.53 s, UnRAR 7.52 s, B-Rar 6.57 s → **5.10 s**.

**Bug (fixed):** in DLL mode unRAR seeks back after every file, and with a RAR5 "quick open"
record present, `QuickOpen::Seek` reloads the whole cache on such a seek: **620 µs per header**
(57k headers = 35 s) vs 10.6 µs on the same archive built with `-qo-`. Patched in the vendored
`unrar_sys` (`dll.cpp`: `Cmd.QOpenMode=QOPEN_NONE`): 12.6 µs per header.

**Why 7-Zip is faster per core:** its own RAR5 decoder (Igor Pavlov's `Rar5Decoder`) does the same
work with 1.5× less CPU on barely compressible data and 2.6–2.8× less on compressible data. It isn't
our build: RARLAB's own `UnRAR.exe` (MSVC) is exactly as slow, and unRAR's 8-byte match copying
(`UNPACK_COPY8`) is enabled. The gap is in the decoder design (bit reading, Huffman tables, match
copying, write-out path), larger where matches dominate.

**Still open (all cores, many small files):** B-Rar's parallel RAR path burns 46 CPU-s where UnRAR
needs 7, because every worker reads all 57k headers to find its own files.

---

## 2026-10-21: 7z, all cores vs one core: WinRAR, 7-Zip, tar, B-Rar

**Archive:** `Italian-GameDrive.7z` (1.04 GB, LZMA2:25, solid, 1 block, 79 files, 1.22 GiB).
**Method:** single runs, 5 s pauses, B-Rar last in each group; all cores (01:05–01:07), then one
core with process affinity (01:07–01:10). UnRAR can't open 7z.

| Tool | All cores | One core |
|---|---|---|
| WinRAR.exe | 5.10 s | 24.20 s |
| **7-Zip 26.04** | **4.13 s** | **23.11 s** |
| Windows tar | 43.96 s | 47.40 s |
| B-Rar | 6.31 s | 35.24 s |

All eight outputs identical (0 SHA-256 mismatches).

- **7-Zip fastest in both modes.** B-Rar is 1.53× slower on all cores and 1.52× slower on one core:
  the same ratio both ways, which points squarely at the decoder (pure-Rust `lzma-rust2` vs 7-Zip's
  LZMA), not at parallelism or I/O.
- WinRAR's 7z decoder is close to 7-Zip's (5% slower on one core).
- Windows tar (libarchive) is ~10× slower than everyone else on 7z.
- Fix: decode LZMA/LZMA2 with the 7-Zip LZMA SDK (see the 7z section below).

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
