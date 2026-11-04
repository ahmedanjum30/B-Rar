# Product

B-Rar: a fast archive extractor for Windows (and a command line for Linux), written in Rust by Ahmad Anjum.

## Platform

web

(The product is a native Windows app, `crates/brar-gui`, plain Win32. Design work happens on HTML mock-ups in `design/` and is then rebuilt natively; `web` is what the design tooling previews in. The website in `docs/` is web too.)

## Stack

Windows app: Rust + plain Win32 (standard controls, custom-drawn where DESIGN.md asks), no UI framework, for size and start-up speed. Website: static HTML/CSS on GitHub Pages. Design mock-ups: static HTML/CSS (`design/screens.html`).

## Users

People on Windows 10 and 11 who download archives (games, software, photo and document bundles) and want them open fast; they reach B-Rar from File Explorer's right-click menu far more often than from the Start menu. Some use the command line. (Inferred: mostly home users and enthusiasts; the licence rules out commercial use.)

## Product Purpose

Extract ZIP, RAR, 7z and tar archives faster than WinRAR and 7-Zip, using every core, and make ZIP archives the same way, with as little UI as possible between the right-click and the result.

## Positioning

"WinRAR, but faster, free for personal use, and from the Microsoft Store." Benchmarks are the proof (benches/RESULTS.md). Creates ZIP only (RAR creation isn't licensed).

## Operating Context

Short, task-shaped sessions: a progress window that appears for one action and closes itself; an archive browser opened to look inside and pull out a few files. Often over File Explorer, often with large archives (tens to hundreds of GB).

## Capabilities and Constraints

- Reads ZIP, RAR 4/5 (multi-part, encrypted), 7z, tar and .gz/.xz/.zst/.bz2; writes ZIP (with AES-256).
- Two windows: the archive browser and the progress window (DESIGN.md lists every screen).
- The Microsoft Store version can't ask for administrator rights.
- Every byte of the program counts: the GUI is plain Win32 to stay small (about 4 MB).

## Brand Commitments

- The name is always written "B-Rar".
- The logo is the blue "B" with a zipper (assets/brar-256.png).
- Looks like part of Windows 11 (Segoe UI, system light/dark), not a themed skin.

## Evidence on Hand

Benchmark results against WinRAR and 7-Zip (benches/RESULTS.md), the Microsoft Store listing, the website (docs/).

## Product Principles

- Speed is the feature; the UI should never be the slow part.
- Say plainly what happened and what to do next.
- Get out of the way: windows close themselves when the job is done.

## Accessibility & Inclusion

Keyboard operable throughout (Tab, Enter, Esc, list keys), follows the system light/dark setting and text scaling. (Inferred: no specific accessibility commitments recorded yet.)
