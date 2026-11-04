---
name: B-Rar
description: A fast archive extractor for Windows; a workbench for archives, in Windows' own controls.
colors:
  accent: "#1F6FEB"
  accent-dark: "#4C9AFF"
  surface: "#F0F0F0"
  surface-dark: "#252526"
  field: "#FFFFFF"
  field-dark: "#1E1E1E"
  row-alt: "#F7F7F7"
  row-alt-dark: "#232324"
  border: "#BDBDBD"
  border-dark: "#4A4A4A"
  text: "#141414"
  text-dark: "#E8E8E8"
  text-weak: "#5A5A5A"
  text-weak-dark: "#A0A0A0"
  select: "#CCE3FF"
  select-dark: "#264F78"
  hover: "#E5F0FF"
  hover-dark: "#2D3A4A"
  ok: "#107C10"
  ok-dark: "#6CCB5F"
  error: "#C42B1C"
  error-dark: "#F48771"
  warn: "#A35A00"
  warn-dark: "#E5C07B"
typography:
  title:
    fontFamily: "Segoe UI Semibold, Segoe UI, sans-serif"
    fontSize: "1.15em (of the Windows message font)"
    fontWeight: 600
  body:
    fontFamily: "Segoe UI, sans-serif"
    fontSize: "Windows message font (9 pt)"
    fontWeight: 400
  small:
    fontFamily: "Segoe UI, sans-serif"
    fontSize: "0.9em"
    fontWeight: 400
  glyph:
    fontFamily: "Segoe Fluent Icons, Segoe MDL2 Assets"
    fontSize: "24px (toolbar, status), 14px (small)"
rounded:
  control: "3px"
  bar: "2px"
spacing:
  page-x: "24px"
  page-y: "20px"
  gap: "8px"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "#FFFFFF"
    rounded: "{rounded.control}"
    height: "button: text + 14px"
  button:
    backgroundColor: "{colors.field}"
    textColor: "{colors.text}"
    rounded: "{rounded.control}"
  tool-button:
    backgroundColor: "transparent; {colors.hover} hovered"
    textColor: "{colors.text}"
    size: "70 × 56px"
  progress:
    backgroundColor: "{colors.field}"
    rounded: "{rounded.bar}"
    height: "14px"
---

# Design System: B-Rar

## Screens

Every screen B-Rar shows, in its two windows. The archive browser opens from the Start menu or by opening an archive; the progress window opens for every right-click action (extract, test, add) and closes itself when done or cancelled. Native file and folder pickers, Explorer's right-click menu and the installer are Windows' own and not part of this system.

**Archive browser** (resizable, 920 × 600, min 560 × 320)

