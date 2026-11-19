// An archive's contents, to look through and extract from: folders, search, and the files
// and folders picked. The Mac counterpart of crates/brar-gui/src/browse.rs.

import AppKit
import SwiftUI

/// A file or folder in the archive.
struct Node: Identifiable, Hashable {
    /// The archive path, with "/" and no leading or trailing slash.
    let id: String
    let name: String
    let isDir: Bool
    let size: UInt64
    let packed: UInt64?
    let modified: Date?
    // For sorting: folders first.
    var kindOrder: Int { isDir ? 0 : 1 }
    var sizeOrder: UInt64 { isDir ? 0 : size }
    var modifiedOrder: Date { modified ?? .distantPast }
}

@MainActor
final class BrowserModel: ObservableObject {
    enum Phase {
        case reading
        case password(wrong: Bool)
        case ready
        case failed(String)
    }

    let archive: URL
    @Published var phase: Phase = .reading
    @Published var folder = ""
    @Published var search = ""
    @Published var selection = Set<Node.ID>()
    @Published var sortOrder = [KeyPathComparator(\Node.kindOrder), KeyPathComparator(\Node.name, comparator: .localizedStandard)]
    @Published var status: JobStatus?

    private(set) var password: String?
    /// Folder path -> what's directly in it.
    private var children: [String: [Node]] = [:]
    private var all: [Node] = []
    private var job: Job?
    private var timer: Timer?

    init(archive: URL) { self.archive = archive }

    func read(password: String? = nil) {
        self.password = password
        phase = .reading
        job = Engine.list(archive, password: password)
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.poll() }
        }
    }

    private func poll() {
        guard let job, let status = job.status() else { return }
        self.status = status
        if status.question?.kind == "part" {
            let panel = NSOpenPanel()
            panel.message = "B-Rar needs \((status.question!.path as NSString).lastPathComponent) to go on. Where is it?"
            panel.directoryURL = URL(fileURLWithPath: status.question!.path).deletingLastPathComponent()
            job.answerPart(panel.runModal() == .OK ? panel.url : nil)
            return
        }
        guard status.isFinished else { return }
        timer?.invalidate()
        if status.state == "done", let entries = job.takeEntries() {
            build(entries)
            phase = .ready
        } else if status.errorKind == "password_required" {
            phase = .password(wrong: false)
        } else if status.errorKind == "wrong_password" {
            phase = .password(wrong: true)
        } else {
            phase = .failed(status.error ?? "Something went wrong.")
        }
        self.job = nil
    }

    /// Normalizes names as the engine does, and adds the folders archives often leave out.
    private func build(_ entries: [Entry]) {
        var nodes: [String: Node] = [:]
        func addFolder(_ path: String) {
            guard !path.isEmpty, nodes[path] == nil else { return }
            nodes[path] = Node(id: path, name: (path as NSString).lastPathComponent, isDir: true, size: 0, packed: nil, modified: nil)
            addFolder((path as NSString).deletingLastPathComponent)
        }
        for entry in entries {
            var name = entry.name.replacingOccurrences(of: "\\", with: "/").trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            while name.hasPrefix("./") { name.removeFirst(2) }
            guard !name.isEmpty, name != "." else { continue }
            // A Mac's metadata for another entry, put back on it when extracting: hidden, as in Finder.
            if name == "__MACOSX" || name.hasPrefix("__MACOSX/") || (name as NSString).lastPathComponent.hasPrefix("._") { continue }
            addFolder((name as NSString).deletingLastPathComponent)
            if entry.isDir && nodes[name] != nil { continue }
            nodes[name] = Node(id: name, name: (name as NSString).lastPathComponent, isDir: entry.isDir, size: entry.size,
                               packed: entry.packed, modified: entry.modified.map { Date(timeIntervalSince1970: $0) })
        }
        all = Array(nodes.values)
        children = Dictionary(grouping: all) { ($0.id as NSString).deletingLastPathComponent }
    }

    /// What the table shows: the folder's contents, or what matches the search in it and below.
    var rows: [Node] {
        let nodes: [Node]
        if search.isEmpty {
            nodes = children[folder] ?? []
        } else {
            let prefix = folder.isEmpty ? "" : folder + "/"
            nodes = all.filter { $0.id.hasPrefix(prefix) && $0.name.localizedCaseInsensitiveContains(search) }
        }
        return nodes.sorted(using: sortOrder)
    }

    func open(_ node: Node) {
        if node.isDir {
            folder = node.id
            search = ""
            selection = []
        } else {
            openFile(node)
        }
    }

    func up() {
        folder = (folder as NSString).deletingLastPathComponent
        selection = []
    }

    /// Extracts one file to a temporary folder and opens it with its app.
    private func openFile(_ node: Node) {
        let temp = FileManager.default.temporaryDirectory.appendingPathComponent("B-Rar/\(UUID().uuidString)")
        let base = (node.id as NSString).deletingLastPathComponent
        guard let job = Engine.extract(archive, to: temp, password: password, items: [node.id], base: base, conflicts: 1) else { return }
        Task { @MainActor in
            var status = job.status()
            while status?.isFinished == false {
                try? await Task.sleep(for: .milliseconds(50))
                status = job.status()
            }
            if status?.state == "done" {
                NSWorkspace.shared.open(temp.appendingPathComponent(node.name))
            } else {
                let alert = NSAlert()
                alert.messageText = "Couldn't open \(node.name)"
                alert.informativeText = status?.error ?? ""
                alert.runModal()
            }
        }
    }

    /// The archive paths to extract: the selection, or everything when nothing's selected.
    func picked() -> (items: [String]?, base: String) {
        selection.isEmpty ? (nil, "") : (Array(selection), folder)
    }

    var stats: String {
        let files = all.filter { !$0.isDir }
        let size = files.reduce(0) { $0 + $1.size }
        let packed = files.compactMap(\.packed).reduce(0, +)
        var parts = [Format.files(UInt64(files.count)), "\(all.count - files.count) folders", Format.bytes(Double(size))]
        if packed > 0, size > 0 { parts.append("packed \(Format.bytes(Double(packed))) (\(Int((Double(packed) / Double(size) * 100).rounded()))%)") }
        return parts.joined(separator: "  ·  ")
    }
}

