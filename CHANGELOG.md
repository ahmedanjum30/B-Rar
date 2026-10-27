# Changelog

## 1.0.0 (2026-10-27)

First release on GitHub.

- **Fast extraction on every core.** ZIP, RAR 4 and 5, 7z and tar archives (and .gz, .xz, .zst, .bz2), many files at once, written while decoding.
- **Own decoders.** B-Rar's RAR5 decoder (faster than 7-Zip's on one core) handles solid, encrypted and multi-volume archives, with big files and solid archives decoded on all cores. B-Rar's Deflate decoder for ZIP is 7% faster than libdeflate on large files on one core, and splits a single big file across all cores (a 545 MB file in 0.58 s).
- **Windows integration.** The right-click menu of archives, **Open with**, an archive browser with search and selective extraction, passwords, archives in parts (RAR volumes, `.001` files), and the command line (`B-Rar x archive.rar`).
- **Installer and updates.** `B-Rar-Setup` installs without admin rights; installed and portable copies say when a new version is out and update when you click **Update**.

The Microsoft Store version (coming soon) was submitted from an earlier build, also numbered 1.0.0.
