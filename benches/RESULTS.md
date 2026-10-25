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

## 2026-10-25 (21:01): libdeflate removed: every ZIP Deflate entry uses B-Rar's own decoder

Later the same day the single-core decoder caught up with libdeflate on small text too, and
replaced it: ZIP entries under 64 MB (and the parallel decoder's fallback) now use B-Rar's
decoder; libdeflate stays only as a benchmark yardstick (a dev-dependency, not in B-Rar.exe).

### Decoder only, one core (CPU cycles per output byte, best of 3)

| Data | B-Rar (earlier today) | B-Rar now | libdeflate | |
|---|---|---|---|---|
| `single.zip` (545 MB entry) | 6.02 | **5.97** | 6.41 | 7% faster |
| `italian.zip` (79 entries) | 4.35 | **4.31** | 4.62 | 7% faster |
| `corpus.zip` (45,089 small entries) | 2.59 | 2.41 | **2.38** | 1.3% slower |
| table build + block header, per block | ~1,500 | **1,390** | 1,535 | 10% cheaper |

What got it there: each block picks one of two loops from its code lengths (literal-heavy
blocks: four literals per refill; match-heavy: each entry consumed before it's tested, a
literal run going straight into its match); both loops, the header decoding and the table
builds compiled for BMI2 and chosen at run time; all tables in one allocation (one base
register in the loop). Found with a sampling profiler built into the tests (suspend the
decoding thread, read its instruction pointer; no admin rights) and libdeflate's own
disassembly.

Tried and dropped: decoding in two passes (symbols to a token list, then the output: the
first pass alone was slower than the whole one-pass decoder, 22 vs ~21 cycles per symbol);
two streams interleaved on one thread (corpus 2.40 -> 2.50, 82 entries 4.31 -> 4.29: the
decoder is limited by instructions and mispredicted branches, not by its dependency chain);
a speculative distance lookup, 8-byte or partial match copies, 9/10-bit distance tables,
running the fast loop to the very end of the stream, SIMD length counting.

### End to end (B-Rar.exe), best of 3, Defender off

| Archive | libdeflate for small entries | own decoder only |
|---|---|---|
| one 545 MB entry, extract / test | 0.585 s / 0.414 s | 0.581 s / 0.422 s |
| 82 entries, 1.25 GB | 0.841 s / 0.623 s | 0.841 s / 0.626 s |
| 55,695 small files | 6.191 s / 0.627 s | 5.989 s / 0.635 s |

The same within noise: the extraction time is in file creation and the ordered output, not
the decoder. B-Rar.exe: 4,338,688 -> 4,366,336 bytes (the decoder is built in several
variants: two loops, with and without BMI2, for bytes and for the parallel decoder's
16-bit output).

---

## 2026-10-25: ZIP gets B-Rar's own Deflate decoder (parallel, and single-core vs libdeflate)

ZIP entries of 64 MB and up are now decoded on all cores by B-Rar's own Deflate decoder
(one stream cut into 2 MB pieces, each decoded without the 32 KB before it, then resolved
and written in order). Smaller entries still use libdeflate. Defender real-time protection
off for all runs below. CPU: AMD Ryzen 7 5800H (8 cores, 16 threads).

Archives: `single.zip` (one 545 MB entry, 460 MB packed), `italian.zip` (82 entries,
1.25 GB), `corpus.zip` (55,695 small files, 976 MB; 45,089 Deflate entries).

### Decoder only, B-Rar vs libdeflate itself

Both decode the same compressed bytes from memory; only decoding is timed. libdeflate via
the `libdeflater` crate (libdeflate 1.26). Cycles per output byte, one core, best of 3
(`inflate_bench`):

| Data | B-Rar | libdeflate | |
|---|---|---|---|
| `single.zip` (545 MB entry) | **6.02** | 6.42 | 6% faster |
| `italian.zip` (79 entries) | **4.35** | 4.64 | 6% faster |
| `corpus.zip` (45,089 small entries) | 2.59 | **2.39** | 8% slower |

B-Rar on all 16 threads against libdeflate on one core, one stream at a time
(`parallel_bench`, two runs):

| Data | libdeflate, 1 core | B-Rar, 16 threads | |
|---|---|---|---|
| `single.zip` | 1.15 s | **0.320–0.328 s** | 3.5–3.6x |
| `italian.zip` (545 MB of entries) | 1.08 s | **0.245–0.254 s** | 4.2–4.4x |

What made the single-core decoder faster than libdeflate on these: one table lookup gives a
symbol's whole value (length or distance with its extra bits); the bit reader lives in
registers and shifts by the table entry as it is; tables built by doubling, with four
independent counting/sorting chains; code lengths read without branches; fixed tables kept
between blocks. Tried and dropped: BMI2 chosen at run time (no gain on Zen 3), two literals
per lookup (almost no table entries qualify on real data, and it slowed the loop), copying
libdeflate's loop layout exactly (better on text, 9% worse on large files). Where libdeflate
still wins: match-heavy text, about 3 cycles per match; per-block table cost is equal
(~1,500 cycles each, measured with the same data in 1 KB and 64 KB blocks).

### End to end (B-Rar.exe), best of 3

| Archive | Build | Extract | Test |
|---|---|---|---|
| one 545 MB entry | before (libdeflate only) | 1.610 s | 1.422 s |
| | parallel decoder | 0.586 s | 0.439 s |
| | + fast loop in the pieces | **0.582 s** | **0.424 s** |
| 82 entries, 1.25 GB | before | 1.585 s | 1.360 s |
| | parallel decoder | 0.859 s | 0.663 s |
| | + fast loop in the pieces | **0.857 s** | **0.637 s** |
| 55,695 small files | before | 6.550 s | 0.653 s |
| | now | 6.015 s | 0.644 s |

The fast loop cut CPU time per byte 6–9% but not wall time: the limit is now outside the
decoder (ordered output, checksum, file writing). The small-files archive is unchanged
(libdeflate, and file creation dominates).

---

## 2026-10-23 (15:36): two files at once in the single-file decoder: tried, dropped

The same trick as phase A's (two streams per thread, up to three literals each per round),
applied to the decoder that handles one whole file per job. On one thread it looked good
(`pair_bench`, the 1.1 GB archive's 77 files under 64 MB: 9.63 -> 7.65 cycles/B), but in
real extractions it didn't help:

| Archive (non-solid) | test, before -> paired | extract, before -> paired |
|---|---|---|
| 110 x 5 MB pieces of the 545 MB file | 0.257 -> 0.276 s | 0.310 -> 0.328 s |
| 48k source files | 0.787 -> 0.788 s | 7.061 -> 7.161 s |
| the 8 usual archives | unchanged (the paired files are a small share of their bytes) | |

With every logical CPU busy, SMT already interleaves two decoding threads per core in
hardware, so interleaving in software adds nothing, and pairing halves the number of jobs
(coarser balancing). The engine's interleaved phase A is different: it runs 8 workers on 16
logical CPUs, so each worker has a core to itself, and there it saves cycles.
## 2026-10-23 (06:47): phase A decodes two chunks at once, less CPU per byte

d820dfc against this commit, back to back, best of 3, idle PC, Defender off. cyc/B = the
process's CPU cycles over all threads per output byte.

| Archive | extract | extract cyc/B | test cyc/B | phase A cyc/B |
|---|---|---|---|---|
| one 545 MB file | 0.416 -> 0.418 s | 15.42 -> **13.25** (-14%) | 13.58 -> **11.61** | 10.85 -> 9.25 |
| 1.1 GB, 2 big + 77 files | 0.787 -> 0.803 s | 20.23 -> 19.90 | 16.76 -> 15.73 | mixed |
| solid 1 GB game | 0.923 -> 0.916 s | 15.25 -> **13.43** (-12%) | 13.43 -> **11.45** | 10.90 -> 8.95 |
| solid 48k files | 6.572 -> 6.573 s | 59.83 -> 60.46 | 5.60 -> 5.31 | 2.90 -> 2.59 |
| solid game, 256 MB dictionary | 0.957 -> 0.949 s | 15.30 -> **13.57** (-11%) | 13.27 -> **11.35** | 10.48 -> 8.73 |
| encrypted 545 MB file | 0.433 -> 0.421 s | 16.92 -> **14.82** (-12%) | 14.88 -> **13.27** | 12.30 -> 10.56 |
| encrypted headers, 4 volumes | 0.936 -> 0.929 s | 16.82 -> **14.97** (-11%) | 14.84 -> **12.93** | 12.22 -> 10.62 |
| encrypted, 12 volumes | 0.839 -> 0.855 s | 23.84 -> 23.67 | 20.73 -> 20.15 | 16.84/14.77 -> 16.04/14.10 |

Wall times are unchanged (phase B, the serial step, sets the pace); the CPU work is 11-15%
lower on every archive the engine decodes on its own. The two mixed archives (two big files
plus many small ones decoded by the single-file decoder, which didn't change) gain little.

Phase A alone, on one thread (`phase_a_bench`), cycles per output byte:

| Loop | 545 MB file | solid game | 48k source files |
|---|---|---|---|
| one chunk at a time (before) | 9.7 | 9.7 | 1.92 |
| two literals decoded branch-free, one branch per pair | 9.7 | 9.7 | 2.5 (wasted decodes after a match) |
| two chunks, one symbol each per round | 9.8 | 9.8 | 1.76 |
| two chunks, up to two literals each per round | 8.2 | 8.2 | 1.79 |
| **two chunks, up to three literals each per round** | **7.6** | **7.6** | 1.92 |

The lookup chain wasn't the limit (two independent streams alone gained nothing on binary
data): the bookkeeping per symbol was. Two streams with three literals each per round spread
it over more symbols and keep both chains busy.
## 2026-10-23 (07:00): no unmapping on the critical path

Test mode (`t`), best of 3, back to back with 7ab90db; b94eeb2 numbers from the run above.
Idle PC, Defender off.

| Archive | 7ab90db | b94eeb2 | this |
|---|---|---|---|
| one 545 MB file | 0.426 s | 0.354 s | **0.322 s** |
| 1.1 GB, 2 big + 77 files | 0.733 s | 0.699 s | **0.639 s** |
| solid 1 GB game | 0.857 s | 0.749 s | **0.704 s** |
| solid 48k files | 0.788 s | 0.655 s | **0.651 s** |
| solid game, 256 MB dictionary | 0.955 s | 0.835 s | **0.784 s** |
| encrypted 545 MB file | 0.423 s | 0.366 s | **0.336 s** |
| encrypted headers, 4 volumes | 0.891 s | 0.802 s | **0.749 s** |
| encrypted, 12 volumes | 0.795 s | 0.725 s | **0.681 s** |

Timestamps through the whole run (open, parse, sink, engine, return) showed ~20 ms after the
engine finished: unmapping the archive, whose pages the header scan had touched (one per
block, ~27,000). Reading headers with plain positioned reads instead removed it but made the
scan slow under load (a syscall per block, 3-10 us: it finished just before phase B on the
solid game). Now the scan maps the archive 64 MB at a time and unmaps each section as it moves
on, on its own thread; encrypted headers are decrypted from the mapped ciphertext too.

Tried and dropped (phase B, measured):
- *Writing through a 1 MB staging buffer* (filters split the output; filtered pieces
  average ~1 KB): the drain thread's cost didn't change. Separately, writes to one file don't
  scale across threads on Windows (0.18 s for 545 MB with 1, 2, 4 or 8 writers).
- *Literal runs up to 1 KB copied inline* (not through memcpy): no change.
- *Prefetching only copies more than 16 KB back*: worse (phase B 1.64 -> 1.69 cyc/B; 48k
  files 1.58 -> 1.83): near copies benefit too.
- Parsing the 48k-file archive's headers takes 39 ms (+7 ms unmapping), under 1 us per
  header; not worth a rework.
## 2026-10-23 (05:34): phase B leaner, B-Rar against its own previous build

From now on B-Rar is measured against itself. Baseline = 7ab90db (the installed build), new
= this commit, back to back per archive, idle PC, Defender off, best of 3. "cyc/B" = the
process's CPU cycles (all threads) per output byte. Phase B = the engine's serial step.

| Archive | extract, baseline -> new | test, baseline -> new | phase B cyc/B |
|---|---|---|---|
| one 545 MB file | 0.473 -> **0.426 s** (-10%) | 0.428 -> **0.354 s** (-17%) | 2.06 -> 1.64 |
| 1.1 GB, 2 big + 77 files | 0.824 -> **0.811 s** (-2%) | 0.735 -> **0.699 s** (-5%) | 2.75 -> 2.45 |
| solid 1 GB game | 0.995 -> **0.934 s** (-6%) | 0.862 -> **0.749 s** (-13%) | 1.80 -> 1.45 |
| solid 48k files | 6.415 -> 6.535 s (file creation) | 0.799 -> **0.655 s** (-18%) | 2.07 -> 1.59 |
| solid game, 256 MB dictionary | 1.108 -> **0.983 s** (-11%) | 0.953 -> **0.835 s** (-12%) | 2.05 -> 1.65 |
| encrypted 545 MB file | 0.494 -> **0.443 s** (-10%) | 0.435 -> **0.366 s** (-16%) | 2.08 -> 1.61 |
| encrypted headers, 4 volumes | 1.022 -> **0.949 s** (-7%) | 0.903 -> **0.802 s** (-11%) | 1.83 -> 1.46 |
| encrypted, 12 volumes | 0.890 -> **0.867 s** (-3%) | 0.788 -> **0.725 s** (-8%) | 2.99 -> 2.38 |

Total CPU work per byte is unchanged (~15 cycles/B on the big archives): phase A now does a
little more (10.2 -> 10.7) so the serial phase B can do less, which is what shortens the
wall time. What changed, each measured on its own (test mode):
1. *Phase B fine-tuning*: copies with a distance under 8 double the pattern up to 8 bytes,
   then copy 8 at a time (was byte by byte); the window-slide path is out of line. Big file
   0.419 -> 0.378 s.
2. *Phase A resolves repeated distances*: each chunk tracks the four recent distances itself,
   so phase B gets plain copies (explicit distance, or "recent distance n as the chunk
   began") and keeps no state per op. 0.378 -> 0.358 s; solid 48k 0.732 -> 0.652 s.
3. *Window pre-faulted* on 4 threads while phase A starts (it was ~24,000 page faults on
   phase B's thread). 0.358 -> 0.342 s.
4. *Phase A*: a profile of phase A alone (`BRAR_PROFILE_ENGINE=A`) puts 64% in the Huffman
   table lookup, a chain of dependent loads, near the floor for one stream; larger or
   two-symbol tables were already measured slower. What did help: engines decoding two big
   files at once had only 4 workers each and waited on phase A; workers are now
   `threads - 2` (at most 8). 1.1 GB set 0.734 -> 0.681 s, 12-volume set 0.795 -> 0.712 s
   (quick A/B). Left: one worker decoding two chunks interleaved (two lookup chains at once),
   a big rewrite of the hot loop, for perhaps 10-20% of phase A.
## 2026-10-22 (02:08): full RAR benchmark, idle PC, Defender off and on

Every RAR5 archive in bench-data, all cores, best of 2, 5 s apart, competitors first.
"B-Rar (installed)" is the MSIX package installed from this build (7ab90db); "unpackaged" is
the same binary run from `target\release`. Password `secret` where encrypted.

**Defender real-time protection off:**

| Archive | WinRAR | UnRAR | 7-Zip | B-Rar unpackaged | B-Rar installed |
|---|---|---|---|---|---|
| one 545 MB file | 2.02 s | 1.56 s | 3.13 s | **0.45 s** | 0.50 s |
| 1.1 GB, 2 big + 77 files | 4.04 s | 3.37 s | 7.58 s | **0.80 s** | 0.88 s |
| solid 1 GB game | 4.04 s | 3.35 s | 7.21 s | **1.02 s** | **1.02 s** |
| solid 48k source files | 29.21 s | 27.78 s | 24.04 s | 6.60 s | **6.57 s** |
| solid 1 GB game, 256 MB dictionary | 5.04 s | 3.80 s | 7.45 s | **1.14 s** | 1.16 s |
| encrypted 545 MB file | 3.03 s | 1.87 s | 3.22 s | **0.48 s** | 0.58 s |
| encrypted headers, solid 1 GB, 4 volumes | 5.03 s | 4.13 s | 7.67 s | **1.08 s** | 1.12 s |
| encrypted 1.1 GB, 12 volumes | 5.02 s | 4.27 s | 8.04 s | **0.91 s** | 1.05 s |

**Defender real-time protection on** (the first four archives; Defender was switched off
during the fifth):

| Archive | WinRAR | UnRAR | 7-Zip | B-Rar unpackaged | B-Rar installed |
|---|---|---|---|---|---|
| one 545 MB file | 2.02 s | 1.59 s | 3.14 s | **0.46 s** | 0.54 s |
| 1.1 GB, 2 big + 77 files | 4.03 s | 3.52 s | 7.61 s | 0.94 s | **0.87 s** |
| solid 1 GB game | 4.02 s | 3.36 s | 7.24 s | 1.11 s | **1.08 s** |
| solid 48k source files | 31.17 s | 28.77 s | 25.50 s | 50.44 s | **8.85 s** |

With Defender on, files written by an unpackaged B-Rar are slowed dramatically, while
WinRAR, UnRAR and 7-Zip lose only ~5% and the installed package is barely affected. It is not
the number of writer threads: a plain test program creating the same 48k files took 43-48 s
with 1, 2, 4, 8 or 16 threads alike (5 s with Defender off). Nor is it a signature alone: the
same B-Rar.exe signed with the local self-signed certificate took 49.3 s against 50.6 s
unsigned. Defender evidently scans the output of programs without a reputation (WinRAR, 7-Zip
and UnRAR are signed by known publishers) and trusts the installed MSIX package. So nothing
in the code fixes it: Store / winget installs are fast, and a portable zip would need a real
code-signing certificate (e.g. Azure Trusted Signing) to be. (The 28.6 GB archive was no
longer on disk, so it wasn't re-run.)
## 2026-10-22 (00:30): phase B profiled; header scan alongside decoding

A sampling profile of phase B (`rar5_profile` with `BRAR_PROFILE_ENGINE=1`) put 39% of its
time in ucrtbase.dll: memcpy/memmove, called for every literal run and for two bulk copies
of the whole output. Measured while a game was running, back to back, test mode (`t`):

| Change | one 545 MB file | solid 1 GB game | 256 MB dictionary game |
|---|---|---|---|
| start (5ea69eb) | 0.71 s, phase B 3.3 cyc/B | 1.38 s, 2.8 | 1.46 s |
| short literal runs copied inline, not through memcpy | 0.58 s, 2.8 | 1.11 s, 2.26 | |
| window slide room 32 -> 64 MB (half the memmoves) | 0.59 s, 2.8 | 1.04 s, 2.03 | |
| drain thread reads finished bytes in the window, no copy | 0.44 s, 2.0 | 0.92 s, 1.75 | |
| header scan runs alongside decoding | **0.41 s** | **0.82 s** | **0.91 s** |

Extraction through the installed package (same load): one 545 MB file 0.51 s, solid 1 GB
game 1.08 s, 1.1 GB 2 big + 77 files 0.87 s; output identical to UnRAR's for all five
archives checked.

*Measurement trap found on the way:* while the game ran, creating many small files from an
unpackaged process (`target\release\B-Rar.exe`, or any copy of it) was 5-6x slower in kernel
time than from the installed MSIX of the very same binary (48k-file archive: 50 s vs 9 s),
presumably a driver that inspects unpackaged processes. Small-file extraction numbers taken
from unpackaged builds while that's running mean nothing.
## 2026-10-21 (late): phase B (the engine's serial step) made cheaper

Measured while a game was running on the same PC, so absolute times are ~10-25% slower
than the rest of this file; every comparison below ran back to back under the same load.
Test mode (`t`), so no disk writes.

**Prefetching copy sources.** On big archives 44% of copies reach more than 1 MB back and 13%
more than 16 MB (past the L3 cache), and phase B handles them one after another, waiting on
memory each time. It now follows the ops 16 ahead (with the same repeat-distance
bookkeeping) and prefetches each copy's source.

| Archive | no prefetch | prefetch 8 | prefetch 16 | prefetch 32 |
|---|---|---|---|---|
| one 545 MB file | 0.97 s (phase B 4.8 cyc/B) | 0.73 s | **0.71 s (3.4)** | 0.73 s |
| solid 1 GB game | 1.73 s (3.7) | 1.39 s | **1.38 s (2.8)** | 1.42 s |
| 545 MB file, 512 MB dictionary | 1.17 s (5.7) | | **0.76 s (3.6)** | |

**Window slides scale with the dictionary.** The window slid every 32 MB of output, moving
the whole dictionary each time: at a 256 MB dictionary, 8 bytes moved per byte produced.
The free room now grows with the dictionary (up to 1 GB).

| Archive | before | after | UnRAR |
|---|---|---|---|
| solid 1 GB game, `-md256m` | 2.08 s (phase B 4.5) | **1.46 s (2.9)** | 3.90 s |

Earlier the same day (idle PC): a drain thread now does the CRCs and the writing (phase B
4.2 -> 3.5 cycles/B when extracting), phase A uses half the logical CPUs (it needs only a few
to keep ahead, and more share cores with phase B), and block headers are read from a memory
map (scan 0.08 -> 0.04 s): one 545 MB file 0.81 -> 0.77 s, solid 1 GB game 1.64 -> 1.51 s.

Looked at and not done:
- *Parallel phase B.* 17-20% of the output bytes of the big archives (90% for the 48k source
  files) depend on earlier chunks, but 75-79% of the copies do, so most of the serial work
  would stay serial.
- *Faster small-file creation.* Just creating and closing 48k empty files takes 3.1 s on
  this PC (5.0 s with their data and times, 16 threads), against 6.3 s for B-Rar's whole
  extraction. Giving each thread whole folders was 5% faster but serializes an archive whose
  files are all in one folder; batches of up to 32 files from one folder gained nothing.
## 2026-10-21 (22:42): encrypted and multi-volume RAR5 through B-Rar's own decoder

RAR5 archives with encrypted data (AES-256), encrypted headers, or several volumes now use
B-Rar's decoder and engine instead of unRAR. All cores, best of 3, 5 s apart, password
"secret". "B-Rar (prev)" is the build from before the two-phase engine. The archives were made
with Rar.exe 7 `-ma5 -m3`. Every B-Rar run: 0 files left to unRAR.

| Archive | UnRAR | B-Rar (prev) | B-Rar (new) |
|---|---|---|---|
| plain: one 545 MB file (check) | 1.51 s | 1.46 s | **0.81 s** |
| plain: solid 1 GB game (check) | 3.21 s | 3.04 s | **1.64 s** |
| encrypted (`-p`): one 545 MB file | 1.77 s | 3.93 s | **0.89 s** |
| encrypted headers (`-hp`), solid 1 GB, 4 × 300 MB volumes | 3.84 s | 22.05 s ¹ | **1.83 s** |
| encrypted (`-p`), 1.1 GB, 2 big + 77 files, 12 × 100 MB volumes | 4.04 s | 9.39 s | **1.29 s** |

Decryption costs B-Rar's engine about 1.3 cycles per byte in phase A (10.3 -> 11.6 on the
545 MB file), spread over the workers. The plain archives didn't get slower (0.87 / 1.68 s
before).

¹ A bug, fixed in the same change: with encrypted headers unRAR reports a volume set as
neither solid nor multi-volume, so B-Rar split it over parallel unRAR handles, each of which
had to decode the whole solid stream (and on some sets it crashed). Volume sets are now
always one sequential unRAR pass, when unRAR is needed at all.
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
