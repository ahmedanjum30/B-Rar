// B-Rar for macOS: the app around the Rust engine (crates/brar-ffi).
//
// AppKit runs the app and its windows, SwiftUI draws what's in them. Windows open the same
// way whether an archive is double-clicked in Finder, picked from B-Rar's Quick Actions
// (Finder's right-click menu, Services.swift) or opened from B-Rar itself.

import AppKit
import SwiftUI

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Whether something opened a window while the app was starting (a double-clicked
    /// archive, a Quick Action), so the home window isn't needed.
    private var openedSomething = false

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.mainMenu = makeMenu()
        NSApp.servicesProvider = Services()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Finder's "open" and Quick Actions arrive just after launching.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [self] in
            if !openedSomething { Router.showHome() }
        }
    }

    func application(_ application: NSApplication, open urls: [URL]) {
        openedSomething = true
        urls.forEach(Router.browse)
    }

    func noteOpened() { openedSomething = true }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        if !hasVisibleWindows { Router.showHome() }
        return true
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    @objc func openDocument(_ sender: Any?) { Router.chooseAndBrowse() }

    @objc func showAbout(_ sender: Any?) {
        NSApp.orderFrontStandardAboutPanel(options: [
            .applicationName: "B-Rar",
            .applicationVersion: Engine.version,
            .credits: NSAttributedString(string: "A fast archive extractor.\nFree for personal and other non-commercial use."),
        ])
    }

    private func makeMenu() -> NSMenu {
        let main = NSMenu()
        func submenu(_ title: String, _ items: [NSMenuItem]) {
            let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            let menu = NSMenu(title: title)
            items.forEach(menu.addItem)
            item.submenu = menu
            main.addItem(item)
        }
        func item(_ title: String, _ action: Selector?, _ key: String = "", _ modifiers: NSEvent.ModifierFlags = .command) -> NSMenuItem {
            let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
            item.keyEquivalentModifierMask = modifiers
            return item
        }
        submenu("B-Rar", [
            item("About B-Rar", #selector(showAbout(_:))),
            .separator(),
            item("Hide B-Rar", #selector(NSApplication.hide(_:)), "h"),
            item("Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option]),
            item("Show All", #selector(NSApplication.unhideAllApplications(_:))),
            .separator(),
            item("Quit B-Rar", #selector(NSApplication.terminate(_:)), "q"),
        ])
        submenu("File", [
            item("Open Archive…", #selector(openDocument(_:)), "o"),
            .separator(),
            item("Close", #selector(NSWindow.performClose(_:)), "w"),
        ])
        // Text fields (passwords, names) need these for copy and paste.
        submenu("Edit", [
            item("Undo", Selector(("undo:")), "z"),
            item("Redo", Selector(("redo:")), "z", [.command, .shift]),
            .separator(),
            item("Cut", #selector(NSText.cut(_:)), "x"),
            item("Copy", #selector(NSText.copy(_:)), "c"),
            item("Paste", #selector(NSText.paste(_:)), "v"),
            item("Select All", #selector(NSText.selectAll(_:)), "a"),
        ])
        let window = [
            item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m"),
            item("Zoom", #selector(NSWindow.performZoom(_:))),
        ]
        submenu("Window", window)
        NSApp.windowsMenu = main.items.last?.submenu
        return main
    }
}

/// Opens B-Rar's windows.
@MainActor
enum Router {
    private static var windows: Set<NSWindow> = []
    private static var home: NSWindow?

    /// A window showing `view`. It stays alive (and the app with it) until it's closed.
    @discardableResult
    static func show<V: View>(_ view: (WindowHandle) -> V, title: String, size: NSSize, resizable: Bool = true) -> NSWindow {
        let handle = WindowHandle()
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: size),
            styleMask: resizable ? [.titled, .closable, .miniaturizable, .resizable] : [.titled, .closable, .miniaturizable],
            backing: .buffered, defer: false)
        window.title = title
        window.isReleasedWhenClosed = false
        window.contentViewController = NSHostingController(rootView: view(handle))
        window.setContentSize(size)
        handle.window = window
        windows.insert(window)
        NotificationCenter.default.addObserver(forName: NSWindow.willCloseNotification, object: window, queue: .main) { note in
            MainActor.assumeIsolated {
                if let closed = note.object as? NSWindow { windows.remove(closed) }
            }
        }
        // Each new window a little below and right of the last one.
        if let last = NSApp.keyWindow, last !== window {
            window.setFrameTopLeftPoint(window.cascadeTopLeft(from: NSPoint(x: last.frame.minX, y: last.frame.maxY)))
        } else {
            window.center()
        }
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        (NSApp.delegate as? AppDelegate)?.noteOpened()
        return window
    }

    static func showHome() {
        if let home, home.isVisible {
            home.makeKeyAndOrderFront(nil)
            return
        }
        home = show({ HomeView(window: $0) }, title: "B-Rar", size: NSSize(width: 520, height: 360), resizable: false)
    }

    /// Opens an archive to look inside.
    static func browse(_ archive: URL) {
        home?.close()
        show({ BrowserView(archive: archive, window: $0) }, title: archive.lastPathComponent, size: NSSize(width: 820, height: 540))
    }

    static func chooseAndBrowse() {
        let panel = NSOpenPanel()
        panel.message = "Choose an archive to open"
        panel.allowsMultipleSelection = true
        if panel.runModal() == .OK { panel.urls.forEach(browse) }
    }

    /// Runs an extraction, test or new archive in a window of its own.
    static func run(_ action: JobAction) {
        show({ JobView(action: action, window: $0) }, title: action.windowTitle, size: NSSize(width: 520, height: 260), resizable: false)
    }
}

/// The window a view is in, so the view can close it or put sheets on it.
@MainActor
final class WindowHandle {
    weak var window: NSWindow?
    func close() { window?.close() }
}

// Top-level code runs on the main thread.
MainActor.assumeIsolated {
    let app = NSApplication.shared
    let delegate = AppDelegate()
    app.delegate = delegate
    app.setActivationPolicy(.regular)
    app.run()
}