struct BrowserView: View {
    @StateObject private var model: BrowserModel
    let window: WindowHandle
    @State private var password = ""
    @State private var showPassword = false

    init(archive: URL, window: WindowHandle) {
        _model = StateObject(wrappedValue: BrowserModel(archive: archive))
        self.window = window
    }

    var body: some View {
        Group {
            switch model.phase {
            case .reading: reading
            case let .password(wrong): passwordPage(wrong: wrong)
            case .ready: contents
            case let .failed(message): failure(message)
            }
        }
        .frame(minWidth: 560, minHeight: 320)
        .onAppear { model.read() }
    }

    private var reading: some View {
        VStack(spacing: 10) {
            ProgressView()
            Text("Reading \(model.archive.lastPathComponent)…").foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func passwordPage(wrong: Bool) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("\(model.archive.lastPathComponent) is encrypted", systemImage: "lock.fill").font(.title3.weight(.semibold))
            Group {
                if showPassword { TextField("Enter password", text: $password) } else { SecureField("Enter password", text: $password) }
            }
            .textFieldStyle(.roundedBorder)
            .onSubmit { if !password.isEmpty { model.read(password: password) } }
            Toggle("Show password", isOn: $showPassword)
            if wrong { Text("That password is incorrect. Try again.").foregroundStyle(.red).font(.callout) }
            HStack {
                Spacer()
                Button("Close", role: .cancel) { window.close() }.keyboardShortcut(.cancelAction)
                Button("OK") { model.read(password: password) }.keyboardShortcut(.defaultAction).disabled(password.isEmpty)
            }
        }
        .padding(28)
        .frame(maxWidth: 440)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func failure(_ message: String) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Couldn't open the archive", systemImage: "exclamationmark.triangle.fill")
                .font(.title3.weight(.semibold)).symbolRenderingMode(.multicolor)
            Text(message).textSelection(.enabled)
            HStack {
                Spacer()
                Button("Close") { window.close() }.keyboardShortcut(.defaultAction)
            }
        }
        .padding(28)
        .frame(maxWidth: 480)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var contents: some View {
        VStack(spacing: 0) {
            Table(model.rows, selection: $model.selection, sortOrder: $model.sortOrder) {
                TableColumn("Name", value: \.name, comparator: .localizedStandard) { node in
                    Label {
                        Text(model.search.isEmpty ? node.name : node.id).lineLimit(1).truncationMode(.middle)
                    } icon: {
                        Image(nsImage: icon(for: node)).resizable().frame(width: 16, height: 16)
                    }
                }
                .width(min: 200, ideal: 360)
                TableColumn("Size", value: \.sizeOrder) { node in
                    Text(node.isDir ? "" : Format.bytes(Double(node.size))).monospacedDigit().foregroundStyle(.secondary)
                }
                .width(min: 70, ideal: 90)
                TableColumn("Packed", value: \.sizeOrder) { node in
                    Text(node.packed.map { Format.bytes(Double($0)) } ?? "").monospacedDigit().foregroundStyle(.secondary)
                }
                .width(min: 70, ideal: 90)
                TableColumn("Modified", value: \.modifiedOrder) { node in
                    Text(node.modified?.formatted(date: .abbreviated, time: .shortened) ?? "").foregroundStyle(.secondary)
                }
                .width(min: 120, ideal: 160)
            }
            .contextMenu(forSelectionType: Node.ID.self) { ids in
                Button(ids.isEmpty ? "Extract All…" : "Extract…") { extract(ask: true, ids: ids) }
                if let id = ids.first, ids.count == 1, let node = model.rows.first(where: { $0.id == id }) {
                    Button(node.isDir ? "Open Folder" : "Open") { model.open(node) }
                }
            } primaryAction: { ids in
                if let id = ids.first, let node = model.rows.first(where: { $0.id == id }) { model.open(node) }
            }
            Divider()
            Text(model.stats).font(.callout).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 12).padding(.vertical, 6)
        }
        .searchable(text: $model.search, placement: .toolbar, prompt: "Search this folder")
        .toolbar {
            ToolbarItem(placement: .navigation) {
                Button { model.up() } label: { Image(systemName: "chevron.left") }
                    .disabled(model.folder.isEmpty).help("Back to the folder above")
            }
            ToolbarItem(placement: .principal) {
                Text(model.folder.isEmpty ? model.archive.lastPathComponent : model.folder)
                    .lineLimit(1).truncationMode(.head).foregroundStyle(.secondary)
            }
            ToolbarItemGroup(placement: .primaryAction) {
                Menu {
                    Button("Extract to \"\(Engine.suggestedDir(model.archive).lastPathComponent)\"") { extract(to: Engine.suggestedDir(model.archive)) }
                    Button("Extract Here") { extract(to: model.archive.deletingLastPathComponent()) }
                    Divider()
                    Button("Test Archive") { Router.run(.test(archive: model.archive)) }
                } label: {
                    Label(model.selection.isEmpty ? "Extract To…" : "Extract Selected…", systemImage: "arrow.down.doc")
                } primaryAction: {
                    extract(ask: true, ids: model.selection)
                }
                .help("Extract the selected files and folders, or the whole archive")
            }
        }
    }

    private func extract(ask: Bool, ids: Set<Node.ID>) {
        guard let dest = chooseFolder(for: model.archive) else { return }
        let (items, base) = ids.isEmpty ? (nil, "") : (Array(ids), model.folder)
        Router.run(.extract(archive: model.archive, dest: dest, items: items, base: base, password: model.password))
    }

    private func extract(to dest: URL) {
        let (items, base) = model.picked()
        Router.run(.extract(archive: model.archive, dest: dest, items: items, base: base, password: model.password))
    }

    private func icon(for node: Node) -> NSImage {
        node.isDir ? NSWorkspace.shared.icon(for: .folder) : NSWorkspace.shared.icon(for: .init(filenameExtension: (node.name as NSString).pathExtension) ?? .data)
    }
}
