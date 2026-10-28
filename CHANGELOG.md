# Changelog

## Unreleased

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
