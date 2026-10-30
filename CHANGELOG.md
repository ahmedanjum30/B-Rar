# Changelog

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
