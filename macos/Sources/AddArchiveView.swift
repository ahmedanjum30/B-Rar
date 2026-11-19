// "Add to archive": the new ZIP's name, compression and password, before compressing.
// The Mac counterpart of the Windows "Add to archive" page.

import AppKit
import SwiftUI

struct AddArchiveView: View {
    let inputs: [URL]
    let window: WindowHandle

    /// The levels the Windows version offers.
    private static let levels: [(String, UInt32)] = [("Store", 0), ("Fastest", 1), ("Normal", 6), ("Best", 9)]

    @State private var path: String
    @State private var level: UInt32 = 6
    @State private var encrypt = false
    @State private var password = ""
    @State private var again = ""

    init(inputs: [URL], window: WindowHandle) {
        self.inputs = inputs
        self.window = window
        _path = State(initialValue: defaultArchiveName(for: inputs).path)
    }

    private var problem: String? {
        if !path.lowercased().hasSuffix(".zip") { return "The archive's name must end in .zip." }
        if encrypt && password.isEmpty { return "Enter a password." }
        if encrypt && password != again { return "The two passwords don't match." }
        return nil
    }

    private var exists: Bool { FileManager.default.fileExists(atPath: path) }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(inputs.count == 1 ? "Add \(inputs[0].lastPathComponent) to a ZIP archive" : "Add \(inputs.count) items to a ZIP archive")
                .font(.title3.weight(.semibold)).lineLimit(1).truncationMode(.middle)
            HStack {
                TextField("Archive", text: $path).textFieldStyle(.roundedBorder)
                Button("Choose…", action: choose)
            }
            Picker("Compression", selection: $level) {
                ForEach(Self.levels, id: \.1) { Text($0.0).tag($0.1) }
            }
            .pickerStyle(.segmented)
            Toggle("Encrypt with a password (AES-256)", isOn: $encrypt)
            if encrypt {
                SecureField("Password", text: $password).textFieldStyle(.roundedBorder)
                SecureField("Type it again", text: $again).textFieldStyle(.roundedBorder)
            }
            if let problem, !(problem.hasPrefix("Enter") && password.isEmpty && again.isEmpty) {
                Text(problem).font(.callout).foregroundStyle(.red)
            } else if exists {
                Text("An archive with this name exists; it will be replaced.").font(.callout).foregroundStyle(.secondary)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { window.close() }.keyboardShortcut(.cancelAction)
                Button("Create", action: create).keyboardShortcut(.defaultAction).disabled(problem != nil)
            }
        }
        .padding(24)
        .frame(width: 480)
        .fixedSize(horizontal: false, vertical: true)
    }

    private func choose() {
        let panel = NSSavePanel()
        panel.allowedContentTypes = [.zip]
        panel.directoryURL = URL(fileURLWithPath: path).deletingLastPathComponent()
        panel.nameFieldStringValue = URL(fileURLWithPath: path).lastPathComponent
        if panel.runModal() == .OK, let url = panel.url { path = url.path }
    }

    private func create() {
        guard problem == nil else { return }
        let archive = URL(fileURLWithPath: (path as NSString).expandingTildeInPath)
        Router.run(.create(inputs: inputs, archive: archive, level: level, password: encrypt ? password : nil, overwrite: exists))
        window.close()
    }
}