1. **Home**: no archive yet. Logo (96 px), "B-Rar", one-line purpose, **Open archive...** (primary), drop hint, Explorer hint, version, "Check for updates" and "Support B-Rar" links.
2. **Loading**: "Reading name.zip..." over a sweeping bar.
3. **Browse**: the toolbar of icon buttons (**Extract to**, **Test**, **Open**, **Info**, **Updates**) with the search field on the right; the address row (Up, and a box with the logo and the path, each folder a link); the list (Name with the file type's Windows icon, Size, Packed, Modified; sortable; alternate rows; Workbench selection); the status bar (selection or folder summary · format, file count, size → packed size, and messages such as "Opening x...").
4. **Browse, searching**: results from all subfolders with relative paths; "No matches"; "This folder is empty".
5. **Password**: lock glyph, "Password required", "name is encrypted", password field, Show password, "That password is incorrect", OK / Cancel.
6. **Couldn't open**: error glyph, the message, **Open another...** / Close.
7. **Update bar** (across the top): "B-Rar 1.4.0 is available." with Update, What's new, Close; also checking, downloading %, up to date, failed with Try again. **Updates** on the toolbar checks on demand (in the Store version it says the Store keeps B-Rar up to date).
8. **Info**: a message box with the archive's path, format, files, folders, size, packed size and ratio.

**Progress window** (fixed width per page, height follows content)

9. **Working**: "Extracting to D:\folder" / "Testing name.zip" / "Compressing to name.zip"; the current file; the bar (accent, amber when paused); a grid of Elapsed, Left, Files, Processed, Speed; Pause / Cancel. The title bar: "27% Extracting name.zip".
10. **Confirm file replace**: warning glyph, "The folder already has name", the folder; Replace / with rows (size, date); Apply to all; **Yes** / No / Rename / Cancel.
11. **Password**: as in the browser (5).
12. **Done**: ok glyph, "Done: 214 files extracted, no errors" (or "No errors found in 214 files" after a test); grid of Size, Time, Folders, Speed; "Saved to folder"; **Open folder** / Close.
13. **Archive created**: ok glyph, "Archive created: name.zip", the folder; Size (original → packed), Time, Files, Ratio; **Show in folder** / Close.
14. **Couldn't extract / test / create**: error glyph, the message; Close. Microsoft Store version, protected folder: how to run as administrator, **Choose another folder** / Close.
15. **Add to archive**: logo, "Add to archive", what's added; Archive path; Compression (Store · Fastest · Normal · Best); Encrypt with a password (AES-256) with password and confirm; the problem or "will be replaced" line; **Create** (or **Replace**) / Cancel.

## Overview

**Creative North Star: "The Workbench"**

B-Rar is a tool for people who handle archives, in the line of WinRAR rather than a consumer app: a toolbar of big labelled icon buttons, an address bar, a dense list, a status bar with fields, and progress shown as numbers in a grid. Everything is built from what Windows itself draws (Segoe UI, Windows' icon font, the system list, title bars and pickers), so it stays small and looks at home.

It follows Windows' light or dark app setting (BRAR_THEME=light or dark overrides it). The logo is the one piece of brand: in the title bars, on the home page, in the address box and on "Add to archive".

**Key Characteristics:**
- Big icon buttons with labels; glyphs from Windows' own icon font, in the accent colour.
- Dense rows, alternate row tints, square-ish 3 px corners.
- Numbers laid out as label/value grids, not sentences.
- One accent (cobalt blue), three status colours, otherwise neutral greys.

## Colors

Neutral greys carry the surfaces; one cobalt blue for what to press, what's moving and what's selected.

### Primary
- **Cobalt** (#1F6FEB; #4C9AFF dark): the primary button, the progress fill, toolbar glyphs, focus, links.

### Neutral
- **Workbench Grey** (#F0F0F0; #252526 dark): window, toolbar, status bar.
- **Field** (#FFFFFF; #1E1E1E dark): the list, text fields, buttons; **Row Tint** (#F7F7F7; #232324) every other row.
- **Rule** (#BDBDBD; #4A4A4A): borders, the toolbar's and status bar's lines, the field dividers.
- **Ink** (#141414; #E8E8E8) and **Muted Ink** (#5A5A5A; #A0A0A0): text, labels, secondary columns.
- **Selection** (#CCE3FF; #264F78) for selected rows; **Hover** (#E5F0FF; #2D3A4A) for hovered tools and the update bar.

### Status
- **Done Green** (#107C10; #6CCB5F), **Fault Red** (#C42B1C; #F48771), **Caution Amber** (#A35A00; #E5C07B): status glyphs, the error line, the paused bar.

**The One Fill Rule.** Only the primary button is filled with the accent; elsewhere blue is a glyph, a bar or a tint.

## Typography

**Body Font:** Segoe UI at Windows' message-font size. **Titles:** Segoe UI Semibold, 1.15×. **Glyphs:** Segoe Fluent Icons (Segoe MDL2 Assets before Windows 11).

**Character:** Windows' own voice; titles say what happened ("Done: 214 files extracted, no errors").

## Layout

Progress-window pages stack inside 24 px side and 20 px top/bottom margins and end in a right-aligned button row; the window takes the page's height. The browser is toolbar (5 px padding, 70 × 56 px tools), address row (26 px), list, status bar (26 px, split 45/55). Centred pages (home, loading, password, error) sit in a 440 px column.

## Elevation & Depth

Flat. Separation is 1 px rules (under the toolbar, above the status bar, around fields and the path box) and the grey/field contrast. No shadows, gradients or translucency.

## Shapes

Nearly square: 3 px corners on buttons and boxes, 2 px on the progress bar; status glyphs are the icon font's own circles.

## Components

### Buttons
- **Primary:** Cobalt fill, white (dark: black) text, 3 px corners. **Secondary:** Field with a Rule border; hover turns the border Cobalt and the fill Hover; pressed takes the Selection tint.

### Tool buttons
- Glyph (24 px, Cobalt) over a small label; no frame until hovered (Hover tint) or pressed (Selection tint).

### Check boxes and radio buttons
- Painted: 18 px box (3 px corners) or circle in Field with a Muted border; checked fills Cobalt with a white check, or a Cobalt dot.

### Progress bar
- 14 px, Field with a Rule border, Cobalt fill (Caution Amber paused), a sweeping segment while the total is unknown.

### File list
- Windows' list view in report mode: the system's file-type icons, Workbench row colours (alternate tint, Selection), Size and Packed right-aligned, Packed and Modified in Muted Ink.

## Do's and Don'ts

### Do:
- **Do** use Windows' icon font for glyphs and Windows' own file icons in the list.
- **Do** show numbers in label/value grids.
- **Do** follow the system light/dark setting with the paired colours above.

### Don't:
- **Don't** change the logo or use it as decoration beyond its places above.
- **Don't** use shadows, gradients, blur or translucency.
- **Don't** add a second accent colour.
