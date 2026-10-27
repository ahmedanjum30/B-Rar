# Privacy policy

This policy covers B-Rar: the app, its command line (`B-Rar.exe`) and its right-click menu.

**Short version:** B-Rar doesn't collect, send or sell any data. It only asks GitHub whether a newer version is out; the Microsoft Store version doesn't connect to the internet at all.

## What B-Rar does with your files

B-Rar reads the archives you open and writes the files you extract, on your own computer. When you open a single file from an archive, it's extracted to a temporary folder (`%TEMP%\B-Rar`), which B-Rar deletes when you close it. Passwords you type are kept in memory only while B-Rar runs, and never written to disk.

## Update checks

The Microsoft Store version is updated by the Store and never connects to the internet itself.

The versions from GitHub (the installer, the portable files, and the MSIX package signed with B-Rar's own certificate) check for a newer version when the window opens: it asks `github.com/ahmedanjum30/B-Rar/releases/latest` which version is the newest, and, if there's a newer one, downloads that release's list of checksums. Like any web request, this reaches GitHub with your internet address and the app's name and version (such as `B-Rar/1.0.0`); nothing about you, your files or your archives is sent. The update itself is only downloaded when you click **Update**.

## Sharing

B-Rar doesn't share data with anyone. There are no accounts, ads or analytics.

## Contact

Questions about this policy: open an issue at https://github.com/ahmedanjum30/B-Rar/issues.

_Last updated: October 27, 2026_
