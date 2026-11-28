# Changelog

## 1.5.3 (2026-11-28)

- **ZIPs on a hard drive extract at the drive's speed.** B-Rar read a ZIP from as many places at once as there are cores, which is what makes it fast on an SSD; on a hard drive it kept the head jumping between them, and a 14 GB ZIP took 27 minutes at 3-4 MB/s where WinRAR took 3 minutes 25 seconds. When Windows says the archive is on a drive that seeks slowly, B-Rar now reads it in order, one big piece at a time, while still decoding and writing on every core. That 14 GB ZIP now takes 2 minutes 50 seconds, ahead of WinRAR.
- **Cancel stops straight away.** A ZIP entry (up to 2 GB) was read in one go before B-Rar checked for a cancel, which on a slow drive could take minutes. Reads now come in 16 MB pieces with a check between each, and the window says "Cancelling..." until it stops. The files already extracted stay; only the one being written is removed, since it would be incomplete.
- **Mac and Linux: an archive with a link and a folder of the same name** no longer stops with "File exists" now and then. The entries are extracted in parallel, so the folder could appear just before the link was made; the link is now skipped as it is when the folder was there first.

## 1.5.2 (2026-11-27)

- **Extract part 1 while part 2 is still downloading.** When the next part of a RAR archive isn't there yet (not next to the others, nor where an earlier part was found), B-Rar now extracts everything in the parts it has first, and asks for the missing part only when it gets to it. It used to ask before extracting anything, because it reads every part's file list up front. Until the part is found the progress bar sweeps instead of filling (the total isn't known yet), and the archive goes through unRAR, one file at a time. Archives with all their parts at hand are unchanged. An empty file under a part's name (Firefox makes one while it downloads) no longer counts as the part.
- **A memory over-read fixed** in the bridge to unRAR: when unRAR moved on to the next part of an archive, B-Rar read the part's name past the end of the buffer holding it. It went unnoticed in practice, but could have crashed.

## 1.5.1 (2026-11-24)

- **Big ZIP entries made of fixed-Huffman blocks extract on all cores**, instead of 15x slower than on one. zlib-ng's and zlib-rs's fastest level (B-Rar's own "Fastest" among them) write one long fixed-Huffman block, which the parallel decoder couldn't find a starting point in: it searched every bit on every core, then decoded the file alone. It now starts pieces inside such blocks. A 290 MB entry: 10.5 s → 0.15 s; a 3.4 GB ZIP of them on a Mac: 80.7 s → 5.2 s. Every other ZIP is as fast as before.
- **On a Mac, B-Rar extracts ZIPs about 5x faster than macOS's own tools:** a 3.4 GB ZIP of 20,008 files in 2.3 s, against 11.0 s with ditto (Archive Utility's engine), 10.4 s with bsdtar and 22.7 s with unzip (M3 Pro).
- Built with Rust 1.100.

## 1.5.0 (2026-11-19)

- **B-Rar for Mac** (Apple Silicon, macOS 13 or later): `brew install --cask ahmedanjum30/tap/b-rar`. The archive browser (folders, search, sorting, opening a file, extracting what you pick), progress with pause and cancel, and the same questions as on Windows (replace, password, a missing part), in a native Mac window (Swift and SwiftUI) around the same Rust engine. Finder's right-click menu gets B-Rar's Quick Actions: Extract Here, Extract to Folder, Extract Files…, Open in B-Rar, Test Archive, Add to ZIP and Add to Archive…. Homebrew also adds the `brar` command line.
- **Apple Silicon:** the RAR5 decoder builds on ARM processors (one memory prefetch was x86-only).
- **Symbolic links** in zip, 7z and tar archives are made on Mac and Linux (zipped apps and frameworks need theirs, `Versions/Current`), as long as they point inside the destination folder; ones that would point outside are refused and counted as not extracted. Zip links were written as small files holding the target's path before; on Windows they're now skipped, as tar's were.
- **Zips made on a Mac** (Finder's Compress) extract on a Mac as Archive Utility does: the `._name` and `__MACOSX` entries holding each file's Mac metadata are put back on the files instead of left as stray files, so an extracted app keeps a valid signature. The browser hides them.

## 1.4.1 (2026-11-04)

- **The browser no longer jumbles its page** when the update bar appears ("Check for updates") or the page changes: moved buttons and text left copies of themselves behind.

## 1.4.0 (2026-11-04)

- **New look: the Workbench, half the size.** The archive browser and the progress window are rebuilt in plain Windows controls, without a UI framework (`B-Rar-GUI.exe` 8.9 MB → 4.1 MB), in a new design (DESIGN.md): a toolbar of big icon buttons (Extract to, Test, Open, Info, Updates), an address bar with the path, a dense list with alternate rows and each file type's Windows icon, a status bar with fields, and progress shown as Elapsed / Left / Processed / Speed. It follows Windows' light or dark setting.
- **Info and Updates buttons.** Info shows the archive's format, files, sizes and ratio; Updates checks for a new version on demand.

## 1.3.2 (2026-11-04)

- **Microsoft Store version: no administrator prompt.** The Store doesn't allow B-Rar to ask for administrator rights, so its package no longer declares that capability. Extracting into a folder that needs them (Program Files, say) explains how to run B-Rar as administrator from the Start menu, with a button to choose another folder. The installer, portable and GitHub MSIX versions still show the Windows prompt.

## 1.3.1 (2026-11-04)

- **Damaged big files in RAR archives are reported once.** A stored (uncompressed) file whose checksum didn't match was checked a second time by unRAR, which for a 125 GB file meant reading it all again while the progress ran past 100%. It is now reported as damaged straight away (a 136 GB test: 90 s instead of 170 s), and the other files carry on. For a file split across RAR volumes, the message names the part that is damaged (each part keeps a checksum of its piece), so only that part needs downloading again. Whenever a file does go to unRAR for a second try, its bytes come off the progress first, so it never passes 100%.

## 1.3.0 (2026-10-31)

- **Parts in different folders.** When the next part of a RAR set or of a split (`.001`) archive isn't next to the first, B-Rar asks where it is, like WinRAR: a file picker in the app, a question on the command line. Parts after it are then looked for in the same folder, so you point at each folder once. Works for RAR5 on B-Rar's own decoder and for RAR 4 through unRAR.
- **Steadier "time left".** The speed is averaged over about 5 seconds and the time left updates once a second, easing toward the new estimate, instead of jumping with every burst of small or big files.

## 1.2.0 (2026-10-29)

- **The command line on Linux.** `B-Rar-<version>-linux-x64.tar.gz` has `brar` for x86-64 Linux: the same extraction (ZIP, RAR, 7z, tar and the rest) and ZIP creation as on Windows. Files keep the permissions stored in the archive, and ZIPs made on Linux store them.
- Website and Microsoft Store search terms include "BRar", without the hyphen.

## 1.1.0 (2026-10-29)

- **B-Rar makes ZIP archives.** Right-click files or folders, **B-Rar**: **Add to archive...** (name, level, AES-256 password) or **Add to "name.zip"** (one B-Rar submenu, like WinRAR's: extract commands on archives, add commands on anything else); or `B-Rar a backup.zip folder`. Every core compresses at once, a big file in pieces; already-compressed files (photos, videos, archives) are stored. 1.1 GB of programs in 1.7 s at Normal level.
- **The installer is made with [B-Installer](https://github.com/ahmedanjum30/B-Installer)** instead of Inno Setup: the same per-user install, right-click menu, Open with and PATH option. It updates and uninstalls copies installed by B-Rar 1.0.0's Inno Setup installer.
- **Own LZMA2 decoder for 7z.** 7z archives in 7-Zip's default format (LZMA2) now use B-Rar's own decoder: 1.5x faster than before on all cores, level with 7-Zip on a 1.25 GB archive (3.9 s), and archives 7-Zip made on several threads are decoded on all cores. Archives of programs (LZMA2 behind the x86 BCJ filter, which 7-Zip adds for .exe and .dll files) and older LZMA archives use it too: 1.2x faster than before, level with 7-Zip on BCJ. B-Rar now reads 7z entirely with its own code (headers, every method and filter, encryption, split archives), without sevenz-rust2: split 7z archives decode on all cores too (3.75 s, was 5.74 s).
- **Streams that can't be split** (one-thread LZMA2, LZMA) decode on one core while another runs the BCJ filter, the CRCs and the writing: 9.57 s on a 1.1 GB LZMA archive of programs, ahead of 7-Zip's 9.90 s.
- **ZIP LZMA and XZ entries** are decoded by B-Rar too (LZMA with B-Rar's decoder, XZ with liblzma), including encrypted ones (ZipCrypto, WinZip AES), so lzma-rust2 is no longer part of B-Rar either.

## 1.0.0 (2026-10-27)

First release on GitHub.

- **Fast extraction on every core.** ZIP, RAR 4 and 5, 7z and tar archives (and .gz, .xz, .zst, .bz2), many files at once, written while decoding.
- **Own decoders.** B-Rar's RAR5 decoder (faster than 7-Zip's on one core) handles solid, encrypted and multi-volume archives, with big files and solid archives decoded on all cores. B-Rar's Deflate decoder for ZIP is 7% faster than libdeflate on large files on one core, and splits a single big file across all cores (a 545 MB file in 0.58 s).
- **Windows integration.** The right-click menu of archives, **Open with**, an archive browser with search and selective extraction, passwords, archives in parts (RAR volumes, `.001` files), and the command line (`B-Rar x archive.rar`).
- **Installer and updates.** `B-Rar-Setup` installs without admin rights; installed and portable copies say when a new version is out and update when you click **Update**.

The Microsoft Store version (coming soon) was submitted from an earlier build, also numbered 1.0.0.
