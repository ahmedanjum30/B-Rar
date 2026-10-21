# Microsoft Store listing: B-Rar

Text and answers to paste into Partner Center. The package to upload is `B-Rar-<version>-store.msix`, from the release workflow's `store-package` artifact (or `installer\build.ps1 -Store`).

## Before the first upload

1. Partner Center > **Apps and games** > **New product** > **MSIX or PWA app**, and reserve the name **B-Rar**.
2. In the new product, **Product management > Product identity**: copy the three values into `installer\store-identity.json`:

   ```json
   { "Name": "<Package/Identity/Name>", "Publisher": "<Package/Identity/Publisher>", "PublisherDisplayName": "<Package/Properties/PublisherDisplayName>" }
   ```

   The Store package must carry exactly these, or Partner Center rejects it.

## Basics

| Field | Value |
|---|---|
| Name | B-Rar |
| Category | Utilities & tools (subcategory: File managers) |
| Pricing | Free |
| Markets | All |
| Privacy policy URL | https://github.com/ahmedanjum30/B-Rar/blob/main/PRIVACY.md |
| Website | https://github.com/ahmedanjum30/B-Rar |
| Support contact | https://github.com/ahmedanjum30/B-Rar/issues |
| Copyright | © 2026 Ahmad Anjum |

## Description

```
B-Rar opens ZIP, RAR, 7z and tar archives and extracts them faster than WinRAR, 7-Zip and Windows' own tools, using every core of your PC.

Right-click any archive in File Explorer for B-Rar's menu, in the short Windows 11 menu: Open in B-Rar, Extract files..., Extract to "folder\", Extract here, or Test archive. Or open an archive in B-Rar to look inside first: browse its folders, search it, open single files, and extract only what you pick.

• Fast: archives are extracted in parallel on all cores. On a 1.1 GB RAR archive, B-Rar takes 2 seconds, WinRAR 4 and 7-Zip 8. Archives with tens of thousands of small files extract three to four times faster than with WinRAR or 7-Zip.
• Formats: ZIP (including AES-encrypted), RAR 4 and 5, 7z, tar, tar.gz, tar.xz, tar.zst, tar.bz2, and single .gz, .xz, .zst and .bz2 files.
• Archives in parts: RAR volumes (.part1.rar, .r00) and split archives (.001). Start from any part.
• Passwords: asks when an archive is encrypted, including archives with encrypted file names.
• Safe: asks before replacing files (replace, skip, keep both), keeps files inside the folder you chose, and checks every file's checksum.
• Protected folders: extracting into Program Files or other folders that need administrator rights shows the Windows permission prompt.
• Command line: B-Rar x archive.rar from any terminal.
• Light: a native app with no bundled browser engine. No ads, no tracking, no internet connection.

B-Rar extracts archives; it doesn't create them.

Free for personal and other non-commercial use.
```

## Short description (Windows 11 search, about 100 characters)

```
Extract ZIP, RAR, 7z and tar archives faster than WinRAR and 7-Zip, from the right-click menu.
```

## Features (one per line, up to 20)

```
Extracts ZIP, RAR, 7z and tar archives on all cores at once
Windows 11 right-click menu: Extract here, Extract to folder, Extract files, Test
Browse an archive and extract only the files you pick
Archives in parts: RAR volumes and split .001 archives
Password-protected archives, including encrypted file names
Asks before replacing files: replace, skip or keep both
Extracts into protected folders with the Windows permission prompt
Command line: B-Rar x archive.rar
No ads, no tracking, no internet connection
```

## Search terms (up to 7)

```
rar, zip, 7z, unzip, unrar, extract, archive
```

## What's new in this version

```
B-Rar 1.0 is here: fast extraction of ZIP, RAR, 7z and tar archives, right from File Explorer.

• Extract here, extract to a folder, or pick where, from the Windows 11 right-click menu
• Browse an archive before extracting, and take out just the files you need
• RAR archives extract twice as fast as with WinRAR; archives full of small files three to four times as fast
• Passwords, multi-part archives and protected folders are all handled for you
```

## Restricted capabilities (Submission options)

Partner Center asks why the package declares these. As submitted for 1.0:

**runFullTrust**

```
B-Rar is a Win32 desktop app (written in Rust) packaged as MSIX. It needs full trust for three things:

1. Reading archives and writing extracted files anywhere on disk. Users open archives from any folder or drive (Downloads, external disks, network shares) and extract them to a folder they choose. B-Rar reads only the archive the user opened and writes only to the destination the user picked.

2. The File Explorer right-click menu. The package registers a context-menu handler (windows.fileExplorerContextMenus with a COM surrogate server, B-Rar-Shell.dll). When the user right-clicks an archive, it shows "Open in B-Rar", "Extract files...", "Extract to folder", "Extract here" and "Test archive", and starts B-Rar-GUI.exe for the chosen action.

3. The command line. The app execution alias B-Rar.exe lets users extract archives from any terminal.

B-Rar doesn't connect to the internet, collects no data, and changes nothing outside the folders the user picks.
```

**allowElevation**

```
B-Rar uses elevation for one case only: when the user chooses to extract an archive into a folder that only administrators can write to, such as C:\Program Files.

Before extracting, B-Rar checks whether it can write to the chosen folder. If Windows denies access, B-Rar restarts that single extraction with administrator rights, and Windows shows its UAC prompt so the user decides. If the user declines, nothing is extracted and B-Rar explains that the folder needs administrator permission.

B-Rar never elevates on its own or at startup. Everything else (browsing archives, extracting to normal folders, the right-click menu) runs without administrator rights.
```
## Age rating questionnaire

Category: **Utility, productivity or other app**. No user-generated content shared with others, no communication between users, no location sharing, no in-app purchases, no ads, no personal data collected. Expected rating: 3+ / Everyone.

## Images

All in `store/images/` (screenshots 1920×1080; made from a demo archive, see the captions):

| File | Used for | Caption |
|---|---|---|
| `logo-300.png` | 1:1 app tile icon (300×300) | |
| `1-browse.png` | Screenshot 1 | Look inside an archive and extract only what you pick |
| `2-extracting.png` | Screenshot 2 | Extracts on every core: 1.2 GB in under 2 seconds |
| `3-done.png` | Screenshot 3 | Every file checked as it's extracted |
| `4-home.png` | Screenshot 4 | Open an archive, or right-click one in File Explorer |

## Other fields

| Field | Value |
|---|---|
| Additional license terms | Free for personal and other non-commercial use under the PolyForm Noncommercial License 1.0.0: https://github.com/ahmedanjum30/B-Rar/blob/main/LICENSE |
| Developed by | Ahmad Anjum |
| Personal information | Partner Center forces "Yes" for any app with runFullTrust; the privacy policy URL is what matters. |
## Notes for certification (optional field)

```
B-Rar has no account or sign-in. To try it: right-click any .zip or .rar file in File Explorer > B-Rar > Extract here, or B-Rar > Open in B-Rar to see its browser window.
```

## winget

Nothing to submit: once the app is live, winget's built-in `msstore` source lists it, so `winget install B-Rar` (or `winget install <Store product ID>`) works.
