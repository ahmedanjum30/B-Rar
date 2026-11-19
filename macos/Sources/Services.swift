// B-Rar's Quick Actions in Finder's right-click menu (macOS Services, declared under
// NSServices in the app's Info.plist by macos/build-app.sh): the Mac counterpart of the
// Windows Explorer menu.

import AppKit

@MainActor
final class Services: NSObject {
    private func files(_ pasteboard: NSPasteboard) -> [URL] {
        (pasteboard.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL]) ?? []
    }

    private func archives(_ pasteboard: NSPasteboard) -> [URL] {
        files(pasteboard).filter(Engine.isArchive)
    }

    @objc func openArchive(_ pasteboard: NSPasteboard, userData: String?, error: AutoreleasingUnsafeMutablePointer<NSString?>) {
        archives(pasteboard).forEach(Router.browse)
    }

    /// Next to the archive, as WinRAR's "Extract here".
    @objc func extractHere(_ pasteboard: NSPasteboard, userData: String?, error: AutoreleasingUnsafeMutablePointer<NSString?>) {
        for archive in archives(pasteboard) {
            Router.run(.extract(archive: archive, dest: archive.deletingLastPathComponent()))
        }
    }

    /// Into a folder named after the archive.
    @objc func extractToFolder(_ pasteboard: NSPasteboard, userData: String?, error: AutoreleasingUnsafeMutablePointer<NSString?>) {
        for archive in archives(pasteboard) {
            Router.run(.extract(archive: archive, dest: Engine.suggestedDir(archive)))
        }
    }

    /// Asks where first.
    @objc func extractFiles(_ pasteboard: NSPasteboard, userData: String?, error: AutoreleasingUnsafeMutablePointer<NSString?>) {
        for archive in archives(pasteboard) {
            guard let dest = chooseFolder(for: archive) else { continue }
            Router.run(.extract(archive: archive, dest: dest))
        }
    }

    @objc func testArchive(_ pasteboard: NSPasteboard, userData: String?, error: AutoreleasingUnsafeMutablePointer<NSString?>) {
        for archive in archives(pasteboard) { Router.run(.test(archive: archive)) }
    }

    /// "name.zip" next to the files, at once.
    @objc func addToZip(_ pasteboard: NSPasteboard, userData: String?, error: AutoreleasingUnsafeMutablePointer<NSString?>) {
        let inputs = files(pasteboard)
        guard !inputs.isEmpty else { return }
        Router.run(.create(inputs: inputs, archive: defaultArchiveName(for: inputs), level: 6, password: nil, overwrite: false))
    }

    /// The "Add to archive" window first: name, compression, password.
    @objc func addToArchive(_ pasteboard: NSPasteboard, userData: String?, error: AutoreleasingUnsafeMutablePointer<NSString?>) {
        let inputs = files(pasteboard)
        guard !inputs.isEmpty else { return }
        Router.show({ AddArchiveView(inputs: inputs, window: $0) }, title: "Add to archive", size: NSSize(width: 480, height: 330), resizable: false)
    }
}

/// Where an extraction goes when the user picks it.
@MainActor
func chooseFolder(for archive: URL) -> URL? {
    let panel = NSOpenPanel()
    panel.message = "Extract \(archive.lastPathComponent) to"
    panel.prompt = "Extract"
    panel.canChooseDirectories = true
    panel.canChooseFiles = false
    panel.canCreateDirectories = true
    panel.directoryURL = archive.deletingLastPathComponent()
    NSApp.activate(ignoringOtherApps: true)
    return panel.runModal() == .OK ? panel.url : nil
}

/// "name.zip" next to the first item: its name for one item, the folder's for several.
func defaultArchiveName(for inputs: [URL]) -> URL {
    let first = inputs[0]
    let folder = first.deletingLastPathComponent()
    let name = inputs.count == 1 ? first.deletingPathExtension().lastPathComponent : folder.lastPathComponent
    return folder.appendingPathComponent((name.isEmpty ? "Archive" : name) + ".zip")
}
