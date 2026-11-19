// The window B-Rar opens with on its own: open an archive, or drop one.

import AppKit
import SwiftUI
import UniformTypeIdentifiers

struct HomeView: View {
    let window: WindowHandle
    @State private var dropping = false

    var body: some View {
        VStack(spacing: 14) {
            Image(nsImage: NSApp.applicationIconImage).resizable().frame(width: 84, height: 84)
            Text("Open an archive to look inside and extract it.").font(.title3)
            Button("Open Archive…") { Router.chooseAndBrowse() }
                .controlSize(.large).keyboardShortcut(.defaultAction)
            Text("Or drop an archive on this window.").foregroundStyle(.secondary)
            Text("In Finder, right-click any archive and choose Quick Actions for B-Rar's menu.")
                .font(.callout).foregroundStyle(.secondary).multilineTextAlignment(.center)
            Spacer().frame(height: 4)
            Text("B-Rar \(Engine.version)").font(.footnote).foregroundStyle(.tertiary)
        }
        .padding(28)
        .frame(width: 520, height: 360)
        .background(dropping ? Color.accentColor.opacity(0.08) : .clear)
        .onDrop(of: [.fileURL], isTargeted: $dropping) { providers in
            for provider in providers {
                _ = provider.loadObject(ofClass: URL.self) { url, _ in
                    guard let url else { return }
                    DispatchQueue.main.async { Router.browse(url) }
                }
            }
            return true
        }
    }
}

enum Format {
    static func bytes(_ count: Double) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(count), countStyle: .file)
    }

    static func files(_ count: UInt64) -> String {
        count == 1 ? "1 file" : "\(count.formatted()) files"
    }

    static func duration(_ seconds: TimeInterval) -> String {
        let s = Int(seconds.rounded())
        return s >= 3600 ? String(format: "%d:%02d:%02d", s / 3600, s / 60 % 60, s % 60) : String(format: "%d:%02d", s / 60, s % 60)
    }
}
