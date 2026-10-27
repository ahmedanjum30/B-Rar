# Changelog

## 1.1.0 (2026-10-27)

**ZIP**
- B-Rar's own Deflate decoder replaces libdeflate. On one core it's 7% faster than libdeflate on large files and level with it on small text files.
- A big file inside a ZIP (64 MB and up) now decodes on all cores: a 545 MB file extracts in 0.58 s instead of 1.61 s.

**RAR**
- RAR5 archives that are encrypted (including encrypted file names) or split into volumes now use B-Rar's own decoder too.
- Big files and solid archives decode on all cores (a two-phase engine), and the decoder itself uses fewer cycles per byte: a 1 GB solid archive extracts in about 1 s, 3–4x faster than WinRAR and UnRAR.

**Updates**
- The portable version and the sideloaded MSIX say when a new version is out, and update when you click **Update** (checked against the release's SHA-256 sums). The Microsoft Store version keeps updating through the Store.

## 1.0.0 (2026-10-21)

First version, submitted to the Microsoft Store: ZIP, RAR 4 and 5, 7z and tar archives extracted on all cores, the Windows 11 right-click menu, an archive browser with selective extraction, passwords, archives in parts, and the command line.
